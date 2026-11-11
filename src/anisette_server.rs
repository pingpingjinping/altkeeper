use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::{
    body::Bytes,
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        DefaultBodyLimit, State,
    },
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use futures_util::{Stream, StreamExt};
use omnisette::{
    adi_proxy::{
        ADIProxy, ADIProxyAnisetteProvider, Identifier, AKD_USER_AGENT, CLIENT_INFO_HEADER, DS_ID,
        IDENTIFIER_LENGTH,
    },
    anisette_headers_provider::AnisetteHeadersProvider,
    store_services_core::StoreServicesCoreADIProxy,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::time::{timeout_at, Instant};
use uuid::Uuid;

const MAX_PAYLOAD_BYTES: usize = 16 * 1024;
const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const SESSION_MAX_DURATION: Duration = Duration::from_secs(300);

// Opt-in compatibility probes for Apple authentication. Native CoreADI provisioning
// is unchanged; these values control only the V3 metadata returned to clients.
const LEGACY_MAC_CLIENT_INFO: &str =
    "<MacBookPro13,3> <Mac OS X;10.12.6;16G29> <com.apple.AuthKit/1 (com.apple.akd/1.0)>";
const LEGACY_MAC_USER_AGENT: &str = "akd/1.0 CFNetwork/811.5.4 Darwin/16.7.0";
const WINDOWS_COMPAT_CLIENT_INFO: &str =
    "<PC> <Windows;6.2(0,0);9200> <com.apple.AuthKit/1 (com.apple.akd/1.0)>";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LocalClientProfile {
    Native,
    LegacyMac,
    WindowsCompat,
}

impl LocalClientProfile {
    fn parse(value: &str) -> Result<Self, String> {
        match value.trim() {
            "native" => Ok(Self::Native),
            "legacy-mac" => Ok(Self::LegacyMac),
            "windows-compat" => Ok(Self::WindowsCompat),
            other => Err(format!(
                "Invalid ALTKEEPER_LOCAL_ANISETTE_CLIENT_PROFILE '{other}'. Expected native, legacy-mac or windows-compat."
            )),
        }
    }

    fn from_env() -> Result<Self, String> {
        match std::env::var("ALTKEEPER_LOCAL_ANISETTE_CLIENT_PROFILE") {
            Ok(value) => Self::parse(&value),
            Err(std::env::VarError::NotPresent) => Ok(Self::Native),
            Err(error) => Err(format!("Invalid local Anisette client profile environment: {error}")),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::LegacyMac => "legacy-mac",
            Self::WindowsCompat => "windows-compat",
        }
    }

    fn values(self) -> (&'static str, &'static str) {
        match self {
            Self::Native => (CLIENT_INFO_HEADER, AKD_USER_AGENT),
            Self::LegacyMac => (LEGACY_MAC_CLIENT_INFO, LEGACY_MAC_USER_AGENT),
            Self::WindowsCompat => (WINDOWS_COMPAT_CLIENT_INFO, AKD_USER_AGENT),
        }
    }
}

type Provider = ADIProxyAnisetteProvider<StoreServicesCoreADIProxy<'static>>;

struct Engine {
    libraries: PathBuf,
    provisioning_root: PathBuf,
    auth_profile: LocalClientProfile,
    serial: tokio::sync::Mutex<()>,
    sequence: std::sync::atomic::AtomicU64,
}

struct ProviderSession {
    provider: Provider,
    provisioning_dir: TemporaryProvisioningDir,
}

struct TemporaryProvisioningDir(PathBuf);

impl Drop for TemporaryProvisioningDir {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            if error.kind() != std::io::ErrorKind::NotFound {
                crate::log::line(&format!(
                    "[anisette-local] Could not remove temporary provisioning state: {error}"
                ));
            }
        }
    }
}

impl Engine {
    fn new(libraries: PathBuf, data_dir: &Path, auth_profile: LocalClientProfile) -> Result<Self, String> {
        let architecture = match std::env::consts::ARCH {
            "x86_64" => "x86_64",
            "x86" => "x86",
            "aarch64" => "arm64-v8a",
            "arm" => "armeabi-v7a",
            other => return Err(format!("Unsupported host architecture: {other}")),
        };
        let native_dir = libraries.join("lib").join(architecture);
        for name in ["libstoreservicescore.so", "libCoreADI.so"] {
            if !native_dir.join(name).is_file() {
                return Err(format!(
                    "Missing native Anisette library: {}",
                    native_dir.join(name).display()
                ));
            }
        }

        for name in ["libstoreservicescore.so", "libCoreADI.so"] {
            let raw = std::fs::read(native_dir.join(name)).map_err(|e| e.to_string())?;
            validate_library(&raw, architecture)?;
            let manifest_path = native_dir.join("source.json");
            if manifest_path.exists() {
                let manifest: Value = serde_json::from_slice(&std::fs::read(&manifest_path).map_err(|e| e.to_string())?)
                    .map_err(|e| format!("Invalid native library manifest: {e}"))?;
                if manifest["sha256"][name].as_str() != Some(hex::encode(Sha256::digest(&raw)).as_str()) {
                    return Err(format!("Native library integrity check failed: {name}"));
                }
            }
        }

        let provisioning_root = data_dir.join("state").join("anisette-provisioning");
        std::fs::create_dir_all(&provisioning_root)
            .map_err(|error| format!("Cannot create Anisette state directory: {error}"))?;
        crate::fsperm::restrict_dir(&provisioning_root)
            .map_err(|error| format!("Cannot protect Anisette state directory: {error}"))?;

        Ok(Self {
            libraries,
            provisioning_root,
            auth_profile,
            serial: tokio::sync::Mutex::new(()),
            sequence: std::sync::atomic::AtomicU64::new(0),
        })
    }

    fn provider(
        &self,
        identifier: Identifier,
        adi_pb: Option<&[u8]>,
    ) -> Result<ProviderSession, String> {
        let identity = Uuid::from_bytes(identifier);
        let sequence = self
            .sequence
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let provisioning_dir = TemporaryProvisioningDir(self.provisioning_root.join(format!(
            "{}-{}-{sequence}",
            identity,
            std::process::id()
        )));
        std::fs::create_dir_all(&provisioning_dir.0)
            .map_err(|error| format!("Cannot create provisioning state: {error}"))?;
        crate::fsperm::restrict_dir(&provisioning_dir.0)
            .map_err(|error| format!("Cannot protect provisioning state: {error}"))?;
        if let Some(adi_pb) = adi_pb {
            crate::fsperm::write_private(&provisioning_dir.0.join("adi.pb"), adi_pb)
                .map_err(|error| format!("Cannot save provisioning data: {error}"))?;
        }

        let mut proxy = StoreServicesCoreADIProxy::with_custom_provisioning_path(
            &self.libraries,
            &provisioning_dir.0,
        )
        .map_err(|error| format!("Cannot load native Anisette libraries: {error}"))?;
        proxy
            .set_device_identifier(identity.to_string().to_uppercase())
            .map_err(|error| format!("Cannot configure Anisette device identity: {error}"))?;
        proxy.set_local_user_uuid(hex::encode_upper(Sha256::digest(identifier)));
        let provider = ADIProxyAnisetteProvider::without_identifier(proxy)
            .map_err(|error| format!("Cannot create Anisette provider: {error}"))?;
        Ok(ProviderSession {
            provider,
            provisioning_dir,
        })
    }
}

fn host_arch_dir() -> Result<&'static str, String> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("x86_64"),
        "x86" => Ok("x86"),
        "aarch64" => Ok("arm64-v8a"),
        "arm" => Ok("armeabi-v7a"),
        other => Err(format!("Unsupported host architecture: {other}")),
    }
}

fn has_native_libs(libraries: &Path) -> bool {
    let Ok(arch) = host_arch_dir() else {
        return false;
    };
    let native_dir = libraries.join("lib").join(arch);
    native_dir.join("libstoreservicescore.so").is_file()
        && native_dir.join("libCoreADI.so").is_file()
}

const APPLE_MUSIC_APK: &str = "https://apps.mzstatic.com/content/android-apple-music-apk/applemusic.apk";
const MAX_APK_BYTES: u64 = 256 * 1024 * 1024;
const MAX_LIBRARY_BYTES: u64 = 16 * 1024 * 1024;

fn validate_library(raw: &[u8], arch: &str) -> Result<(), String> {
    let machine = match arch { "x86_64" => 62u16, "arm64-v8a" => 183u16,
        "x86" => 3u16, "armeabi-v7a" => 40u16, _ => return Err("Unsupported architecture".into()) };
    let class = if matches!(arch, "x86_64" | "arm64-v8a") { 2 } else { 1 };
    if raw.len() < 64 || &raw[..4] != b"\x7fELF" || raw[4] != class || raw[5] != 1
        || u16::from_le_bytes([raw[18], raw[19]]) != machine {
        return Err(format!("Invalid native Anisette ELF library for {arch}"));
    }
    Ok(())
}

// Download the complete ZIP to disk with a strict cap. ZipArchive verifies the
// selected entries' CRC and sizes; no unchecked Range offsets or archive paths.
fn download_native_libs(libraries: &Path) -> Result<(), String> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let arch = host_arch_dir()?;
    std::fs::create_dir_all(libraries).map_err(|e| e.to_string())?;
    crate::fsperm::restrict_dir(libraries).map_err(|e| e.to_string())?;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(180))
        .redirect(reqwest::redirect::Policy::none())
        .https_only(true).build().map_err(|e| e.to_string())?;
    let response = client.get(APPLE_MUSIC_APK).send()
        .and_then(|r| r.error_for_status()).map_err(|e| format!("Apple Music APK download failed: {e}"))?;
    if response.content_length().is_some_and(|n| n > MAX_APK_BYTES) {
        return Err("Apple Music APK exceeds download limit".into());
    }
    let mut apk = tempfile::tempfile().map_err(|e| e.to_string())?;
    let copied = std::io::copy(&mut response.take(MAX_APK_BYTES + 1), &mut apk).map_err(|e| e.to_string())?;
    if copied > MAX_APK_BYTES { return Err("Apple Music APK exceeds download limit".into()); }
    apk.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    install_libraries(apk, libraries, arch)
}

fn install_libraries<R: std::io::Read + std::io::Seek>(apk: R, libraries: &Path, arch: &str) -> Result<(), String> {
    use std::io::Read as _;
    let mut zip = zip::ZipArchive::new(apk).map_err(|e| format!("Invalid Apple Music APK: {e}"))?;
    let lib_root = libraries.join("lib");
    std::fs::create_dir_all(&lib_root).map_err(|e| e.to_string())?;
    crate::fsperm::restrict_dir(&lib_root).map_err(|e| e.to_string())?;
    let staging = tempfile::tempdir_in(&lib_root).map_err(|e| e.to_string())?;
    crate::fsperm::restrict_dir(staging.path()).map_err(|e| e.to_string())?;
    let mut manifest = serde_json::Map::new();
    for name in ["libstoreservicescore.so", "libCoreADI.so"] {
        let entry_name = format!("lib/{arch}/{name}");
        let mut entry = zip.by_name(&entry_name).map_err(|e| format!("Missing {entry_name}: {e}"))?;
        if entry.size() > MAX_LIBRARY_BYTES { return Err(format!("Library {name} exceeds size limit")); }
        let mut raw = Vec::new();
        (&mut entry).take(MAX_LIBRARY_BYTES + 1).read_to_end(&mut raw).map_err(|e| format!("Invalid {name}: {e}"))?;
        if raw.len() as u64 > MAX_LIBRARY_BYTES { return Err(format!("Library {name} exceeds size limit")); }
        validate_library(&raw, arch)?;
        manifest.insert(name.into(), json!(hex::encode(Sha256::digest(&raw))));
        crate::fsperm::write_private(&staging.path().join(name), &raw).map_err(|e| e.to_string())?;
    }
    crate::fsperm::write_private(&staging.path().join("source.json"),
        serde_json::to_string_pretty(&json!({"url": APPLE_MUSIC_APK, "architecture": arch, "sha256": manifest})).unwrap().as_bytes())
        .map_err(|e| e.to_string())?;
    let destination = lib_root.join(arch);
    if destination.exists() { return Err(format!("Incomplete library directory {}; remove it explicitly before retrying", destination.display())); }
    std::fs::rename(staging.path(), &destination).map_err(|e| e.to_string())?;
    Ok(())
}

/// Stato del server Anisette locale su 127.0.0.1:8788, mostrato dalla pagina web: chi sceglie
/// "AltKeeper local server" deve sapere se c'è davvero qualcosa in ascolto, e se no perché.
#[derive(Clone, Debug, PartialEq)]
pub enum LocalStatus {
    Stopped,
    Starting,
    Running,
    Unavailable(String),
}

static RUNTIME_ERROR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
static LOCAL_STATUS: std::sync::Mutex<LocalStatus> = std::sync::Mutex::new(LocalStatus::Stopped);

fn set_local_status(status: LocalStatus) {
    *LOCAL_STATUS.lock().unwrap_or_else(|e| e.into_inner()) = status;
}

pub fn local_status() -> LocalStatus {
    LOCAL_STATUS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Il server locale non parte: lo si scrive nel log (altrimenti non se ne accorge nessuno) e
/// lo si mostra nella pagina.
fn local_unavailable(reason: String) {
    crate::log::line(&format!("[anisette-local] Local server not started: {reason}"));
    println!("ALTKEEPER_ANISETTE_ERROR:{reason}");
    set_local_status(LocalStatus::Unavailable(reason));
}

/// Start only after an explicit local selection/test. A failed child is never
/// replaced by an external provider. kill_on_drop stops it with the main runtime.
pub fn ensure_started(data_dir: &Path) -> Result<(), String> {
    let mut status = LOCAL_STATUS.lock().unwrap_or_else(|e| e.into_inner());
    if matches!(*status, LocalStatus::Starting | LocalStatus::Running) { return Ok(()); }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = data_dir.canonicalize().map_err(|e| e.to_string())?;
    *status = LocalStatus::Starting;
    drop(status);
    tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let result = async {
            let mut child = tokio::process::Command::new(exe)
                .arg("__anisette-local-worker").arg("--supervised").current_dir(dir)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::inherit())
                .kill_on_drop(true).spawn().map_err(|e| e.to_string())?;
            let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
            while let Some(line) = lines.next_line().await.map_err(|e| e.to_string())? {
                if let Some(reason) = line.strip_prefix("ALTKEEPER_ANISETTE_RUNTIME_ERROR:") {
                    *RUNTIME_ERROR.lock().unwrap_or_else(|e| e.into_inner()) = Some(reason.to_string());
                } else if line == "ALTKEEPER_ANISETTE_READY" { set_local_status(LocalStatus::Running); }
                else if let Some(reason) = line.strip_prefix("ALTKEEPER_ANISETTE_ERROR:") {
                    local_unavailable(reason.to_string());
                } else { crate::log::line(&format!("[anisette-worker] {line}")); }
            }
            let exit = child.wait().await.map_err(|e| e.to_string())?;
            if matches!(local_status(), LocalStatus::Unavailable(_)) { return Ok(()); }
            Err(format!("Native Anisette worker exited: {exit}"))
        }.await;
        if let Err(error) = result { local_unavailable(error); }
    });
    Ok(())
}

pub fn require_ready(data_dir: &Path) -> Result<(), String> {
    let _ = data_dir;
    match local_status() {
        LocalStatus::Running => Ok(()),
        LocalStatus::Unavailable(reason) => Err(format!("Local Anisette unavailable: {reason}. Select the external server to revert.")),
        _ => Err("Local Anisette is starting; check its dashboard status and retry. No external fallback was used.".into()),
    }
}

pub fn status_json() -> Value {
    match local_status() {
        LocalStatus::Stopped => json!({"state": "stopped"}),
        LocalStatus::Starting => json!({"state": "starting"}),
        LocalStatus::Running => json!({"state": "running", "error": RUNTIME_ERROR.lock().unwrap_or_else(|e| e.into_inner()).clone()}),
        LocalStatus::Unavailable(reason) => json!({"state": "unavailable", "error": reason}),
    }
}

pub async fn serve_default(data_dir: PathBuf) {
    let bind: std::net::SocketAddr = "127.0.0.1:8788".parse().unwrap();
    let libraries = data_dir.join("anisette-libraries");
    let mut download_error = None;
    if !has_native_libs(&libraries) {
        let libs_clone = libraries.clone();
        crate::log::line("[anisette-local] Downloading Apple Music APK for native Anisette libraries (up to 256 MiB)...");
        download_error = match tokio::task::spawn_blocking(move || download_native_libs(&libs_clone)).await {
            Ok(result) => result.err(),
            Err(e) => Some(e.to_string()),
        };
    }
    if !has_native_libs(&libraries) {
        let arch = host_arch_dir().unwrap_or("<arch>");
        let place = libraries.join("lib").join(arch);
        return local_unavailable(match download_error {
            Some(e) => format!(
                "the native Anisette libraries could not be downloaded ({e}). Put libstoreservicescore.so and libCoreADI.so in {}",
                place.display()
            ),
            None => format!("libstoreservicescore.so and libCoreADI.so are missing from {}", place.display()),
        });
    }
    if let Err(error) = serve(bind, libraries, data_dir).await {
        local_unavailable(error);
    }
}

pub async fn serve(
    bind: std::net::SocketAddr,
    libraries: PathBuf,
    data_dir: PathBuf,
) -> Result<(), String> {
    if !bind.ip().is_loopback() { return Err("Local Anisette must bind to loopback".into()); }
    // Risponde anche a http://127.0.0.1:8788 (la voce "AltKeeper local server") se ascolta lì
    // o su tutte le interfacce, sulla porta 8788.
    let local = bind.port() == 8788 && (bind.ip().is_loopback() || bind.ip().is_unspecified());
    let auth_profile = LocalClientProfile::from_env()?;
    let engine = Arc::new(Engine::new(libraries, &data_dir, auth_profile)?);
    // Load and validate the native engine before reporting readiness.
    let probe = engine.provider([0; IDENTIFIER_LENGTH], None)?;
    drop(probe);
    let router = Router::new()
        .route("/v3/client_info", get(client_info))
        .route(
            "/v3/get_headers",
            post(get_headers).layer(DefaultBodyLimit::max(MAX_PAYLOAD_BYTES)),
        )
        .route("/v3/provisioning_session", get(provisioning_session))
        .with_state(engine);
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|error| format!("Cannot listen for local Anisette on {bind}: {error}"))?;
    set_local_status(if local {
        LocalStatus::Running
    } else {
        LocalStatus::Unavailable(format!(
            "the local server listens on {bind}: use http://{bind} as a custom URL"
        ))
    });
    println!("ALTKEEPER_ANISETTE_READY");
    crate::log::line(&format!("[anisette-local] V3 client profile: {} (native CoreADI provisioning unchanged)", auth_profile.name()));
    crate::log::line(&format!(
        "[anisette-local] V3 server listening at http://{bind}. This protocol has no authentication or TLS; restrict network access."
    ));
    axum::serve(listener, router)
        .await
        .map_err(|error| format!("Local Anisette server failed: {error}"))
}

fn client_info_body(profile: LocalClientProfile) -> Value {
    let (client_info, user_agent) = profile.values();
    json!({
        "client_info": client_info,
        "user_agent": user_agent,
    })
}

async fn client_info(State(engine): State<Arc<Engine>>) -> Json<Value> {
    Json(client_info_body(engine.auth_profile))
}

#[derive(Deserialize)]
struct HeadersRequest {
    identifier: String,
    adi_pb: String,
}

async fn get_headers(State(engine): State<Arc<Engine>>, body: Bytes) -> (StatusCode, Json<Value>) {
    let request: HeadersRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => {
            return get_headers_error(
                StatusCode::BAD_REQUEST,
                &format!("Invalid request: {error}"),
            )
        }
    };
    let identifier = match BASE64.decode(request.identifier) {
        Ok(bytes) if bytes.len() == IDENTIFIER_LENGTH => {
            let mut identifier = [0; IDENTIFIER_LENGTH];
            identifier.copy_from_slice(&bytes);
            identifier
        }
        Ok(_) => return get_headers_error(StatusCode::BAD_REQUEST, "Invalid identifier length."),
        Err(error) => {
            return get_headers_error(
                StatusCode::BAD_REQUEST,
                &format!("Invalid identifier: {error}"),
            )
        }
    };
    let adi_pb = match BASE64.decode(request.adi_pb) {
        Ok(bytes) if !bytes.is_empty() && bytes.len() <= MAX_PAYLOAD_BYTES => bytes,
        Ok(_) => {
            return get_headers_error(StatusCode::BAD_REQUEST, "Invalid provisioning data size.")
        }
        Err(error) => {
            return get_headers_error(
                StatusCode::BAD_REQUEST,
                &format!("Invalid provisioning data: {error}"),
            )
        }
    };

    let _serial = match engine.serial.try_lock() {
        Ok(guard) => guard,
        Err(_) => {
            return get_headers_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "The local Anisette engine is busy.",
            )
        }
    };
    let result = async {
        let mut provider = engine.provider(identifier, Some(&adi_pb))?;
        let headers = generate_headers(&mut provider)
            .await
            .map_err(|error| format!("Could not generate Anisette headers: {error}"))?;
        Ok::<HashMap<String, String>, String>(headers)
    }
    .await;

    match result {
        Ok(headers) => {
            let mut response = serde_json::Map::new();
            response.insert("result".into(), json!("Headers"));
            for name in ["X-Apple-I-MD-M", "X-Apple-I-MD", "X-Apple-I-MD-RINFO"] {
                let Some(value) = headers.get(name) else {
                    return get_headers_error(
                        StatusCode::BAD_GATEWAY,
                        "The native engine omitted a required Anisette header.",
                    );
                };
                response.insert(name.into(), json!(value));
            }
            (StatusCode::OK, Json(Value::Object(response)))
        }
        Err(error) => {
            report_runtime_error(&error);
            crate::log::line(&format!(
                "[anisette-local] Header generation failed: {error}"
            ));
            get_headers_error(StatusCode::BAD_GATEWAY, "The local Anisette engine failed.")
        }
    }
}

async fn generate_headers(
    provider: &mut ProviderSession,
) -> Result<HashMap<String, String>, String> {
    let headers = provider
        .provider
        .get_anisette_headers(true)
        .await
        .map_err(|error| error.to_string())?;
    Ok(provider.provider.normalize_headers(headers))
}

fn report_runtime_error(error: &str) {
    println!("ALTKEEPER_ANISETTE_RUNTIME_ERROR:{error}");
    crate::log::line(&format!("[anisette-local] Native runtime error: {error}"));
}

fn get_headers_error(status: StatusCode, message: &str) -> (StatusCode, Json<Value>) {
    (
        status,
        Json(json!({
            "result": "GetHeadersError",
            "message": message,
        })),
    )
}

async fn provisioning_session(
    State(engine): State<Arc<Engine>>,
    upgrade: WebSocketUpgrade,
) -> impl IntoResponse {
    upgrade
        .max_message_size(MAX_PAYLOAD_BYTES)
        .max_frame_size(MAX_PAYLOAD_BYTES)
        .on_upgrade(move |socket| handle_provisioning(socket, engine))
}

#[derive(Deserialize)]
struct IdentifierRequest {
    identifier: String,
}

#[derive(Deserialize)]
struct StartProvisioningRequest {
    spim: String,
}

#[derive(Deserialize)]
struct EndProvisioningRequest {
    ptm: String,
    tk: String,
}

#[derive(Clone, Copy)]
enum SessionStep {
    Identifier,
    Start,
    End,
}

async fn receive_provisioning_message<S: Stream + Unpin>(
    socket: &mut S,
    session_deadline: Instant,
) -> Result<Option<S::Item>, ()> {
    let now = Instant::now();
    // A ready message must not bypass an already-expired session deadline.
    if now >= session_deadline {
        return Err(());
    }
    let receive_deadline = session_deadline.min(now + SESSION_IDLE_TIMEOUT);
    timeout_at(receive_deadline, socket.next())
        .await
        .map_err(|_| ())
}

async fn handle_provisioning(mut socket: WebSocket, engine: Arc<Engine>) {
    let session_deadline = Instant::now() + SESSION_MAX_DURATION;
    let _serial = match engine.serial.try_lock() {
        Ok(guard) => guard,
        Err(_) => {
            let _ = send_ws(
                &mut socket,
                json!({
                    "result": "StartProvisioningError",
                    "message": "The local Anisette engine is busy.",
                }),
            )
            .await;
            return;
        }
    };
    let mut provider: Option<ProviderSession> = None;
    let mut session = 0;
    let mut step = SessionStep::Identifier;

    if send_ws(&mut socket, json!({"result": "GiveIdentifier"}))
        .await
        .is_err()
    {
        return;
    }

    loop {
        let message = match receive_provisioning_message(&mut socket, session_deadline).await {
            Ok(Some(Ok(message))) => message,
            Ok(Some(Err(error))) => {
                crate::log::line(&format!(
                    "[anisette-local] Provisioning WebSocket failed: {error}"
                ));
                break;
            }
            Ok(None) => break,
            Err(_) => {
                let _ = send_ws(&mut socket, json!({"result": "Timeout"})).await;
                break;
            }
        };

        match message {
            Message::Text(text) => {
                let outcome = match step {
                    SessionStep::Identifier => {
                        let input = match serde_json::from_str::<IdentifierRequest>(text.as_str()) {
                            Ok(input) => input,
                            Err(error) => {
                                send_ws_error(&mut socket, "InvalidIdentifier", error.to_string())
                                    .await;
                                break;
                            }
                        };
                        let identifier = match BASE64.decode(input.identifier) {
                            Ok(bytes) if bytes.len() == IDENTIFIER_LENGTH => {
                                let mut identifier = [0; IDENTIFIER_LENGTH];
                                identifier.copy_from_slice(&bytes);
                                identifier
                            }
                            Ok(_) => {
                                send_ws_error(
                                    &mut socket,
                                    "InvalidIdentifier",
                                    "Identifier has an invalid length.".into(),
                                )
                                .await;
                                break;
                            }
                            Err(error) => {
                                send_ws_error(&mut socket, "InvalidIdentifier", error.to_string())
                                    .await;
                                break;
                            }
                        };
                        match engine.provider(identifier, None) {
                            Ok(instance) => {
                                provider = Some(instance);
                                step = SessionStep::Start;
                                json!({"result": "GiveStartProvisioningData"})
                            }
                            Err(error) => {
                                crate::log::line(&format!(
                                    "[anisette-local] Provider initialization failed: {error}"
                                ));
                                send_ws_error(
                                    &mut socket,
                                    "InvalidIdentifier",
                                    "Could not initialize the local Anisette engine.".into(),
                                )
                                .await;
                                break;
                            }
                        }
                    }
                    SessionStep::Start => {
                        let input =
                            match serde_json::from_str::<StartProvisioningRequest>(text.as_str()) {
                                Ok(input) => input,
                                Err(error) => {
                                    send_ws_error(
                                        &mut socket,
                                        "StartProvisioningError",
                                        error.to_string(),
                                    )
                                    .await;
                                    break;
                                }
                            };
                        let spim = match decode_payload(&input.spim) {
                            Ok(bytes) => bytes,
                            Err(error) => {
                                send_ws_error(&mut socket, "StartProvisioningError", error).await;
                                break;
                            }
                        };
                        let Some(provider) = provider.as_mut() else {
                            send_ws_error(
                                &mut socket,
                                "StartProvisioningError",
                                "Provisioning state was lost.".into(),
                            )
                            .await;
                            break;
                        };
                        match provider
                            .provider
                            .adi_proxy()
                            .start_provisioning(DS_ID, &spim)
                        {
                            Ok(result) => {
                                session = result.session;
                                step = SessionStep::End;
                                json!({
                                    "result": "GiveEndProvisioningData",
                                    "cpim": BASE64.encode(result.cpim),
                                })
                            }
                            Err(error) => {
                                crate::log::line(&format!(
                                    "[anisette-local] Provisioning start failed: {error}"
                                ));
                                send_ws_error(
                                    &mut socket,
                                    "StartProvisioningError",
                                    "Native provisioning could not start.".into(),
                                )
                                .await;
                                break;
                            }
                        }
                    }
                    SessionStep::End => {
                        let input =
                            match serde_json::from_str::<EndProvisioningRequest>(text.as_str()) {
                                Ok(input) => input,
                                Err(error) => {
                                    send_ws_error(
                                        &mut socket,
                                        "EndProvisioningError",
                                        error.to_string(),
                                    )
                                    .await;
                                    break;
                                }
                            };
                        let ptm = match decode_payload(&input.ptm) {
                            Ok(bytes) => bytes,
                            Err(error) => {
                                send_ws_error(&mut socket, "EndProvisioningError", error).await;
                                break;
                            }
                        };
                        let tk = match decode_payload(&input.tk) {
                            Ok(bytes) => bytes,
                            Err(error) => {
                                send_ws_error(&mut socket, "EndProvisioningError", error).await;
                                break;
                            }
                        };
                        let Some(provider) = provider.as_mut() else {
                            send_ws_error(
                                &mut socket,
                                "EndProvisioningError",
                                "Provisioning state was lost.".into(),
                            )
                            .await;
                            break;
                        };
                        if let Err(error) = provider
                            .provider
                            .adi_proxy()
                            .end_provisioning(session, &ptm, &tk)
                        {
                            report_runtime_error(&error.to_string());
                            crate::log::line(&format!(
                                "[anisette-local] Provisioning completion failed: {error}"
                            ));
                            send_ws_error(
                                &mut socket,
                                "EndProvisioningError",
                                "Native provisioning could not finish.".into(),
                            )
                            .await;
                            break;
                        }
                        match std::fs::read(provider.provisioning_dir.0.join("adi.pb")) {
                            Ok(adi_pb) => {
                                session = 0;
                                json!({
                                    "result": "ProvisioningSuccess",
                                    "adi_pb": BASE64.encode(adi_pb),
                                })
                            }
                            Err(error) => {
                                crate::log::line(&format!(
                                    "[anisette-local] Could not read adi.pb: {error}"
                                ));
                                send_ws_error(
                                    &mut socket,
                                    "EndProvisioningError",
                                    "Could not read the completed provisioning data.".into(),
                                )
                                .await;
                                break;
                            }
                        }
                    }
                };

                if send_ws(&mut socket, outcome).await.is_err() {
                    break;
                }
            }
            Message::Ping(payload) => {
                if socket.send(Message::Pong(payload)).await.is_err() {
                    break;
                }
            }
            Message::Close(_) => break,
            _ => {
                let _ = send_ws(&mut socket, json!({"result": "TextOnly"})).await;
                break;
            }
        }
    }

    if session != 0 {
        if let Some(provider) = provider.as_mut() {
            if let Err(error) = provider
                .provider
                .adi_proxy()
                .destroy_provisioning_session(session)
            {
                crate::log::line(&format!(
                    "[anisette-local] Could not destroy provisioning session: {error}"
                ));
            }
        }
    }
}

fn decode_payload(payload: &str) -> Result<Vec<u8>, String> {
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err("Provisioning payload is too large.".into());
    }
    BASE64
        .decode(payload)
        .map_err(|error| format!("Invalid base64 provisioning payload: {error}"))
}

async fn send_ws(socket: &mut WebSocket, value: Value) -> Result<(), String> {
    let text = serde_json::to_string(&value).map_err(|error| error.to_string())?;
    socket
        .send(Message::Text(text.into()))
        .await
        .map_err(|error| error.to_string())
}

async fn send_ws_error(socket: &mut WebSocket, result: &str, message: String) {
    if let Err(error) = send_ws(
        socket,
        json!({
            "result": result,
            "message": message,
        }),
    )
    .await
    {
        crate::log::line(&format!(
            "[anisette-local] Could not send provisioning error: {error}"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn provisioning_preserves_idle_timeout() {
        let start = Instant::now();
        let mut socket = futures_util::stream::pending::<Message>();
        assert!(
            receive_provisioning_message(&mut socket, start + SESSION_MAX_DURATION)
                .await
                .is_err()
        );
        assert_eq!(Instant::now() - start, SESSION_IDLE_TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn provisioning_activity_cannot_extend_session_deadline() {
        let start = Instant::now();
        let deadline = start + SESSION_MAX_DURATION;
        let mut socket = futures_util::stream::iter([
            Message::Ping(Bytes::new()),
            Message::Text("{}".into()),
            Message::Ping(Bytes::new()),
        ]);
        for _ in 0..2 {
            tokio::time::advance(SESSION_IDLE_TIMEOUT - Duration::from_secs(1)).await;
            assert!(receive_provisioning_message(&mut socket, deadline)
                .await
                .unwrap()
                .is_some());
        }
        let mut idle_socket = futures_util::stream::pending::<Message>();
        assert!(receive_provisioning_message(&mut idle_socket, deadline)
            .await
            .is_err());
        assert_eq!(Instant::now(), deadline);
        // Even a queued Ping is rejected once the total duration has elapsed.
        assert!(receive_provisioning_message(&mut socket, deadline)
            .await
            .is_err());
    }

    #[test]
    fn client_info_matches_v3_shape() {
        for profile in [LocalClientProfile::Native, LocalClientProfile::LegacyMac, LocalClientProfile::WindowsCompat] {
            let value = client_info_body(profile);
            assert!(value["client_info"].as_str().is_some_and(|s| !s.is_empty()));
            assert!(value["user_agent"].as_str().is_some_and(|s| !s.is_empty()));
        }
    }

    #[test]
    fn client_profiles_are_explicit_and_native_remains_unchanged() {
        assert_eq!(LocalClientProfile::parse("native").unwrap(), LocalClientProfile::Native);
        assert_eq!(LocalClientProfile::parse("legacy-mac").unwrap(), LocalClientProfile::LegacyMac);
        assert_eq!(LocalClientProfile::parse("windows-compat").unwrap(), LocalClientProfile::WindowsCompat);
        assert!(LocalClientProfile::parse("").is_err());
        assert!(LocalClientProfile::parse("other").is_err());
        let native = client_info_body(LocalClientProfile::Native);
        assert_eq!(native["client_info"].as_str(), Some(CLIENT_INFO_HEADER));
        assert_eq!(native["user_agent"].as_str(), Some(AKD_USER_AGENT));
        let mac = client_info_body(LocalClientProfile::LegacyMac);
        assert!(mac["client_info"].as_str().unwrap().contains("MacBookPro13,3"));
        let windows = client_info_body(LocalClientProfile::WindowsCompat);
        assert!(windows["client_info"].as_str().unwrap().contains("<PC> <Windows;"));
    }

    #[test]
    fn rejects_oversized_provisioning_payloads() {
        assert!(decode_payload(&"A".repeat(MAX_PAYLOAD_BYTES + 1)).is_err());
    }
}

#[cfg(test)]
mod download_tests {
    use super::*;
    use std::io::{Cursor, Write};
    fn elf(arch: &str) -> Vec<u8> {
        let mut bytes = vec![0u8; 64];
        bytes[..4].copy_from_slice(b"\x7fELF"); bytes[4] = 2; bytes[5] = 1;
        bytes[18..20].copy_from_slice(&(if arch == "x86_64" {62u16} else {183u16}).to_le_bytes());
        bytes
    }
    fn apk(arch: &str, second: bool, wrong_arch: bool) -> Cursor<Vec<u8>> {
        let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for name in ["libstoreservicescore.so", "libCoreADI.so"] {
            if !second && name == "libCoreADI.so" { continue; }
            archive.start_file(format!("lib/{arch}/{name}"), zip::write::SimpleFileOptions::default()).unwrap();
            archive.write_all(&elf(if wrong_arch {"arm64-v8a"} else {arch})).unwrap();
        }
        archive.finish().unwrap()
    }
    #[test]
    fn library_pair_is_validated_and_installed_atomically_for_both_targets() {
        for arch in ["x86_64", "arm64-v8a"] {
            let dir = tempfile::tempdir().unwrap();
            install_libraries(apk(arch, true, false), dir.path(), arch).unwrap();
            let native = dir.path().join("lib").join(arch);
            assert!(native.join("libCoreADI.so").is_file());
            let manifest: Value = serde_json::from_slice(&std::fs::read(native.join("source.json")).unwrap()).unwrap();
            assert_eq!(manifest["architecture"], arch);
            assert_eq!(manifest["sha256"]["libCoreADI.so"], hex::encode(Sha256::digest(elf(arch))));
        }
    }
    #[test]
    fn partial_and_wrong_arch_downloads_do_not_install_a_library_pair() {
        for (second, wrong) in [(false, false), (true, true)] {
            let dir = tempfile::tempdir().unwrap();
            assert!(install_libraries(apk("x86_64", second, wrong), dir.path(), "x86_64").is_err());
            assert!(!dir.path().join("lib/x86_64").exists());
        }
        assert!(validate_library(b"not an ELF", "x86_64").is_err());
    }
    #[tokio::test]
    async fn rejects_external_bind_before_loading_native_libraries() {
        let dir = tempfile::tempdir().unwrap();
        assert!(serve("0.0.0.0:8788".parse().unwrap(), dir.path().to_path_buf(), dir.path().to_path_buf()).await.unwrap_err().contains("loopback"));
    }
}

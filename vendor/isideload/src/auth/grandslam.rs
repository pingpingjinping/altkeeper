use super::middleware::WasmProxyMiddleware;
use plist::Dictionary;
use plist_macro::plist_to_xml_string;
use plist_macro::pretty_print_dictionary;
#[cfg(not(feature = "wasm"))]
use reqwest::Certificate;
use reqwest::{
    ClientBuilder,
    header::{HeaderMap, HeaderValue},
};
use reqwest_middleware::ClientBuilder as MwClientBuilder;
use rootcause::prelude::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use tracing::debug;
use tracing::warn;

#[cfg(not(feature = "wasm"))]
use crate::sideload::cert_identity::APPLE_ROOT;
use crate::{SideloadError, anisette::AnisetteClientInfo, util::plist::PlistDataExtract};

// Apple's published G3 root for the Android MidService TLS chain.
// https://www.apple.com/certificateauthority/AppleRootCA-G3.cer
#[cfg(not(feature = "wasm"))]
const APPLE_ROOT_G3: &[u8] = include_bytes!("../assets/AppleRootCA-G3.cer");

const URL_BAG: &str = "https://gsa.apple.com/grandslam/GsService2/lookup";
const GSD_SIGN_IN_URL: &str = "https://gsd.apple.com/grandslam/GsService2/";
const PLIST_MEDIA_TYPE: &str = "text/x-xml-plist";
const MAC_CLIENT_INFO: &str =
    "<Mac15,7> <macOS;27.0;26A5378j> <com.apple.AuthKit/1 (com.apple.akd/1.0)>";

/// Con ALTKEEPER_DEBUG (o la vecchia ALTREFRESH_DEBUG) impostata, stampa su stderr come risponde Apple a ogni richiesta:
/// stato, versione HTTP, intestazioni e, se è un errore, il corpo. Mai il corpo di una richiesta
/// (ha la prova della password e i dati anisette) né quello di una risposta riuscita.
fn gs_debug() -> bool {
    std::env::var_os("ALTKEEPER_DEBUG").is_some() || std::env::var_os("ALTREFRESH_DEBUG").is_some()
}

/// Numero d'ordine delle richieste a GrandSlam, per riconoscerle nel debug.
static GS_SEQ: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone)]
pub struct GrandSlam {
    pub client: reqwest_middleware::ClientWithMiddleware,
    pub client_info: AnisetteClientInfo,
    url_bag: Dictionary,
    local_android_login: Option<AnisetteClientInfo>,
}

impl GrandSlam {
    /// Create a new GrandSlam instance
    ///
    /// # Arguments
    /// - `client`: The reqwest client to use for requests
    pub async fn new(
        client_info: AnisetteClientInfo,
        debug: bool,
        proxy_url: Option<String>,
    ) -> Result<Self, Report> {
        Self::new_with_auth_mode(client_info, debug, proxy_url, false).await
    }

    pub(crate) async fn new_with_auth_mode(
        client_info: AnisetteClientInfo,
        debug: bool,
        proxy_url: Option<String>,
        local_android: bool,
    ) -> Result<Self, Report> {
        let client = Self::build_reqwest_client_for_auth(debug, proxy_url, local_android)
            .context("Failed to build HTTP client")?;
        let base_headers = Self::base_headers(&client_info, false, local_android)?;
        let url_bag = Self::fetch_url_bag_for_auth(&client, base_headers, local_android).await?;
        Ok(Self {
            client,
            local_android_login: local_android.then(|| client_info.clone()),
            client_info,
            url_bag,
        })
    }

    pub(crate) fn is_local_android(&self) -> bool {
        self.local_android_login.is_some()
    }

    // GrandSlam expects a Mac client when issuing the Xcode token even when the
    // password exchange used Android ADI. Reuse the HTTP client and URL bag.
    pub(crate) fn developer_client(self: &Arc<Self>) -> Arc<Self> {
        if self.is_local_android() {
            let mut client = self.as_ref().clone();
            client.client_info.client_info = MAC_CLIENT_INFO.to_owned();
            Arc::new(client)
        } else {
            self.clone()
        }
    }

    pub(crate) fn provisioning_client(self: &Arc<Self>) -> Arc<Self> {
        if let Some(login) = &self.local_android_login
            && self.client_info.client_info != login.client_info
        {
            let mut client = self.as_ref().clone();
            client.client_info = login.clone();
            return Arc::new(client);
        }
        self.clone()
    }

    /// Fetch the URL bag from GrandSlam using the legacy Mac lookup identity.
    pub async fn fetch_url_bag(
        client: &reqwest_middleware::ClientWithMiddleware,
        base_headers: HeaderMap,
    ) -> Result<Dictionary, Report> {
        Self::fetch_url_bag_for_auth(client, base_headers, false).await
    }

    fn url_bag_request(
        client: &reqwest_middleware::ClientWithMiddleware,
        mut headers: HeaderMap,
        local_android: bool,
    ) -> reqwest_middleware::RequestBuilder {
        // Apple can reject the first Android SRP request after a Mac URL lookup
        // with HTTP 503. Keep the sign-in identity for the local Android flow.
        if !local_android {
            headers.insert("X-Mme-Client-Info", HeaderValue::from_static(MAC_CLIENT_INFO));
        }
        client.get(URL_BAG).headers(headers)
    }

    async fn fetch_url_bag_for_auth(
        client: &reqwest_middleware::ClientWithMiddleware,
        base_headers: HeaderMap,
        local_android: bool,
    ) -> Result<Dictionary, Report> {
        debug!("Fetching URL bag from GrandSlam");
        let n = GS_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
        let started = Instant::now();
        let resp = Self::url_bag_request(client, base_headers, local_android)
            .send()
            .await
            .context("Failed to fetch URL Bag")?;
        if gs_debug() {
            eprintln!(
                "[gs #{n}] GET {URL_BAG} -> {} {:?} remote={:?} in {:?}",
                resp.status(),
                resp.version(),
                resp.remote_addr(),
                started.elapsed()
            );
        }
        let resp = resp
            .text()
            .await
            .context("Failed to read URL Bag response text")?;

        let dict: Dictionary =
            plist::from_bytes(resp.as_bytes()).context("Failed to parse URL Bag plist")?;
        let urls = dict
            .get("urls")
            .and_then(|v| v.as_dictionary())
            .cloned()
            .ok_or_else(|| report!("URL Bag plist missing 'urls' dictionary"))?;

        Ok(urls)
    }

    pub fn get_url(&self, key: &str) -> Result<String, Report> {
        let mut url = self
            .url_bag
            .get_string(key)
            .context("Unable to find key in URL bag")?;
        // Apparition PR #17 reports that the slash clears an HTML 403 on this route.
        if self.is_local_android()
            && key == "gsService"
            && url == GSD_SIGN_IN_URL.trim_end_matches('/')
        {
            url.push('/');
        }
        Ok(url)
    }

    pub fn get(&self, url: &str) -> Result<reqwest_middleware::RequestBuilder, Report> {
        let builder = self
            .client
            .get(url)
            .headers(self.request_headers(false)?);

        Ok(builder)
    }

    pub fn get_sms(&self, url: &str) -> Result<reqwest_middleware::RequestBuilder, Report> {
        let builder = self
            .client
            .get(url)
            .headers(self.request_headers(true)?);

        Ok(builder)
    }

    pub fn put_sms(&self, url: &str) -> Result<reqwest_middleware::RequestBuilder, Report> {
        let builder = self
            .client
            .put(url)
            .headers(self.request_headers(true)?);

        Ok(builder)
    }

    pub fn post(&self, url: &str) -> Result<reqwest_middleware::RequestBuilder, Report> {
        let mut headers = self.request_headers(false)?;
        if self.is_local_android() && url == GSD_SIGN_IN_URL {
            headers.insert(
                reqwest::header::CONTENT_TYPE,
                HeaderValue::from_static("text/xml; charset=UTF-8"),
            );
        }
        Ok(self.client.post(url).headers(headers))
    }

    pub(crate) fn post_developer_plist(
        &self,
        url: &str,
    ) -> Result<reqwest_middleware::RequestBuilder, Report> {
        let mut headers = self.request_headers(false)?;
        // Developer Services expects the plist media type after GrandSlam sign-in.
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            HeaderValue::from_static(PLIST_MEDIA_TYPE),
        );
        Ok(self.client.post(url).headers(headers))
    }

    pub fn post_sms(&self, url: &str) -> Result<reqwest_middleware::RequestBuilder, Report> {
        let builder = self
            .client
            .post(url)
            .headers(self.request_headers(true)?);

        Ok(builder)
    }

    pub fn patch(&self, url: &str) -> Result<reqwest_middleware::RequestBuilder, Report> {
        let builder = self
            .client
            .patch(url)
            .headers(self.request_headers(false)?);

        Ok(builder)
    }

    pub async fn plist_request(
        &self,
        url: &str,
        body: &Dictionary,
        additional_headers: Option<HeaderMap>,
        retry_limit: Option<u32>,
    ) -> Result<Dictionary, Report> {
        let debug_on = gs_debug();
        let n = GS_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
        let started = Instant::now();
        let request_xml = plist_to_xml_string(body);
        if debug_on {
            let extra: Vec<String> = additional_headers
                .iter()
                .flat_map(|h| h.iter().map(|(k, v)| format!("{k}: {v:?}")))
                .collect();
            eprintln!(
                "[gs #{n}] POST {url} body={} B client-info={:?} ua={:?} extra-headers={extra:?}",
                request_xml.len(),
                self.client_info.client_info,
                self.client_info.user_agent
            );
        }
        let sent = self
            .post(url)?
            .headers(
                additional_headers
                    .clone()
                    .unwrap_or_else(reqwest::header::HeaderMap::new),
            )
            .body(request_xml)
            .send()
            .await;
        if debug_on
            && let Err(e) = &sent
        {
            eprintln!("[gs #{n}] send error after {:?}: {e:?}", started.elapsed());
        }
        let result = sent.context("Failed to send grandslam request")?;

        if let Some(retries) = retry_limit
            && result.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
        {
            if retries > 0 {
                warn!(
                    "Received 429 Too Many Requests, retrying request ({} retries left)",
                    retries
                );
                return Box::pin(self.plist_request(
                    url,
                    body,
                    additional_headers,
                    Some(retries - 1),
                ))
                .await;
            } else {
                bail!("Received 429 Too Many Requests, no retries left");
            }
        }

        if debug_on {
            eprintln!(
                "[gs #{n}] -> {} {:?} remote={:?} in {:?}",
                result.status(),
                result.version(),
                result.remote_addr(),
                started.elapsed()
            );
            for (k, v) in result.headers() {
                eprintln!("[gs #{n}]   {k}: {v:?}");
            }
        }

        if let Some(e) = result.error_for_status_ref().err() {
            if debug_on {
                let text = result.text().await.unwrap_or_default();
                let head: String = text.chars().take(800).collect();
                eprintln!("[gs #{n}] error body ({} B): {head}", text.len());
            }
            Err::<(), _>(e).context("Received error response from grandslam")?;
            unreachable!("error_for_status_ref gave an error");
        }
        let resp = result
            .text()
            .await
            .context("Failed to read grandslam response as text")?;

        let dict: Dictionary = plist::from_bytes(resp.as_bytes())
            .context("Failed to parse grandslam response plist")
            .attach_with(|| resp.clone())?;

        let response_plist = dict
            .get("Response")
            .and_then(|v| v.as_dictionary())
            .cloned()
            .ok_or_else(|| {
                report!("grandslam response missing 'Response'")
                    .attach(pretty_print_dictionary(&dict))
            })?;

        if debug_on {
            // Solo "Status" (codice e messaggio d'errore di Apple): il resto ha token e chiavi.
            eprintln!(
                "[gs #{n}] ok, {} B, Status={:?}",
                resp.len(),
                response_plist.get("Status")
            );
        }

        Ok(response_plist)
    }

    fn request_headers(&self, sms: bool) -> Result<HeaderMap, Report> {
        Self::base_headers(&self.client_info, sms, self.is_local_android())
    }

    fn base_headers(
        client_info: &AnisetteClientInfo,
        sms: bool,
        local_android: bool,
    ) -> Result<HeaderMap, Report> {
        let mut headers = HeaderMap::new();
        if !sms {
            // Apple edge rules have treated legacy plist MIME types differently on
            // the provisioning and sign-in hosts. Both accept XML sent as text/xml.
            let content_type = if local_android {
                "text/xml"
            } else {
                PLIST_MEDIA_TYPE
            };
            headers.insert("Content-Type", HeaderValue::from_static(content_type));
            headers.insert("Accept", HeaderValue::from_static(PLIST_MEDIA_TYPE));
        } else {
            headers.insert("Content-Type", HeaderValue::from_static("application/json"));
            headers.insert("Accept", HeaderValue::from_static("application/json"));
        }
        headers.insert(
            "X-Mme-Client-Info",
            HeaderValue::from_str(&client_info.client_info)?,
        );
        headers.insert(
            "User-Agent",
            HeaderValue::from_str(&client_info.user_agent)?,
        );
        headers.insert(
            "X-Xcode-Version",
            HeaderValue::from_static("27.0 (27A5218g)"),
        );
        headers.insert(
            "X-Apple-App-Info",
            HeaderValue::from_static("com.apple.gs.xcode.auth"),
        );
        headers.insert(
            reqwest::header::CONNECTION,
            HeaderValue::from_static("close"),
        );

        Ok(headers)
    }

    /// Build a reqwest client with the Apple root certificate
    ///
    /// # Arguments
    /// - `debug`: DANGER, If true, accept invalid certificates and enable verbose connection logging
    /// # Errors
    /// Returns an error if the reqwest client cannot be built
    pub fn build_reqwest_client(
        debug: bool,
        proxy_url: Option<String>,
    ) -> Result<reqwest_middleware::ClientWithMiddleware, Report> {
        Self::build_reqwest_client_for_auth(debug, proxy_url, false)
    }

    fn build_reqwest_client_for_auth(
        #[cfg(not(feature = "wasm"))] debug: bool,
        #[cfg(feature = "wasm")] _debug: bool,
        proxy_url: Option<String>,
        #[cfg(not(feature = "wasm"))] local_android: bool,
        #[cfg(feature = "wasm")] _local_android: bool,
    ) -> Result<reqwest_middleware::ClientWithMiddleware, Report> {
        #[cfg(not(feature = "wasm"))]
        let cert = Certificate::from_der(APPLE_ROOT)?;
        // Il bordo di Apple lascia passare solo ~2 richieste per connessione e rifiuta il resto
        // (AltSign: sessione nuova e una sola connessione per ogni richiesta). Quindi: HTTP/1.1,
        // niente riuso e "Connection: close" su tutte.
        #[cfg(not(feature = "wasm"))]
        let mut close = HeaderMap::new();
        #[cfg(not(feature = "wasm"))]
        close.insert("Connection", HeaderValue::from_static("close"));
        #[cfg(not(feature = "wasm"))]
        let client = ClientBuilder::new()
            .add_root_certificate(cert)
            .http1_only()
            .http1_title_case_headers()
            .pool_max_idle_per_host(0)
            .default_headers(close)
            .danger_accept_invalid_certs(debug)
            .connection_verbose(debug);
        #[cfg(not(feature = "wasm"))]
        let client = if local_android {
            client.add_root_certificate(Certificate::from_der(APPLE_ROOT_G3)?)
        } else {
            client
        };
        // Su macOS parla con Apple con il TLS di sistema (Security.framework), come i client Apple.
        #[cfg(all(not(feature = "wasm"), target_os = "macos"))]
        let client = client.tls_backend_native();
        #[cfg(not(feature = "wasm"))]
        let client = client.build()?;
        #[cfg(feature = "wasm")]
        let client = ClientBuilder::new().build()?;

        let builder = MwClientBuilder::new(client);
        let builder = if let Some(proxy_url) = proxy_url {
            builder.with(WasmProxyMiddleware::new(proxy_url))
        } else {
            builder
        };
        Ok(builder.build())
    }
}

pub trait GrandSlamErrorChecker {
    fn check_grandslam_error(self) -> Result<Dictionary, Report<SideloadError>>;
}

impl GrandSlamErrorChecker for Dictionary {
    fn check_grandslam_error(self) -> Result<Self, Report<SideloadError>> {
        let result = match self.get("Status") {
            Some(plist::Value::Dictionary(d)) => d,
            _ => &self,
        };

        if result.get_signed_integer("ec").unwrap_or(0) != 0 {
            bail!(SideloadError::AuthWithMessage(
                result.get_signed_integer("ec").unwrap_or(-1),
                result.get_str("em").unwrap_or("Unknown error").to_string(),
            ))
        }

        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anisette::remote_v3::LOCAL_ANDROID_CLIENT_INFO;

    fn client(info: &str, local_android: bool) -> Arc<GrandSlam> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client_info = AnisetteClientInfo {
            client_info: info.to_owned(),
            user_agent: "akd/1.0 CFNetwork/808.1.4".to_owned(),
        };
        let mut url_bag = Dictionary::new();
        url_bag.insert(
            "gsService".to_owned(),
            GSD_SIGN_IN_URL.trim_end_matches('/').into(),
        );
        Arc::new(GrandSlam {
            client: GrandSlam::build_reqwest_client_for_auth(false, None, local_android)
                .unwrap(),
            local_android_login: local_android.then(|| client_info.clone()),
            client_info,
            url_bag,
        })
    }

    #[test]
    fn local_android_uses_android_for_sign_in_and_provisioning_and_mac_for_xcode() {
        let login = client(LOCAL_ANDROID_CLIENT_INFO, true);
        let developer = login.developer_client();
        let provision = developer.provisioning_client();
        let sign_in_url = login.get_url("gsService").unwrap();
        let token_url = developer.get_url("gsService").unwrap();
        assert_eq!(sign_in_url, GSD_SIGN_IN_URL);
        assert_eq!(token_url, GSD_SIGN_IN_URL);
        let cases = [
            (
                login.as_ref(),
                sign_in_url.as_str(),
                LOCAL_ANDROID_CLIENT_INFO,
                "text/xml; charset=UTF-8",
            ),
            (
                developer.as_ref(),
                token_url.as_str(),
                MAC_CLIENT_INFO,
                "text/xml; charset=UTF-8",
            ),
            (
                provision.as_ref(),
                "https://gsd.apple.com/grandslam/MidService/start",
                LOCAL_ANDROID_CLIENT_INFO,
                "text/xml",
            ),
        ];
        for (client, url, expected_claim, expected_content_type) in cases {
            let request = client.post(url).unwrap().build().unwrap();
            assert_eq!(request.url().as_str(), url);
            assert_eq!(request.headers()["x-mme-client-info"], expected_claim);
            assert_eq!(request.headers()["content-type"], expected_content_type);
            assert_eq!(request.headers()["accept"], "text/x-xml-plist");
        }

        let sms = login
            .put_sms("https://gsa.apple.com/")
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(sms.headers()["x-mme-client-info"], LOCAL_ANDROID_CLIENT_INFO);
        assert_eq!(sms.headers()["content-type"], "application/json");
    }

    #[test]
    fn developer_plist_posts_use_the_legacy_media_type_with_either_provider() {
        use crate::dev::device_type::{DeveloperDeviceType::Any, dev_url};

        let url = dev_url("listTeams", Any);
        let external_claim = "<MacBookPro13,3> <Mac OS X;10.12.6;16G29>";
        let local = client(LOCAL_ANDROID_CLIENT_INFO, true).developer_client();
        let external = client(external_claim, false).developer_client();
        for (developer, expected_claim) in [(local, MAC_CLIENT_INFO), (external, external_claim)] {
            let request = developer.post_developer_plist(&url).unwrap().build().unwrap();
            assert_eq!(request.method(), reqwest::Method::POST);
            assert_eq!(request.url().as_str(), url);
            assert_eq!(request.headers()["content-type"], PLIST_MEDIA_TYPE);
            assert_eq!(request.headers().get_all("content-type").iter().count(), 1);
            assert_eq!(request.headers()["accept"], PLIST_MEDIA_TYPE);
            assert_eq!(request.headers()["x-mme-client-info"], expected_claim);
        }
    }

    #[test]
    fn local_android_uses_the_sign_in_identity_for_url_lookup() {
        let login = client(LOCAL_ANDROID_CLIENT_INFO, true);
        let lookup = GrandSlam::url_bag_request(
            &login.client,
            login.request_headers(false).unwrap(),
            login.is_local_android(),
        )
        .build()
        .unwrap();
        let srp = login
            .post("https://gsa.apple.com/grandslam/GsService2")
            .unwrap()
            .build()
            .unwrap();

        assert_eq!(lookup.url().as_str(), URL_BAG);
        assert_eq!(lookup.method(), reqwest::Method::GET);
        for header in ["x-mme-client-info", "user-agent", "content-type"] {
            assert_eq!(lookup.headers()[header], srp.headers()[header]);
        }
    }

    #[test]
    fn external_provider_keeps_its_claim_and_legacy_transport_for_all_stages() {
        let claim = "<MacBookPro13,3> <Mac OS X;10.12.6;16G29>";
        let login = client(claim, false);
        assert!(Arc::ptr_eq(&login, &login.developer_client()));
        assert!(Arc::ptr_eq(&login, &login.provisioning_client()));
        let sign_in_url = login.get_url("gsService").unwrap();
        assert_eq!(sign_in_url, GSD_SIGN_IN_URL.trim_end_matches('/'));
        let request = login.post(&sign_in_url).unwrap().build().unwrap();
        assert_eq!(request.url().as_str(), sign_in_url);
        assert_eq!(request.headers()["x-mme-client-info"], claim);
        assert_eq!(request.headers()["content-type"], "text/x-xml-plist");
        assert_eq!(request.headers()["accept"], "text/x-xml-plist");

        let lookup = GrandSlam::url_bag_request(
            &login.client,
            login.request_headers(false).unwrap(),
            login.is_local_android(),
        )
        .build()
        .unwrap();
        assert_eq!(lookup.headers()["x-mme-client-info"], MAC_CLIENT_INFO);
        assert_eq!(lookup.headers()["content-type"], "text/x-xml-plist");
    }
}

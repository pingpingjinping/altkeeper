# Optional local Anisette V3

Existing installations keep their effective external server, environment override,
Apple account, pairing, certificates and volume layout. The default stays
`https://ani.jkcoxson.com`; no automatic migration to local mode takes place.

Choose one of the following in the existing Anisette settings:

| Choice | Effective URL |
| --- | --- |
| External Anisette server | https://ani.jkcoxson.com |
| AltKeeper local Anisette server | http://127.0.0.1:8788 |
| Custom URL | Existing user-supplied http(s) V3 server |

Save a choice, wait for local status **running**, then test the connection.
The first local start downloads an Apple Music APK (bounded to 256 MiB; allow disk
space for the temporary APK). A connection test requests only `/v3/client_info`;
it does not verify Apple provisioning or login. First local login can require 2FA.
CLI login/renew commands use the local endpoint provided by a running `serve`
process; keep that service running when using local mode from the CLI. They do not
start a transient native worker of their own.
The environment variable ALTKEEPER_ANISETTE_URL still takes precedence and locks
web changes. Clearing restores the existing default external server.

There is **no authentication-time fallback** to an external server. If local startup
or authentication fails, the job fails visibly. Select external mode and save to
revert. Switching effective URLs preserves the old identity as
`state/anisette_state.stale` and provisions a new identity rather than reusing it
against a different server. Initial upgrade with no effective-URL marker preserves
the existing external identity. Stale identities are not silently restored on revert.

## Runtime and architecture

The official v0.5.6 omnisette engine and V3 protocol are ported, including serialized
native access, bounded request sizes, 120-second provisioning idle timeout and
300-second total session limit. Linux AMD64 uses `lib/x86_64`; ARM64 uses
`lib/arm64-v8a`. Both Docker targets have native build and loader smoke tests in CI.
Windows CI remains enabled. ARM32 and macOS runtime behavior are not verified here.

An explicit local choice or local connection test starts a separate native worker.
External mode alone does not download or start it. The worker remains available if
you revert to external during the same process lifetime; it remains loopback-only.
Native panic/crash does not terminate the dashboard or scheduler. Parent pipe EOF
and runtime shutdown stop a supervised worker. On startup failure, inspect the
status/error and logs; **Test connection** or save local mode again retries startup.
Runtime native errors also appear in the local status. Persistent external mode
starts no worker after a service restart.

The server binds only `127.0.0.1:8788`; a non-loopback bind is rejected. No Docker port
mapping or public ingress is added. It has no TLS or separate authentication: other
processes on the same host can access it. In host-network Docker this also includes
other host processes/containers sharing that network. Keep the host trusted.

## Native download integrity and licensing review

Source: Apple's HTTPS Apple Music Android APK endpoint, listed in
THIRD-PARTY-NOTICES.md. Redirects are rejected. The entire archive is streamed to a
bounded temporary file; ZIP CRC, per-library size, ELF class/endianness/machine and
both required filenames are validated. Only the two exact architecture-specific
entries are read; no archive paths are extracted. Both libraries are staged and
renamed into place together. Downloads are reused, not updated on every restart.
A SHA-256/source manifest is written and checked before subsequent native loading.

**Limit:** Apple's endpoint is mutable and no independently pinned APK hash or APK
signature verification is implemented. HTTPS is the download authenticity boundary;
ZIP CRC and the saved hashes detect corruption/later modification, not compromise
of the initial download/source or of this host. The loader executes native code
with the service user's privileges. Process separation protects service availability
against native crashes, not against malicious native code. Use a restricted service
user/volume permissions as appropriate for your deployment.

Apple libraries are proprietary and are not redistributed in the repository/image.
Choosing local mode downloads them to your volume; their use is subject to applicable
Apple terms. omnisette is MPL-2.0, its license and source are retained.
The pinned android-loader source has no LICENSE file or Cargo license declaration;
its redistribution license is unresolved and must be clarified before publishing
a production binary/image containing it. This is a release review blocker, not a
claim that omnisette's MPL covers the loader. The source PR can be reviewed meanwhile.

## Data and recovery

No account password is sent to the local V3 server. It receives the random identifier
and provisioning blob and generates Anisette headers. Existing account/password and
signing keys remain in their previous files; they are not moved or encrypted anew.
Anisette state is sensitive even though it is not the account password.

- Existing `state/anisette_state` and `.stale`: private client provisioning identity.
- `state/anisette-provisioning/*`: private per-session `adi.pb`, deleted on normal
  session completion. A crash can leave a private temporary directory; after stopping
  the service you may remove this temporary directory, keeping `anisette_state`.
- `anisette-libraries/lib/<arch>`: downloaded executable libraries and source/hash
  manifest. Directories use private permissions, files use 0600 on Unix.
- The worker inherits the service environment; credentials are not placed in command
  arguments. Avoid sharing verbose library/debug logs or the data volume.

If a download fails, the incomplete staging directory is discarded. An existing
partial or corrupt architecture directory is not overwritten automatically: stop
AltKeeper and remove only that `anisette-libraries/lib/<arch>` directory, then retry.
Alternatively provision trusted libraries manually at the same path; files without
a manifest are accepted as explicitly operator-managed libraries after ELF validation.

Before deployment, confirm both native Docker CI runs and resolve the loader license.
Then test explicit local selection, client_info, fresh Apple login/2FA, renewal,
restart with saved local identity, external rollback, network/download failure and
unlocked/locked phone behavior. Existing 06:30 Asia/Seoul schedule, two 30-minute
retries, Discord failures, missing-App-ID option/quota and installed-app fallback
remain unchanged. Keep external selected until these checks are complete.

## Experimental client identity profiles

The upstream AltKeeper built-in V3 service uses `omnisette` and Apple's native
`libCoreADI.so` from the Android Apple Music APK. Its default `/v3/client_info`
values come from `vendor/omnisette/src/adi_proxy.rs`, **not** from the older
standalone `anisette-v3-server` container. A successful V3 check and successful
machine provisioning do not prove Apple account authentication will work.

For **isolated diagnostics only**, the local V3 worker can expose a different
client identity without changing the external server or rebuilding the image:

| `ALTKEEPER_LOCAL_ANISETTE_CLIENT_PROFILE` | V3 identity |
| --- | --- |
| unset or `native` (default) | Original `omnisette` MacBookPro13,2 identity and user agent |
| `legacy-mac` | MacBookPro13,3 / Mac OS X 10.12.6 / akd+CFNetwork pair observed on a working public V3 endpoint |
| `windows-compat` | Experimental Windows AuthKit identity reported as accepted at *initial* login by iPASide; not verified with AltKeeper |

Set the variable on **only a disposable test worker/container** and restart that
worker. The active profile is logged on startup. Invalid values fail startup
explicitly. The V3 response has the normal `client_info` and `user_agent`
keys; no password, account ID or Anisette token is included in the diagnostic.
The default remains unchanged when the variable is absent.

For these three profiles this switches only the advertised V3 client metadata. The underlying Apple Music
Android CoreADI library, its native provisioning request identity, and the
provisioning/OTP algorithms are **not** changed. Consequently this is a diagnostic
experiment, not a validated workaround for Apple's `-80045` or `-80046`:
changing the client string might get past the initial sign-in and still fail at
2FA or at the final token request. Do not retry login repeatedly or enable
automatic fallback. Use a fresh, separate `/data` directory for each profile
when testing: `state/anisette_state` is tied to the provisioning identity and
AltKeeper's existing retirement check tracks the server URL, **not** this
experimental profile setting. Do not change it on the production 06:30 instance.

References: [official AltKeeper local engine](https://github.com/BartolomeoRusso9/altkeeper/blob/main/src/anisette_server.rs),
[omnisette ADI constants](https://github.com/SideStore/apple-private-apis), and
[iPASide changelog (October 2026)](https://github.com/pwnapplehat/iPASide/blob/main/CHANGELOG.md).

## Experimental Android CoreADI sign-in on ARM64 Linux

Select `ALTKEEPER_LOCAL_ANISETTE_CLIENT_PROFILE=coreadi-android` on an isolated
test container with a fresh `/data` directory. Choose the built-in loopback
server in the dashboard. The profile is picked up on worker startup. Its
connection check verifies only that the V3 endpoint responds.

This profile follows [Apparition's Android CoreADI approach][apparition-claim].
It uses the existing CoreADI engine and varies the Apple request by stage:

| Stage | Apple-facing request |
| --- | --- |
| GrandSlam URL lookup | Same Android Apple Music client claim as SRP |
| V3 machine provisioning and GrandSlam SRP sign-in | Android Apple Music client claim |
| Xcode app token and subsequent developer requests | Mac AuthKit client claim |
| Reprovisioning during a developer session | Original Android claim |
| XML request bodies | `Content-Type: text/xml`, with the existing plist response type |
| JSON 2FA requests | Existing JSON media type |

Only the built-in URL and the exact opt-in profile activate this flow. The
default public endpoint, custom URLs and the three existing profiles use their
existing request behavior. The Android profile does not retry 429/503 sign-ins
automatically. Normal Apple-driven 2FA and resync handling remains in place.
Apparition [reports HTTP 503 on the first Android SRP request][apparition-lookup]
when the URL lookup used a Mac claim. The opt-in flow keeps the same identity
across lookup and SRP, while the external flow keeps its existing Mac lookup.

The public [SideStore directory][server-list] classifies `ani.jkcoxson.com` as
Windows ADI even though the endpoint has advertised a Mac client string. That
string describes the HTTP client identity, while the ADI engine computes the
machine ID and one-time password from provisioned state. The `windows-compat`
profile in this repository still executes Android CoreADI. A native Windows ADI
route uses Apple's Windows components (e.g. the [iCloud AOSKit route][windows-adi]);
the [macOS route][apparition-macos] uses AOSKit/AuthKit on a Mac. Either could
serve the Pi from a separate compatible host. This branch does not depend on
running Windows DLLs or macOS frameworks under emulation on the Pi.

Apparition reports successful Android sign-in after changing the claimed
platform and using [the XML media type][apparition-transport]. It also reports
that some fresh identities need additional trusted-device verification before
Xcode token issuance. This branch uses AltKeeper's existing 2FA flow. If Apple
returns `-22411` without presenting the 2FA challenge, that case still needs
verification and may require a targeted follow-up. An Apple `503` or `-80045`
on initial SRP happens before that challenge. The errors alone do not identify
which Apple-side check failed.

GitHub Actions builds the existing image on native AMD64 and ARM64 runners and
starts the loader with this profile to check `/v3/client_info`. The Rust tests
check request headers and scoping without contacting Apple authentication.
Neither check verifies a real Apple login. For the later manual test, use only a
separate test volume and dashboard port, with automatic renewal disabled there.
Record machine provisioning, SRP, 2FA if requested, Xcode token issuance, and
developer team listing from a single controlled attempt. Leave the production
container and its 06:30 Asia/Seoul job on the existing external server.

[apparition-claim]: https://github.com/illixion/Apparition/blob/d10e2fde5b3e7ca767aac0aa07a4c52d49b819fd/core/src/claimed_platform.rs
[apparition-transport]: https://github.com/illixion/Apparition/blob/d10e2fde5b3e7ca767aac0aa07a4c52d49b819fd/core/vendor/isideload/src/auth/grandslam.rs
[apparition-macos]: https://github.com/illixion/Apparition/blob/d10e2fde5b3e7ca767aac0aa07a4c52d49b819fd/core/src/anisette/native.rs
[server-list]: https://github.com/SideStore/anisette-servers/blob/main/servers.json
[windows-adi]: https://github.com/nab138/isideload/issues/27

[apparition-lookup]: https://blog.illixion.com/2026/10/sideloading-error-80046/#fixing-a-tool-that-signs-in

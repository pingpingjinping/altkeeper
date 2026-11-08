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

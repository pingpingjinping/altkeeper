# Local Anisette third-party components

## omnisette — MPL-2.0

`vendor/omnisette` is copied from official AltKeeper v0.5.6 (`50dc10e`), based on
SideStore/apple-private-apis `03beb1aa42991ccdad6214dee77e72282bef461f`.
The MPL-2.0 license is retained at `vendor/omnisette/LICENSE`. The source is provided
in this repository, including the official fork's macOS AOSKit error/async changes
and Windows POSIX path, file mode, O_TRUNC and timestamp changes. This port does not
change those files. MPL file-level obligations continue to apply to that component;
AltKeeper's own license remains MIT.

Sources:
- https://github.com/SideStore/apple-private-apis/tree/03beb1aa42991ccdad6214dee77e72282bef461f/omnisette
- https://github.com/BartolomeoRusso9/altkeeper/tree/50dc10e/vendor/omnisette

## android-loader

The loader is pinned to BartolomeoRusso9/android-loader
`96ed02b2aee91d7854dcb4b75a17e4288f40ab94`, as in official v0.5.6.
It replaces xmas-elf 0.9 with 0.10 (RUSTSEC-2025-0018). The exact Git revision and
transitive registry checksums are recorded in Cargo.lock. Its included sysv64 crate
is available at the pinned source. That source has neither a LICENSE file nor a
Cargo license declaration, so redistribution permission is unresolved; clarify it
before publishing a production binary/image with this component:
https://github.com/BartolomeoRusso9/android-loader/tree/96ed02b2aee91d7854dcb4b75a17e4288f40ab94

## Apple proprietary libraries

`libstoreservicescore.so` and `libCoreADI.so` are proprietary Apple software, extracted
unchanged from Apple's Apple Music Android APK at:
https://apps.mzstatic.com/content/android-apple-music-apk/applemusic.apk

They are not included in this repository or Docker image and are not covered by the
MIT/MPL licenses. This project grants no rights to them. Enabling local mode causes
the operator's running instance to download the APK and extract the two libraries
for its architecture into its existing data volume. The operator must assess the
applicable Apple terms and permission to use these libraries. Select external mode
to avoid downloading or executing them. No Apple ID is needed for downloading.

See docs/LOCAL_ANISETTE.md for provenance, storage and security limitations.

Apple publishes a [software license for Apple Music on Android][apple-music-sla]
that limits the install/use grant to compatible Android devices and restricts
redistribution. Direct download from Apple's CDN provides no additional grant.
This experiment includes no Apple libraries, makes no new distribution of the
loader or Apple libraries, and leaves the release review above open.

The optional Android client claim and per-stage request approach are based on
[Apparition's MIT-licensed implementation][apparition-claim]. The existing
Android engine and native dependencies are unchanged.

[apple-music-sla]: https://www.apple.com/legal/sla/docs/AppleMusic.pdf
[apparition-claim]: https://github.com/illixion/Apparition/blob/d10e2fde5b3e7ca767aac0aa07a4c52d49b819fd/core/src/claimed_platform.rs

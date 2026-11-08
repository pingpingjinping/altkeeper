# AltStore signing compatibility (v0.5.6 selective port)

New AltStore IPA signing uses `com.<TEAM>.com.rileytestut.AltStore` and the same prefix
for `AltWidget`. `ALTBundleIdentifier` retains `com.rileytestut.AltStore`; the app group
retains `group.com.rileytestut.AltStore.<TEAM>`. Both `ALTDeviceID` and the older
`ALTDeviceId` are written with the UDID already registered for signing.
Already signed legacy AltStore IPAs are recognized without adding the team twice.
General IPA / YouTube / LiveContainer signing keeps the existing suffix scheme.

Renewal never renames an installed bundle or migrates its App IDs. A legacy AltStore
host and its widget are grouped for renewal only when the host exactly matches
`com.<current team>.com.rileytestut.AltStore`. Existing installed-app filtering,
missing-App-ID registration opt-in and Apple slot checks remain active.

If an older AltKeeper installation signed AltStore as `com.rileytestut.AltStore.<TEAM>`,
its self refresh can disagree with its widget's prefix. To adopt the corrected format,
the user must explicitly install the original AltStore IPA again. This changes the
install identity and may create a separate app: export any data first; do not remove
the existing app until the replacement is verified. The new host and selected
extensions can each consume an App ID slot. Old App IDs do not immediately disappear
or release their slots when an app is removed; wait for Apple's registration window
if the limit is reached. No reinstall is performed automatically.

On a real iPhone, verify new installation, host + widget self refresh, registered UDID,
app group access and Refresh All before relying on this workflow. No Apple account,
pairing file, certificate, existing profile or volume format is migrated by this PR.

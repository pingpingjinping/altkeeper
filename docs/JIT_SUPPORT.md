# JIT activation (selective v0.5.6 port)

This PR adds the official running-app JIT workflow and AltStore's
`EnableUnsignedCodeExecutionRequest` support. It does not change installation,
signing or automatic renewal. After installation, open the app, use **Refresh
running apps** in the dashboard, select it, and choose **Enable JIT**. Results appear
in the existing operation log. Korean and English dashboard labels are provided.

Requirements: iOS 17 or later through the existing RSD transport, Developer Mode,
and a compatible Developer Disk Image already mounted on the iPhone. This PR does
not download or mount a DDI, bypass platform requirements, or promise support on
an untested iOS version. Legacy transport returns a clear prerequisites error.
The app must be running and allow debugging. Activation lasts for the app's current
process; after it exits/restarts, refresh the list and activate again.

The implementation lists user app processes via DVT, revalidates that the selected
PID is still an application, negotiates GDB no-ack mode, attaches and continues it.
It checks debugger responses and applies explicit timeouts. AltStore requests must
name the connected paired UDID and a valid PID, or an unambiguous running-app name.
All operations use the existing phone lock, saved home/VPN priority and connection
recovery. Web endpoints use the existing PIN and request-header protections and
busy guard. No Apple login, App ID registration, certificate or profile writes occur.
The AltServer endpoint retains its existing LAN trust model: keep it behind the
home LAN/VPN; do not forward its port to the public internet.

The official optional "enable after install" checkbox is intentionally outside this
initial port: use the explicit running-app action after a successful installation.
This preserves the fork's existing installer and lets JIT be reviewed independently
of the AltStore signing PR.

Hardware verification needed: your iOS 26.1 iPhone, mounted matching DDI, intended
JIT-capable app, app exit/relaunch, wrong UDID, process termination, locked/offline
phone, simultaneous renewal and JIT requests. CI only verifies protocol helpers,
input validation, regressions and build compatibility; it cannot prove JIT activation.

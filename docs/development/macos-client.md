# Build and run the macOS client

The application requires macOS 14 or newer and Xcode 16 or newer for Swift 6. The Rust headless host and its CLI must be built and installed separately. The app contains no bundled runtime and does not automatically install or start one.

```sh
cargo build --release --bin everything --locked
xcodebuild -project apps/Apple/Everything.xcodeproj -scheme EverythingMac -configuration Debug -destination 'platform=macOS' CODE_SIGNING_ALLOWED=NO build analyze
xcodebuild -project apps/Apple/Everything.xcodeproj -scheme EverythingMac -configuration Release -destination 'platform=macOS' CODE_SIGNING_ALLOWED=NO build
```

The project deliberately uses no Swift package manifest, third-party app framework or test target. Development signing is disabled by those commands; distribution signing is not configured.

Open Everything, choose Settings, select the physical trusted Rust CLI executable and the existing private state directory, then Connect. The CLI's default state directory is `~/.everything`; the owner socket's complete physical path must fit 100 UTF-8 bytes. The app stores only these connection preferences in UserDefaults. Revisioned This Mac preferences use their own SQLite store in `~/Library/Application Support/Everything/DevicePreferences`. It does not store credentials or open the Host database.

The menu shows host connection/draining state, recent tasks and per-conversation pending permissions. Inspect full scope and arguments before allowing an operation. Settings edit future-task defaults with revision comparison. Existing tasks retain their captured authority. Provider retry, cleanup attestation, credentials, pairing and advanced configuration remain available through the existing owner CLI rather than implicit UI grants.

The Notifications tab works independently of the Host connection. Edit completion/attention preferences and Save; a revision conflict keeps the draft until explicit Reload. Inspect an unresolved original save instead of resubmitting it. Saved preference values and observed macOS authorization are separate. Request permission contextually in the foreground; denial shows supported System Settings instructions. Closing Settings blocks unpresented actions while accepted writes/callbacks retain their original identity. These controls do not schedule notification delivery. See the [consumer contract](../contracts/this-mac-settings.md).

Install/start/stop/login controls use the actual per-user launchd service. Installation is explicit. Quitting the menu bar app drains its own local work and CLI children; the host keeps running. Stopping the service is a separate confirmed action and does not stop a foreground `serve` process.

If an action fails after possible delivery, refresh authoritative state before choosing a new action. Repeat exact action preserves the original command/request identity and payload. The in-memory reminder is lost if the app is forcibly terminated; reopening performs reads and does not automatically resend mutations.

## Available validation

The Rust owner-command bridge passed Rust 1.88 formatting, all-target/all-feature compilation, strict Clippy and release build. A disposable real host/CLI walkthrough resolved an actual shell approval without executing it, cancelled a managed task, preserved exact receipt replay, and rejected changed command decisions, unknown envelope/payload fields and omitted nullable fields. Those observations establish the bridge, not Swift execution.

This implementation environment has no Apple SDK or Xcode. Official Swift 6.0.3 parsed all ten Swift files successfully; genuine Linux Foundation-only Swift 6 type checking passed for JSONValue, OwnerWire and OwnerRecords. Plist/XML and OpenStep project object/reference inspection also passed. These checks are separate from an Apple build. No claim is made that the app currently compiles against Apple frameworks or that its UI/lifetime acceptance criteria have run. Fresh exact-head GitHub Actions and actual macOS SDK build/static analysis are pending for this current-main extraction. Earlier billing failures are historical; they do not establish current CI availability. The independently reviewed CLI child-cleanup correction is included; actual macOS cleanup remains unverified. No required check is bypassed. Deferred automated assertions are tracked in [#32](https://github.com/izyuumi/everything/issues/32).

On a disposable macOS setup, record the actual host PID through the CLI or service status, launch and quit the menu bar app, and confirm the same host still answers. Exercise real task inspection, exact permission allow/deny, settings revision conflict, service controls and cancellation; inspect that no provider credential entered app preferences, request files or logs. A build alone does not verify these behaviors. See [client architecture](../architecture/macos-client.md) and [owner IPC](../contracts/owner-ipc.md).

## Owned CLI cancellation

The app retains each launched CLI Process, its pipes and private request input until the actual child exits and is joined. A request timeout starts cancellation; the supervisor attempts SIGINT immediately, SIGTERM after two seconds and SIGKILL after four seconds, each once and only for its tracked positive CLI PID while that Process is running. It does not look up the Host PID or signal a process group; signal attempts use only the captured CLI PID. Failed nonblocking descriptor setup enters the same cleanup path, and only successfully configured nonblocking descriptors are read. Signal and pipe/input cleanup failures are reported alongside the original cancellation, timeout or command failure.

The timeout bounds the start of cancellation and signal escalation rather than kernel process death. An uninterruptible OS process can leave the actual join and app Quit pending; the supervisor retains its ownership rather than reporting a live child as joined. Foundation Process plus a raw PID does not provide atomic PID pinning. This correction has source inspection and available Swift syntax/Foundation type checks only. Fresh SDK build/analyze and actual macOS cancellation, signal, descriptor-error and quit/Host-PID observations remain pending; the earlier published SDK run does not verify this source.

# Build and run the macOS client

The application requires macOS 14 or newer and Xcode 16 or newer for Swift 6. The Rust headless host and its CLI must be built and installed separately. The app contains no bundled runtime and does not automatically install or start one.

```sh
cargo build --release --bin everything --locked
xcodebuild -project apps/Apple/Everything.xcodeproj -scheme EverythingMac -configuration Debug -destination 'platform=macOS' CODE_SIGNING_ALLOWED=NO build analyze
xcodebuild -project apps/Apple/Everything.xcodeproj -scheme EverythingMac -configuration Release -destination 'platform=macOS' CODE_SIGNING_ALLOWED=NO build
```

The project deliberately uses no Swift package manifest, third-party app framework or test target. Development signing is disabled by those commands; distribution signing is not configured.

Open Everything, choose Settings, select the physical trusted Rust CLI executable and the existing private state directory, then Connect. The CLI's default state directory is `~/.everything`; the owner socket's complete physical path must fit 100 UTF-8 bytes. The app stores only these connection preferences in UserDefaults. Revisioned This Mac preferences use their own SQLite store in `~/Library/Application Support/Everything/DevicePreferences`. It does not store credentials or open the Host database.

The menu shows host connection/draining state, recent tasks and per-conversation pending permissions. Inspect full scope and arguments before allowing an operation. Settings edit future-task defaults with revision comparison. Existing tasks retain their captured authority. Models/Memory/Tool Selection, secure key entry and Devices now have explicit native controls. Provider retry, cleanup attestation and advanced owner operations retain their separate CLI authority; the optional assistant cannot grant them.

The Notifications tab works independently of the Host connection and includes saved app appearance. Edit completion/attention preferences or system/light/dark appearance and Save; a revision conflict keeps the draft until explicit Reload. Inspect an unresolved original save instead of resubmitting it. Saved preference values and observed macOS authorization are separate. Request permission contextually in the foreground; denial shows supported System Settings instructions. Closing Settings blocks unpresented actions while accepted writes/callbacks retain their original identity. These Mac controls do not schedule notification delivery. Saved appearance styles only this app, including inherited native presentations; it does not change the machine theme. See the [consumer contract](../contracts/this-mac-settings.md).

Use the Ask Settings toolbar button to show its optional native inspector beside the controls. Open Host settings through the conventional forms, then explicitly Open Ask Settings to inspect the configured provider, model, recipient and tools. Opening and expansion send no question. Review each question and data boundary before Send; never paste credentials into chat. Use Models or Tool Selection for secure key entry. Authenticated cards reuse native controls and known destinations; literal model text has no action authority. Collapse requests closure of that original child and clears private display without stopping the Host. See [composition and lifetime semantics](../contracts/apple-settings-assistant-composition.md).

Install/start/stop/login controls use the actual per-user launchd service. Installation is explicit. Quitting the menu bar app drains its own local work and CLI children; the host keeps running. Stopping the service is a separate confirmed action and does not stop a foreground `serve` process.

If an action fails after possible delivery, inspect its authoritative original before choosing a new action. The ordinary owner reminder's explicit Repeat preserves its original command/request identity and payload. Native credential submission never repeats the secret; use its original Inspect and keep storage/linking uncertainty separate. The in-memory reminder is lost if the app is forcibly terminated; reopening performs reads and does not automatically resend mutations.

## Repair configuration without Ask Settings

No model or assistant is needed for a valid-but-unusable provider configuration. Connect in General, open Host settings from Models, and read the actual saved revision before editing. Choose the provider/model and official or explicit loopback endpoint, Save, then separately confirm the original protected batch. A missing credential does not authorize fallback. Use the native credential control for that exact saved recipient; initial storage Completed needs its separate Link button, while configured replacement performs no metadata CAS. Inspect original storage/link results after uncertainty instead of entering another key. A syntax-invalid settings file that prevents Host startup is a different recovery problem; these controls do not claim to repair arbitrary invalid files.

Memory exposes the real opt-in and reported dimension choices, including model default. Save once and distinguish Saved from pending rebuild/application, unavailable helper and restart-required. Off preserves the operator's model paths; unknown metadata cannot produce guessed choices. System One is opt-in: use the TypeSafe preset or a valid compatible custom endpoint/model, then confirm the exact recipient change. This configuration consent is separate from later exact payload disclosure through ordinary tool selection. See [embedding limits](../architecture/embeddinggemma2.md) and [System One consent](../contracts/host-selection.md).

A stale revision or changed registry retains your draft: explicitly Reload, review and make a new request only after resolving originals. Inspect is observational; Reconcile examines the original unresolved file intent without rewriting or applying a new backend. Applying/Unknown, a successful transport response or another ready backend is not an Applied receipt. Do not turn a newer query into an old request's changed replay.

The existing CLI can inspect/reconcile without an LLM. These are setup instructions, not executed walkthrough evidence. Use the existing trusted CLI and private state while its ONE Host serves; no second Agent is opened. For a nonsecret settings query:

```sh
umask 077
EV_SETTINGS_REQUESTS="$(mktemp -d)"
printf '%s\n' '{"kind":"query","query":{"kind":"settings"}}' > "$EV_SETTINGS_REQUESTS/settings.json"
everything --state-dir /absolute/private/state owner --request-file "$EV_SETTINGS_REQUESTS/settings.json"
```

For original outcome inspection, put this typed query in another private request file and replace the placeholder with the actual retained UUID:

```json
{"kind":"query","query":{"kind":"settings_operation","request_id":"ORIGINAL_OPERATION_UUID"}}
```

Explicit original file reconciliation uses a distinct transport request ID; it does not replace the original write ID:

```json
{"kind":"operation","request_id":"TRANSPORT_REQUEST_UUID","operation":{"kind":"reconcile_settings","operation_id":"ORIGINAL_OPERATION_UUID"}}
```

Submit either file with the same `owner --request-file` command. Preserve original IDs/candidates across response loss. A permitted full replacement uses `kind: operation`, an explicit `request_id`, and `operation.kind: replace_settings` with the observed `expected_revision` and COMPLETE edited settings object. Preserve every unrelated field/reference; the service performs actual CAS. Raw owner replacement returns HostActionRequired for protected provider/recipient changes: use the native Models/Tool Selection confirmation rather than inventing a consent reference or editing the Host file around the service. [Owner command setup](host.md#owner-inspection-and-configuration) and [receipt recovery](../contracts/settings-owner-routes.md) define the exact route. Credential bytes/native proofs never belong in these files; only the existing private pipe or explicit operator environment-credential route can transfer them.

Notification choices are not OS permission. The foreground permission button observes capability before/after genuine OS interaction; denial yields supported instructions, not repeated prompts or a fabricated grant. Mac choices do not schedule notices. iOS has its separate bounded foreground committed-event attempt path, not APNS/background delivery. Devices requires its own private invitation, full identity comparison, explicit access and separate approval; an invitation or Host pin is not completion on the phone. Close/cancel/Inspect retain their original identity and never imply rollback, remote stop or secure erasure. See [native actions](../architecture/native-settings-actions.md).

## Available validation

The Rust owner-command bridge passed Rust 1.88 formatting, all-target/all-feature compilation, strict Clippy and release build. A disposable real host/CLI walkthrough resolved an actual shell approval without executing it, cancelled a managed task, preserved exact receipt replay, and rejected changed command decisions, unknown envelope/payload fields and omitted nullable fields. Those observations establish the bridge, not Swift execution.

Local source validation uses official Swift syntax parsing and OpenStep project inspection; this environment cannot execute AppKit or Apple UI. Recent exact-head GitHub Actions, including #162, did run supported Xcode 26.6 build/static analysis successfully. The corrected final #163 head `81810ec3` subsequently passed actual Ubuntu run `38943440168` and supported Mac/iOS build/analyze in `38943440172`; all four checkout-bearing jobs used the published tree. Its earlier macro-access failure remains historical evidence. A successful SDK build does not establish actual dialog, navigation, provider, cancellation, accessibility or Host-lifetime behavior. Earlier billing failures are historical. Deferred automated assertions remain [#32](https://github.com/izyuumi/everything/issues/32).

On a disposable macOS setup, record the actual host PID through the CLI or service status, launch and quit the menu bar app, and confirm the same host still answers. Exercise real task inspection, exact permission allow/deny, settings revision conflict, service controls and cancellation; inspect that no provider credential entered app preferences, request files or logs. A build alone does not verify these behaviors. See [client architecture](../architecture/macos-client.md) and [owner IPC](../contracts/owner-ipc.md).

A focused disposable observation for #114 on 2026-10-17 (macOS, Apple Silicon, branch `settings/issue-114` from `9295355`) used `cargo run` with a fresh state directory, no credentials and no provider requests. The settings query returned revision 1 with no model credentials and embeddings disabled. A revision-1 replacement saved revision 2; a different request at the same revision-1 was rejected as a revision conflict; replaying the original request ID returned the same revision-2 receipt, and Inspect returned its applied write. A raw owner provider/model selection without credentials returned `host_action_required` with no write. Enabling embeddings saved revision 3 as `pending_application`, and Inspect reported application `unavailable` with `embedding_boundary_unavailable`, the state the Memory pane renders. After the Host was stopped, the CLI reported the owner socket absent. These observations exercise the Rust owner routes the native controls share; they do not execute the Swift UI, native session, Keychain or notification paths.

With the Host stopped while Settings is open, confirm General labels its cached defaults offline, the Host panes show stale values with Save disabled, and This Mac preferences still save. Restart the Host and confirm the panes re-read without another click. Confirm error and warning rows read as `Error:`/`Warning:` in VoiceOver and that the General pane and device/credential sheets resize without clipping.

Credential replacement controls identify the saved provider/endpoint and show its public reference/storage. Device revocation controls identify the device label and include its exact public UUID in the accessibility label. With multiple same-provider endpoints and duplicate device labels, verify the displayed target, VoiceOver name/value and keyboard focus on a disposable macOS setup before confirming. These contextual labels do not establish secret presence or replace native confirmation; actual accessibility behavior remains unverified.

## Owned CLI cancellation

The app retains each launched CLI Process, its pipes and private request input until the actual child exits and is joined. A request timeout starts cancellation; the supervisor attempts SIGINT immediately, SIGTERM after two seconds and SIGKILL after four seconds, each once and only for its tracked positive CLI PID while that Process is running. It does not look up the Host PID or signal a process group; signal attempts use only the captured CLI PID. Failed nonblocking descriptor setup enters the same cleanup path, and only successfully configured nonblocking descriptors are read. Signal and pipe/input cleanup failures are reported alongside the original cancellation, timeout or command failure.

The timeout bounds the start of cancellation and signal escalation rather than kernel process death. An uninterruptible OS process can leave the actual join and app Quit pending; the supervisor retains its ownership rather than reporting a live child as joined. Foundation Process plus a raw PID does not provide atomic PID pinning. Historical correction evidence includes source inspection and Swift syntax/Foundation checks. Its code is also present in the later integrated #163 source that passed supported SDK build/analyze. Actual macOS cancellation, signal, descriptor-error, real cleanup and quit/Host-PID observations remain pending; compilation does not prove them.

# Hybrid Settings architecture

This is the #118 integration checkpoint at main `47bad21`: conventional macOS controls, secure entry, Devices and optional Ask Settings are composed in the existing app and Host, and the Host answers scoped remote Settings and remote restricted chat for explicitly granted paired devices (#116), consumed by the iOS Connected Host form and Ask Settings pane (#117, main `e3d3d41`). The headless Host remains independent. Start here for component boundaries, then read [the restricted assistant](settings-agent.md), [setup and recovery](../development/macos-client.md) and the detailed [settings contract](../contracts/settings.md). Implemented source and successful SDK compilation do not establish native UI, provider or security acceptance.

## Authorities and current capabilities

Host configuration has one authority: `SettingsRepository` and its private revisioned file. `SettingsService` shares the leased Host Agent, Store, mutation guard, device endpoint and application driver. Native controls and assistant tools reach that same service; neither owns a second Host configuration copy. Device-local SQLite separately owns notification choices and appearance. OS authorization is an observation/action, never a writable preference or inferred grant.

| Capability | macOS native controls | Ask Settings | Current iOS client |
| --- | --- | --- | --- |
| Host Models, Memory and Tool Selection | Typed draft, revision CAS, protected native confirmation | Host-scoped tools and authenticated exact-diff cards | Granted keys only, through the remote Settings row below; no protected-key edits |
| Credentials | Private SecureField; configured replacement or initial storage, then separate metadata link | No secret/card authority; known-section navigation can lead to native controls | No Host credential route |
| Devices | Redacted pages, private invitation, explicit identity/access approval, exact-pin revocation | Opaque Pair/Revoke proposals; sealed handoff to the same ceremony | Existing explicit Replica pairing; no remote Settings administration |
| Appearance and notification preferences | Same local device store, usable without Host/model | No device-store facade adapter | Same strict store bound to actual Replica identity, usable offline |
| Notification OS interaction/delivery | Fresh foreground permission gesture; preferences do not schedule notices | No authorization card or OS authority | Fresh permission gesture; bounded committed-event attempts while active and sync-enabled |
| Optional remote Settings | Local-owner grant set/revoke/inspection | No remote grant or owner pass-through; a granted device can run its own scoped restricted chat on the Host | Connected Host form (granted values, stale cache offline, granted embeddings writes with exact-identity reconcile), Host-only action explanations and an Ask Settings pane over the scoped E2EE query client (#117) |

The metadata contract also describes authority ceilings for future clients. The matrix above describes current integrations: the seven-tool model profile is Host-scoped, and only OpenSection is a native action it can request. General workspace/security/service controls remain separate owner controls. Appearance is a local typed extension, not an extra Host key or eighth model tool.

## Module map

| Boundary | Implemented owner |
| --- | --- |
| Host registration and shared execution | [`src/host/runtime.rs`](../../src/host/runtime.rs), `Composition`, existing Scheduler/Broker/Agent |
| Configuration, facade and history | [`src/host/settings.rs`](../../src/host/settings.rs), [`service.rs`](../../src/host/settings/service.rs), `operations.rs`, `driver.rs`; [service contract](../contracts/settings-service.md) |
| Trusted local/native ingress | `host/owner.rs`, `settings/native.rs`, `authority.rs`; [owner receipts](../contracts/settings-owner-routes.md), [private routes](../contracts/native-settings-routes.md) |
| macOS app ownership | [`HostModel.swift`](../../apps/Apple/macOS/HostModel.swift), `SettingsView`, `SettingsWindowObserver`; one shared client/Store; [composition](../contracts/apple-settings-assistant-composition.md) |
| Native effects and drafts | `NativeSettingsStore/Session/Lifecycle`, `NativeSettingsControls`, credential and Devices leaves; [controls](../contracts/apple-native-settings-controls.md), [credentials](../contracts/apple-native-credential-entry.md), [Devices](../contracts/apple-native-devices.md) |
| This-device authority and OS adapter | `Shared/Settings/DevicePreferenceStore`, `DeviceNotificationActions`; [local authority](../contracts/local-device-preferences.md), [appearance](../contracts/device-appearance.md), [iOS attempts](../contracts/ios-notification-delivery.md) |
| Grant history | `settings/grants.rs`, `storage/settings_grants.rs`; [grant contract](../contracts/settings-grants.md) |
| Remote scoped Settings | `devices/settings_connection.rs`, `devices/settings_ingress*`, `devices/settings_publication.rs`, `devices/settings_access_reply.rs`, `settings/service/paired*.rs`, `settings/authority/paired.rs`, `storage/settings_transport.rs`; [transport](../contracts/settings-access-transport.md), [paired authority](../contracts/paired-settings-authority.md), [remote session](../contracts/settings-remote-session.md) |

Native proof/credential/invitation and private assistant frames use the existing authenticated pipe-only route. The generic request-file owner bridge carries nonsecret queries/operations, including separate credential-reference linking. Public UUIDs, digests and validated DTOs do not create a trusted caller or consent. Existing pairing supplies no default Settings grant.

## Saved, effective and uncertain

A draft retains its original snapshot, registry and revision. A conflict requires explicit reread/review; reconnect and chat do not rebase or resend it. One same-scope batch either passes complete validation or fails. Protected provider/model/recipient changes need a separate fresh native confirmation of that retained batch.

A known file receipt can establish Saved while actual application remains pending, unavailable, failed or restart-required. Embedding dimensions/enabling use the real Memory generation/rebuild path; off preserves operator configuration. Model selection affects subsequent runs, not a captured active provider. System One is opt-in, accepts the existing compatible custom endpoint policy and retains separate later payload consent. Actual descriptors supply dimension choices; cached model metadata is not helper readiness. Lexical fallback and [#72 limits](../contracts/host-composition.md) remain explicit.

Unknown/Applying retains original identity and blocks conflicting fresh work. Inspect reads it; explicit Reconcile observes the original file intent without rewriting or replaying it. File Unknown alone may resolve through that same intent; native terminal uncertainty is not silently promoted. Credential storage Completed is separate from metadata linking and current secret presence. Invitation creation, Host pin approval, acceptance presentation and completion on the Replica are different facts. Saved notification choices never imply OS permission or delivery.

## Evidence and remaining checkpoint

Source, SDK and historical runtime evidence retain their exact identities in [implementation status](../status/implementation.md) and the [Settings](../development/settings-validation.md), [pairing](../development/native-pairing-validation.md) and [restricted-runtime](../development/restricted-settings.md) records. Compilation does not establish native UI, provider, transport or security acceptance.

The [#118 integration walkthrough](../development/settings-validation.md#hybrid-integration-walkthrough-118) drove the real CLI/native pipe, a scripted loopback model and a scratch Replica over a loopback relay on disposable state: no-model bootstrap, protected confirmation, secure entry plus separate link, assistant/control cross-updates, a concurrent stale edit, invitation versus approval, confirmed revocation, restart/replay, pane close and an explicitly granted remote subset and session. Real native dialogs, accessibility/navigation, Keychain, OS notification permission, hosted provider inference and foreground/connection races remain unverified; they need a person at the Mac. The [iOS checkpoint](../development/settings-validation.md#ios-checkpoint-117) records the #117 Simulator walkthrough of the shared Swift client.

| Area | Implemented in source | Observed on disposable state | Still needs |
| --- | --- | --- | --- |
| Host service, CAS, replay, confirmation | Yes (#109) | Yes (#118, #170) | Automation (#32) |
| macOS controls, secure entry, Devices, Ask Settings | Yes (#114/#110/#112/#115) | Rust routes they call, not the Swift UI | Person at a Mac: UI, VoiceOver, Keychain, notification dialogs |
| Restricted seven-tool assistant | Yes (#113/#115) | Scripted loopback model only | Hosted provider inference |
| Remote grant, scoped requests, remote chat | Yes (#116) | Scratch Replica over loopback relay; real Swift client in the Simulator | Physical iPhone run |
| iOS Connected Host form and Ask Settings | Yes (#117) | Shared Swift query client in the Simulator, not the SwiftUI screens | Person at an iPhone: taps, background/scene, notification UI |

Reuse [ADR-0010](../decisions/ADR-0010-hybrid-settings-contract.md), [ADR-0011](../decisions/ADR-0011-restricted-settings-agent.md) and [ADR-0012](../decisions/ADR-0012-scoped-settings-grants.md); automated/security coverage remains [#32](https://github.com/izyuumi/everything/issues/32).

## Remote Settings transport

One pipeline serves the remote request kinds `access_status`, `snapshot`, `changes`, `update`, `host_action` and the session kinds `session_open`, `session_send`, `session_poll` and `session_close` ([transport](../contracts/settings-access-transport.md), [requests](../contracts/settings-access-status.md), [replies](../contracts/settings-access-replies.md)). The Host admits each request durably, then, holding the Host mutation guard, rereads the pin and grant head, checks the request's generation and per-key policy, and answers on the same relay socket. Snapshot and Changes reuse the existing Host snapshot and change hints filtered to readable keys. Update reuses the existing `SettingsService` journal and file writer through a per-request paired caller: exact `request_id` replay, changed-payload conflict, revision CAS and receipts are the same as for local callers, and the journal origin is `PairedDevice`. Credential, pairing, revocation, grant, policy, consent and lifecycle requests return `host_action_required`. There is no second runtime and no OwnerQuery/OwnerOperation forwarding. Reply capacity is separate from ordinary sending ([transport storage](../contracts/settings-transport-storage.md)).

A granted device can also run one remote restricted Settings chat ([remote session](../contracts/settings-remote-session.md)). The owner's grant must carry `ConfiguredProvider` session consent matching the provider captured at Open, and each Send echoes the disclosed binding digest. The chat is an ordinary restricted-runtime child under a paired session parent bound to the device pin and grant revision, not to the relay connection. Reads are filtered to readable keys, updates need writable keys and reread the grant under the Host mutation guard, and action/device tools are refused. Results are polled, with committed text paged, so a turn can outlast one request. Grant changes, device revocation and a lost grant retire the parent.

The iOS client sends all of these kinds through one [query client](../contracts/apple-settings-access-client.md): the original Access pending cell, receive path and metadata replay state, with the generation taken only from the current Access summary. `AppModel` runs a serial driver for refresh, granted writes, host-action explanations and the Ask Settings pane. The older Swift Bind, Snapshot, Inspection and Update codecs do not match the Host and have no caller.

Peer-attributable failures are per-message rejections; only uncertain storage outcomes and broken bookkeeping quarantine Settings receive until restart ([ADR-0013](../decisions/ADR-0013-permanent-store-quarantine.md)). A disposable real-relay walkthrough with a scratch Replica helper is recorded in [Settings validation](../development/settings-validation.md#remote-scoped-settings-walkthrough); no Apple device, UI or security review was performed.

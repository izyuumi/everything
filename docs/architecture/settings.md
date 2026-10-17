# Hybrid Settings architecture

This is the macOS checkpoint at merged `b480a6a`: conventional controls, secure entry, Devices and optional Ask Settings are composed in the existing app and Host. The headless Host remains independent. Start here for component boundaries, then read [the restricted assistant](settings-agent.md), [setup and recovery](../development/macos-client.md) and the detailed [settings contract](../contracts/settings.md). Implemented source and successful SDK compilation do not establish native UI, provider or security acceptance.

## Authorities and current capabilities

Host configuration has one authority: `SettingsRepository` and its private revisioned file. `SettingsService` shares the leased Host Agent, Store, mutation guard, device endpoint and application driver. Native controls and assistant tools reach that same service; neither owns a second Host configuration copy. Device-local SQLite separately owns notification choices and appearance. OS authorization is an observation/action, never a writable preference or inferred grant.

| Capability | macOS native controls | Ask Settings | Current iOS client |
| --- | --- | --- | --- |
| Host Models, Memory and Tool Selection | Typed draft, revision CAS, protected native confirmation | Host-scoped tools and authenticated exact-diff cards | No Host value display or mutation controls |
| Credentials | Private SecureField; configured replacement or initial storage, then separate metadata link | No secret/card authority; known-section navigation can lead to native controls | No Host credential route |
| Devices | Redacted pages, private invitation, explicit identity/access approval, exact-pin revocation | Opaque Pair/Revoke proposals; sealed handoff to the same ceremony | Existing explicit Replica pairing; no remote Settings administration |
| Appearance and notification preferences | Same local device store, usable without Host/model | No device-store facade adapter | Same strict store bound to actual Replica identity, usable offline |
| Notification OS interaction/delivery | Fresh foreground permission gesture; preferences do not schedule notices | No authorization card or OS authority | Fresh permission gesture; bounded committed-event attempts while active and sync-enabled |
| Optional remote Settings | Local-owner grant storage/inspection exists | No remote grant or owner pass-through | Explicit Access status check over the relay, answered by the Host; other remote families remain #116 work |

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
| Remote Access status | `devices/settings_connection.rs`, `devices/settings_ingress*`, `devices/settings_publication.rs`, `settings/service/paired*.rs`, `settings/authority/paired.rs`, `storage/settings_transport.rs`; [Access transport](../contracts/settings-access-transport.md) |

Native proof/credential/invitation and private assistant frames use the existing authenticated pipe-only route. The generic request-file owner bridge carries nonsecret queries/operations, including separate credential-reference linking. Public UUIDs, digests and validated DTOs do not create a trusted caller or consent. Existing pairing supplies no default Settings grant.

## Saved, effective and uncertain

A draft retains its original snapshot, registry and revision. A conflict requires explicit reread/review; reconnect and chat do not rebase or resend it. One same-scope batch either passes complete validation or fails. Protected provider/model/recipient changes need a separate fresh native confirmation of that retained batch.

A known file receipt can establish Saved while actual application remains pending, unavailable, failed or restart-required. Embedding dimensions/enabling use the real Memory generation/rebuild path; off preserves operator configuration. Model selection affects subsequent runs, not a captured active provider. System One is opt-in, accepts the existing compatible custom endpoint policy and retains separate later payload consent. Actual descriptors supply dimension choices; cached model metadata is not helper readiness. Lexical fallback and [#72 limits](../contracts/host-composition.md) remain explicit.

Unknown/Applying retains original identity and blocks conflicting fresh work. Inspect reads it; explicit Reconcile observes the original file intent without rewriting or replaying it. File Unknown alone may resolve through that same intent; native terminal uncertainty is not silently promoted. Credential storage Completed is separate from metadata linking and current secret presence. Invitation creation, Host pin approval, acceptance presentation and completion on the Replica are different facts. Saved notification choices never imply OS permission or delivery.

## Evidence and remaining checkpoint

Source, SDK and historical runtime evidence retain their exact identities in [implementation status](../status/implementation.md) and the [Settings](../development/settings-validation.md), [pairing](../development/native-pairing-validation.md) and [restricted-runtime](../development/restricted-settings.md) records. Compilation does not establish native UI, provider, transport or security acceptance.

Real native dialogs, accessibility/navigation, provider inference, foreground/connection races and quit-with-Host-alive walkthroughs remain unverified. The local iOS preferences/notification path is implemented; remote Host Settings is separate. #118 remains open for manual and remote acceptance. Reuse [ADR-0010](../decisions/ADR-0010-hybrid-settings-contract.md), [ADR-0011](../decisions/ADR-0011-restricted-settings-agent.md) and [ADR-0012](../decisions/ADR-0012-scoped-settings-grants.md); automated/security coverage remains [#32](https://github.com/izyuumi/everything/issues/32).

## Remote Settings transport

One remote family is wired end to end: the read-only Access status query. The iOS Settings screen sends an explicit check through the [Access client](../contracts/apple-settings-access-client.md); the Host [admits, answers and sends the reply](../contracts/settings-access-transport.md) on the same relay socket, using the existing Store, mutation guard and device endpoint. There is no second runtime. Reply capacity is separate from ordinary sending ([transport storage](../contracts/settings-transport-storage.md)).

The Host records every other Settings kind as rejected. Bind, Update and permanent Originals, Snapshot and Original-inspection reads, paired Settings supervision and history, and parent registration are deferred to follow-ups with their callers. Swift clients and codecs for Bind, Snapshot, Inspection and Update exist without a screen control and without a responding Host.

Peer-attributable failures are per-message rejections; only uncertain storage outcomes and broken bookkeeping quarantine Settings receive until restart ([ADR-0013](../decisions/ADR-0013-permanent-store-quarantine.md)). Validation is local compile and static checks; no runtime, device, relay round-trip or security verification has been performed.

# Hybrid Settings architecture

This is the macOS checkpoint at merged `b480a6a`: conventional controls, secure entry, Devices and optional Ask Settings are composed in the existing app and Host. The headless Host remains independent. Start here for component boundaries, then read [the restricted assistant](settings-agent.md), [setup and recovery](../development/macos-client.md) and the detailed [settings contract](../contracts/settings.md). Implemented source and successful SDK compilation do not establish native UI, provider or security acceptance.

## Authorities and current capabilities

Host configuration has one authority: `SettingsRepository` and its private revisioned file. `SettingsService` shares the leased Host Agent, Store, mutation guard, device endpoint and application driver. Native controls and assistant tools reach that same service; neither owns a second Host configuration copy. Device-local SQLite separately owns notification choices and appearance. OS authorization is an observation/action, never a writable preference or inferred grant.

| Capability | macOS native controls | Ask Settings | Current iOS client |
| --- | --- | --- | --- |
| Host Models, Memory and Tool Selection | Typed draft, revision CAS, protected native confirmation | Host-scoped tools and authenticated exact-diff cards | Remote Host Settings unavailable |
| Credentials | Private SecureField; configured replacement or initial storage, then separate metadata link | No secret/card authority; known-section navigation can lead to native controls | No Host credential route |
| Devices | Redacted pages, private invitation, explicit identity/access approval, exact-pin revocation | Opaque Pair/Revoke proposals; sealed handoff to the same ceremony | Existing explicit Replica pairing; no remote Settings administration |
| Appearance and notification preferences | Same local device store, usable without Host/model | No device-store facade adapter | Same strict store bound to actual Replica identity, usable offline |
| Notification OS interaction/delivery | Fresh foreground permission gesture; preferences do not schedule notices | No authorization card or OS authority | Fresh permission gesture; bounded committed-event attempts while active and sync-enabled |
| Optional remote Settings | Local-owner grant storage/inspection exists | No remote grant or owner pass-through | Encrypted private Settings lane and grant consumer remain #116 work |

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
| Grant history without remote ingress | `settings/grants.rs`, `storage/settings_grants.rs`; [grant contract](../contracts/settings-grants.md) |

Native proof/credential/invitation and private assistant frames use the existing authenticated pipe-only route. The generic request-file owner bridge carries nonsecret queries/operations, including separate credential-reference linking. Public UUIDs, digests and validated DTOs do not create a trusted caller or consent. Existing pairing supplies no default Settings grant.

## Saved, effective and uncertain

A draft retains its original snapshot, registry and revision. A conflict requires explicit reread/review; reconnect and chat do not rebase or resend it. One same-scope batch either passes complete validation or fails. Protected provider/model/recipient changes need a separate fresh native confirmation of that retained batch.

A known file receipt can establish Saved while actual application remains pending, unavailable, failed or restart-required. Embedding dimensions/enabling use the real Memory generation/rebuild path; off preserves operator configuration. Model selection affects subsequent runs, not a captured active provider. System One is opt-in, accepts the existing compatible custom endpoint policy and retains separate later payload consent. Actual descriptors supply dimension choices; cached model metadata is not helper readiness. Lexical fallback and [#72 limits](../contracts/host-composition.md) remain explicit.

Unknown/Applying retains original identity and blocks conflicting fresh work. Inspect reads it; explicit Reconcile observes the original file intent without rewriting or replaying it. File Unknown alone may resolve through that same intent; native terminal uncertainty is not silently promoted. Credential storage Completed is separate from metadata linking and current secret presence. Invitation creation, Host pin approval, acceptance presentation and completion on the Replica are different facts. Saved notification choices never imply OS permission or delivery.

## Evidence and remaining checkpoint

PR [#163](https://github.com/izyuumi/everything/pull/163) merged the final inspector composition after exact-head Ubuntu fmt/strict Clippy, both real Apple build/analyze jobs and compatibility passed. Its failed first macro-access build is retained; only corrected `81810ec3` has the final pass. PR [#164](https://github.com/izyuumi/everything/pull/164) adds the bounded grant reader with Ubuntu/compatibility success and an actual irrelevant-path SDK skip, not another SDK execution. [Settings](../development/settings-validation.md), [pairing](../development/native-pairing-validation.md) and [restricted-runtime](../development/restricted-settings.md) records retain their own historical artifact identities and limits.

Real native dialogs, accessibility/navigation, provider inference, foreground/connection races and quit-with-Host-alive walkthroughs remain unverified. The local iOS preferences/notification path is implemented; remote Host Settings is separate. #118 remains open for manual and remote acceptance. Reuse [ADR-0010](../decisions/ADR-0010-hybrid-settings-contract.md), [ADR-0011](../decisions/ADR-0011-restricted-settings-agent.md) and [ADR-0012](../decisions/ADR-0012-scoped-settings-grants.md); automated/security coverage remains [#32](https://github.com/izyuumi/everything/issues/32).

## Unmerged scoped transport source

Draft [#172](https://github.com/izyuumi/everything/pull/172) adds internal authenticated receipt ownership and genuine joined Bind, Update and Access events to the existing runtime. Work retains complete private sources, fixed original clocks and uncertainty; the native supervisor transfers real paired results on its existing channel. The single guarded Main publisher qualifies, seals, consumes its exact reservation, rechecks current authority and observes one commit. The checked Store connection is permanently quarantined after observed rollback failure. These internal source boundaries do not activate public transport.

On the Apple side, the identity actor supplies typed scoped sealing and opaque authenticated proofs. Replica Service owns one actual-socket scope, pending Access original and receive ticket across the existing nested handler. Classification authenticates once, checks original frame size and returns a sealed proof. Genuine Fresh Access qualification retains that actual attempt and separates encrypted request correlation from query identity. Its synchronous token checks establish RAM association; the private adopter performs the final asynchronous Store recheck before same-turn consume/project/cache assignment. The finite cache uses a weak actual scope and fixed original wall/monotonic bounds clipped by packet/grant expiry. View-state reading checks the same cache entry around Store waits. Public045 compiled qualification on iOS; the newer display source awaits SDK CI. The live Service continues private refusal; no Settings Send, Bind requester, live adopter or UI consumer is installed.

The single registered worker and original JoinSet now support real Bind/Fresh/Historical/Access publication. Access derives its actual embedded capability; Main rejects a pointer mismatch and uses the genuine qualifier/sealer plus common consume/comparison/final-current/one-COMMIT helper. Original, Bind and Access share the same complete current persisted-pin check without acquiring another Store lock. After a true join, Work installs whole queued custody before fallible settlement, moves the real BUILD slot into its witness before disarming, and retains all original private/live charges and parent/creator ownership. The bounded registry still has no retirement consumer. Protected-history/current queued validation, counted sending and cleanup remain incomplete, so no complete protocol drain or accepted private activation follows. See [implementation status](../status/implementation.md), [publication](../contracts/settings-reply-publication.md), [Access receipt](../contracts/settings-access-receipts.md) and [Apple wire](../contracts/apple-remote-settings-wire.md).

Scoped remote source also still requires actual Host setting-value snapshots, fresh permanent-Original inspection across retired connections, a genuine paired restricted Agent child/profile and ChatOpen/Send/Close owners, scoped hints/results and host-required outcomes. Native restricted Open is currently native-only. AccessStatus is a grant observation, not a settings snapshot. The existing iOS local controls do not implement Connected Host forms or chat. These groups follow the same current grants, immutable IDs, owned runtime and private publication boundaries; none is supplied by forwarding Owner IPC or native assistant DTOs.

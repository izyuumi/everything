# Hybrid settings contract

Issue [#108](https://github.com/izyuumi/everything/issues/108) defines version-one metadata and data contracts in `host::settings::contract`. The existing [SettingsRepository](../architecture/host-settings.md) remains the only host configuration authority. The code here supplies typed descriptors and shape/bounds validation. The command facade, confirmation/action lifecycle, restricted runtime, native controls and scoped transport are implementation work in #109/#110/#112–#118; those behaviors are not registered or implemented by this contract.

## Scopes and authority

Every snapshot, patch, result and action targets exactly one `SettingsScope`: a particular Host identity, a particular device's preferences, or that device's observed OS state. Host revisions come from the existing repository. Device preference revisions belong to that device's own store. OS observations are read-only, with a local observation revision/time. Cross-scope edits are separate operations and receipts; no file/Keychain/job/device transaction is implied.

Requests contain no caller-provided owner flag, principal or grant. #109 must derive an opaque caller context from the authenticated ingress and actual immutable Settings session. Same-user authentication alone does not elevate a model call to an owner operation. #116 must derive paired identity from the verified envelope and intersect an explicit live settings key/session grant with the restricted profile. Existing paired devices receive no new grant through migration. Every call and pending apply rechecks the current actor, session, device grant, key ceiling and target identity.

| Surface | Restricted settings agent | Local native owner | Paired settings client |
| --- | --- | --- | --- |
| Allowlisted ordinary preferences | Only within origin grants and exact scope | Same validated facade | Explicit per-key grant and current connection |
| Generation selection or System One recipient/configuration | Request exact native confirmation | Trusted confirmation of exact diff/recipient | `host_action_required` for new recipient consent |
| Notification preference | Requesting device only | This foreground device | Requesting foreground device only |
| OS notification authorization | Read status; request native action | OS/user interaction | Local requesting-device action only |
| Credential values and writes | No values; request secure entry | Ephemeral secure owner route only | `host_action_required` |
| Pairing invitation/final approval/grants/revocation | Opaque request/status only | Existing explicit ceremony/confirmation | `host_action_required` |
| Workspace/security/execution policy, shell/MCP startup, lifecycle/recovery | No editable key or pass-through; known owner section navigation only | Existing separate authorized controls | No generic settings route |

Descriptor mutability is a ceiling, never a permission or proof of availability. A validated payload or public result DTO cannot create authority. Observed OS state never becomes a device/host grant.

## Registry and values

`SettingKey` is closed; unknown keys and unknown fields fail deserialization. `SettingRegistry::from_active_embedder` returns eight descriptors with a value shape, bounds/choices, default, scope, sensitivity, agent mutability and apply behavior:

| Key | Scope | Default | Agent ceiling | Apply contract |
| --- | --- | --- | --- | --- |
| `models.selection` | Host | No selected model | Native confirmation | Subsequent run; active provider pinned |
| `tools.selection` | Host | Local | Native confirmation | Subsequent selection; disabling stops new external classification |
| `memory.embeddings.enabled` | Host | False | Allowlisted preference with real feature checks | Stop new work on disable; pending apply remains visible |
| `memory.embeddings.dimensions` | Host | Unset | Allowlisted preference with actual model choices | Explicit rebuild/application state |
| `memory.embeddings.model` | Host | Unavailable until reported | Read-only | Observed active model identity |
| `notifications.completion` | This device | False | Allowlisted preference | Local preference only |
| `notifications.attention` | This device | False | Allowlisted preference | Local preference only |
| `notifications.authorization` | Observed OS | Unavailable until queried | Read-only | Actual OS query |

Model selection contains only the existing provider kind, validated official/canonical loopback endpoint and opaque model ID, bounded to 256 bytes. It changes no instructions, execution limits, credential binding or workspace. Validation reuses the existing conversation configuration checks without constructing a provider or sending a request. An absent model is valid: native settings must remain usable.

Tool selection is a closed local/System One preference. An enabled selection contains the exact generic compatible endpoint and bounded opaque model ID and uses the existing System One configuration validator; TypeSafe remains an existing preset, never an exclusive provider. Credential references are managed separately by the native action/credential service. Configuration or confirmation of a configuration change does not approve a later task/payload disclosure: the existing exact [System One consent](host-selection.md) still applies.

Dimension choices come only from `active_embedder.profile().spec().supported_dimensions()`. No constructor loads artifacts, initializes a backend or reads vectors. With no active Embedder the choices are `None`, meaning metadata unavailable, rather than guessed choices or an empty supported list. Desired opt-in and an optional desired dimension remain representable in snapshots/patches without loading anything. #109 must validate supported dimensions against actual authoritative metadata before application, represent blocked/pending state honestly and retain real FTS under [#72](https://github.com/izyuumi/everything/issues/72). Unset dimensions preserve the backend's default selection; the registry invents no default dimension. EmbeddingGemma 2 remains the only shipped model; the Rust interface stays model-independent. Swift consumes the descriptor instead of maintaining a model table.

The apply-behavior metadata defines the required future facade semantics. It does not change the current #107 restart-fixed service generation. In particular #109 must implement stopping new embedding/classification work when disabled; #108 does not claim that dynamic behavior works already.

## Patches, snapshots and outcomes

`SettingsPatch` carries version 1, an immutable `CommandId`, exact target scope, expected positive signed-64-bit revision, 1–8 typed changes and an optional confirmation reference. The encoded patch is at most 16 KiB. `validate()` rejects duplicate keys, cross-scope batches, read-only writes, key/value shape mismatches and bounded/canonical model/selection errors. It validates shape, not grants, consent or current backend availability. The service must bound bytes before decoding and invoke validation before processing. Unsupported combinations fail visibly rather than being normalized into permission.

#109 validates the entire same-scope batch and original revision before committing, preserves every unrelated field, and uses the existing private file/CAS path. Request identity binds authenticated actor, target, exact patch, original expected revision and confirmation. Exact replay returns the original durable outcome; changed reuse conflicts. A stale revision is not silently rebased. A disconnect neither proves rollback nor authorizes a new request ID.

`SettingsSnapshot` is a redacted 32-KiB maximum view with target scope, authoritative revision, observation time, stale flag and at most eight unique entries. Each entry distinguishes desired and effective values and Effective, PendingApplication, RestartRequired or Unavailable state; unavailable entries carry a closed reason code. Read-only state has no desired value. Effective state requires an effective value and no differing desired value. No snapshot contains instructions, main-chat history, workspace/security roots, credential references/values, private checkpoints, pairing material or device grants. Endpoint/model metadata is non-secret recipient data and still subject to per-key read authorization and hosted-provider disclosure.

`SettingsOperationResult` retains version, original request ID and target scope. States distinguish Applied with revision, PendingApplication, RequiresConfirmation, RequiresUserAction, HostActionRequired, RestartRequired, Conflict with observed revision, Unavailable, Failed, Unknown and Cancelled. Failure/unavailable reasons are closed sanitized enums. These serializable objects are inspection data, not authenticated receipts. Only an authoritative service result from the original authenticated request supports Saved/Applied presentation. #109 owns durable command outcomes and redacted change hints; missed hints/reconnect require a scoped reread. Relay delivery ACK is not mutation acceptance.

## Exactly seven logical tools

`SettingsToolName::ALL` fixes this complete namespace. `logical_name` is the public contract; `provider_name` is its one-to-one function-name-safe encoding, not another tool or an extensible alias list.

| Logical tool | Provider function name |
| --- | --- |
| `settings.list` | `settings_list` |
| `settings.get` | `settings_get` |
| `settings.update` | `settings_update` |
| `settings.request_action` | `settings_request_action` |
| `devices.list` | `devices_list` |
| `devices.pair` | `devices_pair` |
| `devices.revoke` | `devices_revoke` |

#113 must enforce this immutable offered and executable ceiling in trusted runtime/Broker code, including direct-ID calls. There is no shell, MCP, search_tools, System One selector, schedules, subagents, wiki, main history or owner-IPC pass-through. A different prompt on a general-purpose agent is insufficient. Settings context contains only safe descriptors, authorized redacted values and bounded session chat. Session state and its pinned active generation provider remain separate from ordinary conversations, consolidation and embeddings. No provider or credential fallback is silently selected. UI closure cancels its owned inference and expires unapplied ephemeral actions where safe; admitted mutations remain reconciled. The Host lifetime is independent.

## Confirmation and native/device actions

Confirmation and action references are non-secret UUID/expiry lookup metadata. They are not bearer authorization, public constructors of trusted consent or deserializable execution proofs. `SettingsRequestBinding` records the service-derived principal/session identity, exact target, request, observed revision, exact request SHA-256 and original issue/expiry times, capped at five minutes. It validates bounds only. #109/#110/#112 must retain the actual trusted request/diff/action and actor binding, generate the reference, and recheck exact contents, native confirmation, monotonic/original expiry and live grants immediately before apply. Never trust a caller-supplied binding or renew expiry on replay. Authenticated native cards come from this service state; model text/Markdown cannot create one. Closing/suspending or switching the owning connection/session cannot retarget a pending action.

`NativeSettingsAction` has exactly three cases: CredentialEntry with a purpose-specific binding, this-device NotificationAuthorization, and OpenSection with a known section enum. There are no arbitrary URLs, paths, commands, scripts, approval operations or generic service/account fields. Model credentials reuse the complete existing `ModelCredentialReference`; System One uses its distinct exact endpoint/reference namespace. Neither case contains a credential value. Credential entry stays host-local-owner-only. #110 must add the smallest authenticated in-memory/pipe-only path; the current macOS request-file bridge and ordinary chat/pending-action state must never carry secrets. Return only sanitized status/presence. An ambiguous write is inspectable, not presumed rolled back or automatically repeated.

Notification preferences belong to their named device. Observed authorization includes not-determined, denied, authorized, provisional, ephemeral and unavailable; only an actual OS query supplies it. A preference does not grant permission. Request authorization contextually on the requesting foreground device; denial needs supported native settings navigation or accurate instructions. Unsupported/background/disconnected UI actions expire rather than appearing later as surprise dialogs.

Device administration uses separate `SettingsDeviceRequest` and closed Pair/Revoke actions. Revoke binds the exact typed device ID and validated public keys. A sorted redacted `SettingsDevicePage` contains at most 100 bounded labels/public identities/revocation flags and an exact last-row continuation; no access metadata or invitation secret is included. #112 derives it from current authorized state. Pair returns only a non-secret action handle and expiry to the model; invitation/QR material stays on the authenticated ephemeral native-owner path. Existing explicit fingerprint comparison, final approval and grant selection are unchanged. Revocation requires exact native confirmation and live authorization. Creating an invitation never approves trust. Remote requests for credentials, pairing/revocation/grants, new recipient consent or lifecycle/security changes return HostActionRequired; no general OwnerOperation value crosses E2EE.

## Implementation gate and validation

This contract is based on normally merged #107 main `3893843744ab8cbb2ac630c31227df92051881eb`. It adds no database, runtime, native action, grant migration, settings command route or UI. #109/#110/#112 start only after this contract merges; #113/#114 follow their real interfaces, then #115/#116 and optional #117. [#118](https://github.com/izyuumi/everything/issues/118) reconciles actual macOS and remote/iOS milestones incrementally and reuses [ADR-0010](../decisions/ADR-0010-hybrid-settings-contract.md).

Rust 1.88 formatting, source/manifest/link inspection and independent source review are recorded for this contract. Fresh exact-head compilation, strict Clippy/build and any applicable manual observations remain pending. No runtime, action/consent, provider, Apple, credential or end-to-end settings verification is claimed. Deferred scope/security/replay/UI automation is consolidated in [#32](https://github.com/izyuumi/everything/issues/32); no new tests or fixtures are introduced.

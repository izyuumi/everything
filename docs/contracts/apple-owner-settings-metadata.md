# Apple owner credential metadata adapter

The two macOS leaves `OwnerSettingsDocument.swift` and `OwnerSettingsReceipts.swift` provide pure metadata preparation and original-outcome matching for [#110](https://github.com/izyuumi/everything/issues/110) and [#114](https://github.com/izyuumi/everything/issues/114). Their source base is normally merged PR #136, `95f23a5548e48932ee58385da0c171d1475c1431`, including PR #135's actual shared Settings DTOs. The current integration registers both helpers in the macOS target. They send no IPC, render no credential entry and do not establish a composed credential workflow; the actual client/connection lifecycle belongs to the app-owned native Session.

## Original document

`OwnerSettingsDocument(settingsResponse:hostID:)` accepts only the actual owner `kind: settings` query wrapper. It privately retains the complete immutable, nonsecret HostSettings JSONValue document, bounded to 64 KiB with schema version 1 and a positive signed-64-bit revision. It exposes only Host ID, revision, typed default-model selection, up to sixteen exact model credential bindings and configured SystemOne endpoint/model/reference/enable metadata. Host ID must come from the caller's actual connected Host identity; it is not supplied by HostSettings or inferred from a credential UUID.

The document also reserves every configured model, SystemOne and MCP environment reference. MCP references are read from the actual `mcp.servers[].environment` map of `{reference, storage}` records, with sixteen servers and sixty-four environment entries per server. References must be nonnil canonical lowercase UUID strings. Duplicate model provider/endpoint tuples are refused. Existing references shared across purposes are retained as metadata; a new enrollment cannot reuse any of them.

The original complete document stays inside the adapter. Candidates retain unrelated values, including opaque future fields, rather than rebuilding HostSettings from the redacted snapshot. Known projections use the merged DTO's closed variants and bounded validation; an unsupported schema, endpoint variant or credential storage is unavailable rather than guessed. This is a focused projection, not full Rust HostSettings validation: filesystem policy, canonical endpoints, authoritative file/cache agreement, original operation identity and all mutation authorization remain on the Host. Input formatting and object-key order are not retained; JSONValue semantic values are retained.

## Accepted credential context

The synchronous preparation entry point is:

```swift
func credentialMetadataPlan(
    request: SettingsActionRequest,
    admittedTarget: SettingsActionTarget,
    admittedOperation: SettingsOperationInspection,
    originalReference: SettingsActionReference,
    completed: SettingsOperationInspection,
    nativeSessionID: UUID,
    ownerRequestID: OwnerID = OwnerID()
) throws -> OwnerSettingsCredentialMetadataPlan
```

Callers supply the actual retained original request, Details target/inspection, action reference and final inspection from the same original authenticated native session and immutable connection. The adapter requires exact `.native(request:)` target equality, the original AwaitingAction reference including expiry, identical full admitted/completed bindings and origins, matching Host scope/revision/operation/session, and a genuine-shaped Finished Completed credential result without file/application metadata. No supplied success Boolean, message text or request digest reconstruction is accepted. The target's exact binding supplies the backend-accepted endpoint context; the helper implements no Swift URL canonicalizer. A configured raw SystemOne endpoint that differs from the accepted canonical endpoint conflicts and requires a separate reviewed configuration action.

These checks correlate typed outcomes. A DTO or UUID is not native authorization, a credential-presence query or proof of its transport provenance. Root must obtain these values through the actual retained native owner; constructing the same data locally grants no Host capability. The metadata CAS has its own verified LocalOwner authority.

The result has two cases:

- `configuredReplacement`: the exact existing provider/endpoint/reference/storage or configured SystemOne endpoint/reference already matches. No metadata request is prepared. This does not require the model tuple to be currently selected.
- `initialEnrollment(OwnerSettingsPreparedLink)`: a new binding can be linked at the original revision. A model must match the currently selected default provider/endpoint, have no configured tuple, fit the sixteen-binding bound, and use a reference absent from every configured purpose. SystemOne must already be configured at the exact endpoint with no reference and use a fresh reserved-namespace reference.

The prepared candidate only appends that one model binding or sets that one SystemOne `credential_reference`. It preserves provider/recipient, model, enable override, storage and all unrelated settings. It retains its own revision R; the Host's real CAS writes R+1. The final readable revision can support configured replacement, but initial linking at that revision is refused. The separate owner request ID must differ from the original native operation and native session IDs.

`OwnerSettingsPreparedLink` exposes immutable `hostID`, `expectedRevision`, `binding` and the exact `ownerRequest`. Its private candidate is never sent to model context or a raw formatter. The request is nonsecret and belongs only to actual owner IPC. The caller retains it synchronously before any await; helpers do not silently regenerate it after an uncertain send or rebase onto a newer snapshot.

## Receipts and original recovery

The prepared link provides:

```swift
func receipt(_ response: JSONValue, previous: OwnerSettingsReceipt? = nil) throws -> OwnerSettingsReceipt
func inspected(_ response: JSONValue, previous: OwnerSettingsReceipt? = nil) throws -> OwnerSettingsReceipt
func reconciled(_ response: JSONValue, previous: OwnerSettingsReceipt? = nil) throws -> OwnerSettingsReceipt
func inspectionRequest() -> OwnerRequest
func reconciliationRequest() throws -> OwnerRequest
```

The wrappers are closed and match the actual Rust owner protocol:

| Matcher | Required wrapper fields |
| --- | --- |
| receipt | `kind: settings_receipt`, `operation`, `settings` (object or null), `restart_required` |
| inspected | `kind: settings_operation`, `operation` |
| reconciled | `kind: settings_reconciled`, `operation` |

Legacy `kind: settings` write replies cannot become a successful link receipt. The shared SettingsOperationInspection decoder/validator checks bounded typed semantics first. Additional matching requires the original LocalOwner request ID equal to client-session ID, the actual Host principal/scope, original expected R, and Applying or a compatible Finished state. NativeUi or SettingsAgent inspections cannot be adopted as the metadata operation. The client does not guess Rust's canonical request-hash serialization; after the first real observation, the optional `previous` receipt binds all original request-binding fields, origin, write intent/resolution, immutable terminal phase and terminal application observation. Root must pass that retained observation on subsequent reads rather than discard known history.

This narrow metadata candidate has empty changed keys and no file restart requirement under the actual owner difference algorithm. Its write, if present, must retain expected R and post revision R+1. Applying and Unknown remain unsettled. A known NotApplied or pre-write rejection is distinct from an applied file. A known file save remains applied when separate application is pending, failed or unavailable. `OwnerSettingsReceipt` exposes `operation`, optional matching `snapshot`, `fileOutcome`, `restartRequired`, `application` and `applicationPending`; callers must present those separate facts.

A supplied receipt snapshot is accepted only with an Applied write resolution and must equal the complete original candidate except for revision R+1. Comparison recursively normalizes nonnegative signed integers and unsigned integers; known credential UUIDs are canonical lowercase. It never compares JSONValue's signed and unsigned cases naively. An unrelated field difference, different binding or conflicting snapshot is an error, not a saved result. Absence of a snapshot does not erase a known applied original file receipt: a later current revision may have superseded it. Snapshot adoption and current-state refresh remain caller responsibilities.

Inspection requests name only the retained original metadata operation. Reconciliation uses that same original target under a fresh, distinct outer owner request ID and never prepares a replacement write. Root must retain the actual outer request before sending. The current OwnerRequest file-transfer allowlist admits the existing nonsecret `reconcile_settings` operation. Its request contains only the original operation UUID, under a retained distinct outer request. The same allowlist refuses legacy `approve_pairing`, which could carry a signed request through a file. Final native pairing approval belongs only to the private native route. No private pipe accepts arbitrary owner operations. Reconciliation may resolve original file history and make its known original application work available to the Host; it never certifies a second file write or credential storage attempt.

## Composition and verification boundary

The future secure-entry consumer keeps the literal secret in the native SecureField/private submit path. These helpers accept no secret, SecretTransfer, raw invitation or private native proof. They perform no RPC, provider request, credential lookup, filesystem write, cache/history/logging, presence query, deletion, automatic retry, rollback or model-context conversion. Root keeps the same actual OwnerIPCClient and connection incarnation through original task completion and drain; no operation is retargeted to another Host/session on reconnect.

Initial storage and metadata linking remain separate resources. Storage Completed leaves HostSettings unchanged; the prepared link does not prove the key still exists, particularly after a Host restart loses Session storage. A CAS conflict or unknown result leaves a potentially stored but unlinked key, preserves the original request and requires original inspection/reconciliation. No automatic secret resubmission, reference migration or new revision is authorized by the adapter. Removal and current credential presence have no implemented native protocol here.

Source validation covers diff/ownership checks, Markdown links and real Swift grammar. Full semantic compilation depends on actual SettingsActionTarget/device types and CryptoKit, unavailable in this Linux environment; no substitute declarations or stubs establish a pass. Both leaves are registered. The complete composed PR #141 passed actual macOS/iOS SDK build/analyze in run `38903695700`, including both Mac files on arm64 and x86_64. Required Rust CI `38903695646` passed all Linux/macOS forms and Rust 1.88 MSRV before normal merge. All five raw job checkout trees equal the reviewed head. No UI, Keychain, Session-vault, SystemOne application, failure/Unknown/reconciliation runtime, provider inference or universal non-disclosure/erasure behavior is verified by these pure helpers. Earlier [credential walkthrough](../development/settings-validation.md#initial-credential-admission-increment) evidence retains its exact c627 artifact/source attribution; it is not a runtime observation of this Swift source. New automated verification remains deferred to [#32](https://github.com/izyuumi/everything/issues/32), and both implementation issues remain open.

See [hybrid Settings](settings.md), [revisioned owner routes](settings-owner-routes.md), [Settings service](settings-service.md) and [private credential actions](native-credential-actions.md).

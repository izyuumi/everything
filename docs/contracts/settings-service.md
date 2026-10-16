# Settings operation core

The #109 core extends the [typed contract](settings.md) with one leased Host authority and one bounded operation journal. This first source slice implements authentication, exact retained action identity, native admission and closed native observations. Host/CLI registration, the secret pipe, device effects, describe/get/update, cross-store file application and dynamic memory/selection changes are not integrated by this slice. It does not establish that the full settings facade works. A locked Rust1.88 all-feature workspace/all-target check passed on frozen afb4c4d with expected unconsumed private-entry warnings and one unused import. Later source corrections have formatting/static inspection only; full production integration, strict analysis and runtime validation remain separate coordinator gates.

## Trusted sources

SettingsService opens only with the same actual Agent StateLease as SettingsRepository and the actual configured Host endpoint identity. It shares the Host mutation admission supplied by composition. LocalCall is the server-created same-UID authenticated local request; a supplied principal/session UUID cannot create a caller. The local-owner caller, native UI caller and host-created restricted Settings session are distinct private source variants.

A native session receives a random 32-byte private capability through a dedicated sensitive pipe route once that route is implemented. Only a domain-separated SHA-256 fingerprint remains in its bounded in-memory record. Capability encoding uses the existing base64 library, randomness the existing OS RNG and secret ownership zeroizing storage. Public IDs, action references, operation inspections and receipts are data. There is no app attestation claim or new remote grant. Sensitive session/credential/invitation values must never reach ordinary request files, model arguments, durable rows, snapshots or logs. Secret-bearing wire types have redacted Debug and share one zeroizing allocation through Arc when the existing local transport clones a request/response. Opaque Rust callers, restricted-session leases and native admissions remain non-Clone/non-Serde. Frame, serde, Swift and OS buffers do not provide universal zero-copy or secure erasure.

There are at most sixteen native sessions and sixteen restricted Settings authority sessions. Native session lifetime is five minutes with original wall and monotonic deadlines. Closing a one-shot CLI connection is not UI-session close. Explicit close, UI-target change, expiry or Host restart rejects new admission; no replay extends a lifetime. An already-admitted exact action may finish/inspect through its original private session proof after close/expiry while that session remains retained. Such cleanup cannot read a fresh action, approve another patch, start another effect or retry a credential. A current authenticated local owner can inspect authorized historical receipts after restart.

SettingsAgentSession is an opaque authority lease minted from a live native UI caller. It is not an implemented agent runtime. Each caller minted from that lease is bound to one of the seven SettingsToolName values; action and device preparation require the matching exact tool. It cannot perform native admission/finalization or create owner/native authority. Closing/dropping its lease cancels its ceiling; native confirmation rechecks this original source. The actual immutable restricted profile and executable seven-tool namespace remain #113 work. There is no remote caller constructor; #116 must supply verified envelope/live explicit grants before such a path exists.

## Original operation and confirmation

The retained closed target is a SettingsPatch, SettingsActionRequest or SettingsDeviceRequest. Shape validation preserves the original exact typed bytes, scope and genuine optional revision. Fresh confirmation metadata is rejected. Binding also retains actual principal, native parent session, origin kind/Settings session where applicable, canonical request digest and original times. Changing any target, origin, revision or binding under the original request ID conflicts.

| Private phase | Public interpretation |
| --- | --- |
| AwaitingConfirmation | The original protected patch returns its one original reference and expiry. |
| AwaitingAction | The original native/device request returns its one original handle and expiry. |
| InFlight | One exact native request has been admitted; inspection reports this phase without inventing a settings revision. A projected operation outcome remains unknown until completion. |
| Finished | The retained authoritative result is immutable and replayable. |

Native confirmation/admission is a separate request tied to the retained original operation and exact reference. It does not add a confirmation field to an agent's original patch or mutate that patch's identity. One native claim per original effect is enforced by the database. Repeating the exact claim returns Existing inspection and never grants another dialog. Changing its native request ID, session or reference conflicts. Host grant/session/revision and the original monotonic deadline are rechecked under SQLite admission and before commit. No file mutation or protected patch application is supplied by this initial core.

NativeActionAdmission is opaque, non-Clone and non-Serde. It retains exact identity, cancellation, the actual Agent and state through real worker completion. `check_new_effect` rejects closed, cancelled or expired admission using both original wall and monotonic bounds. It is not permission to bypass actual credential/device validation. Host effect adapters must own their actual mutation admission and finish from the real vault/endpoint result; those adapters are not present here.

## Closed native observations

The native-only finalize surface accepts only Dismissed, exact known-section navigation, or actual notification authorization observation. It validates the original private session, admitted request and action kind, then the service generates the stable receipt. A supplied SettingsOperationResult, receipt ID or generic success flag is never accepted. Native observations cannot complete a credential write or device mutation. Observed OS state is not a grant. Cancellation closes admission and requests safe cleanup; it does not assert rollback of an admitted OS/Keychain effect.

The first source slice cannot validate a device preference revision without its actual device-owned adapter. Such valid but unsupported action requests return durable native-action-unavailable, rather than borrowing HostSettings revision. #110 supplies that real local adapter and notification/credential routes. No model/provider request is made by this core.

## Durable storage and resource limits

Migration0013 adds three tables to the existing Agent Store after actual worker schema12:

- settings_operations: immutable typed original identity, original creation/expiry, at most18KiB per canonical JSON record.
- settings_native_requests: immutable exact native request binding, unique native ID and unique original operation ID, at most1KiB.
- settings_operation_updates: append-only indexed sequence/phase/result, at most2KiB per canonical record. Later key-only change hints have a separate1KiB bound.

SQL enforces JSON/identifier/time/byte shape and immutable-row triggers. Typed reads check canonical encoding, complete binding/target digest, source identities, reference expiry and actual native-claim linkage. Lengths are checked on borrowed SQLite text before copies. Private DB data alone never mints a live caller/native session.

There are at most1024 combined retained original/native request identities,64 pending operations and8192 updates. Preparation reserves future native-identity and terminal-update capacity in the same transaction; a pending operation cannot consume the last space needed for its own claim/completion. Exact retained replay precedes capacity checks. No receipt/identity is evicted or silently renewed. New admission fails visibly when retention is exhausted. The unique native claim covers a single effect: a pairing creation receipt does not represent later trust approval or pending invitation cancellation.

Startup closes old unadmitted requests as expired and old InFlight requests as Unknown because ephemeral native authority was lost. It never recreates a session, invitation, confirmation or effect. Final known native observations replay only with their original exact source and observation. Host worker/file/Keychain reconciliation will need additional exact effect evidence; this slice makes no atomic cross-store claim.

All blocking work retains the complete service core, whose actual Agent is its last field. Native claims separately retain their actual Agent last. Owned Host mutation guards move into the actual blocking worker before awaiting, so waiter cancellation cannot release them ahead of SQL work. There is no standard mutex held across an await and no detached unowned effect.

## Pending integration

Root owns production registration, storage schema13 registration and sensitive owner/local/CLI routes. Direct pending-pairing attachment/cancellation waits for the actual typed DeviceEndpoint primitives to merge; no guessed API or substitute callback has been added. #110/#112 bind only after this core's actual interfaces are reviewed. Full #109 still requires descriptors/snapshots, complete typed batch application, file/journal intent/reconciliation, redacted hints, actual desired/effective generation application and disposable verification. Deferred automation remains [#32](https://github.com/izyuumi/everything/issues/32).

Fresh native admission checks exact durable replay first, then verifies memory ownership eligibility before committing InFlight. The map mutex is released before SQL; the shared Host mutation guard serializes fresh growth while completion only shrinks ownership. An unexpected postcommit bookkeeping failure attempts to retain an explicit Unknown and returns its original request identity and structured repair errors. Known unadmitted cancellation removes the original pending deadline. NativeHostEffectAdmission is a second opaque server-only lease: it retains the same actual Host mutation guard and rechecks original source, revision and both expiry bounds before marking effect admission. Potentially blocking revision inspection precedes the last live-origin, cancellation and original-expiry checks; no captured deadline is renewed. It accepts only a retained patch/device/credential target; it does not finalize an effect or accept client-supplied success. Actual effect adapters remain pending.

# Rejection boundary for encrypted Settings packets

The Rust `DeviceEndpoint` classifies authenticated plaintext before the durable inbox copies it. Settings is unavailable at this boundary: no remote Settings authority, session, command, model work or mutation is admitted. This is a prerequisite for eventual scoped remote Settings using [explicit settings grants](settings-grants.md), not a completed #116 receiver.

The source is subject to exact-head compilation, independent review and later disposable encrypted observations. Formatting and source review do not establish runtime privacy or security readiness. Automated coverage remains deferred to #32.

## Routing

The existing native signature/header/AEAD checks, plaintext size limit and `ReplayGuard` API remain unchanged. Authentication, empty/oversized plaintext and other pre-hook crypto failures retain their original meaning and do not invent rejection markers.

Inside the replay callback, the complete input must be UTF-8 and a single JSON object with exactly `schema_version` and `payload`. The payload must be an object with one string `kind`. Standard serde map/string visitors constrain these representations; derived records reject duplicate routing fields and unknown outer fields. Other payload members are ignored without building an owned body tree. The existing serde recursion limit stays enabled. Escaped field names/discriminators are decoded by serde.

| Complete routing result | Admission |
| --- | --- |
| Representable outer version other than current schema1 | Terminal `unsupported_version` |
| Schema1 and `command`, `command_result`, `sync_request`, `sync_batch`, `sync_acknowledgement` or `revoked` | Existing ordinary path with the original authenticated bytes |
| Schema1 and `settings`, including escaped spelling | Terminal `settings_unavailable` |
| Malformed/ambiguous routing, non-object records, non-string/unknown kind, invalid UTF-8, missing fields, unknown outer fields or trailing documents | Terminal `invalid_packet` |

The complete probe must deserialize before version classification. Out-of-range/noninteger versions are invalid packets. No future Settings body/version is decoded, and no grant is looked up. Parse errors and invalid values are discarded without formatting or logging.

Normative ordinary packet bytes are neither normalized nor re-encoded. Their body validation and authorization still happen in the existing ordinary processor. Duplicate unrelated ignored body fields retain that later behavior. Duplicate/ambiguous routing fields intentionally lose the old downstream `Value` normalization behavior and are rejected before persistence. The routing boundary cannot detect arbitrary private content mislabeled inside an ordinary lane.

## Durable receipt

`Store::reject_transport_inbox` is crate-private and accepts only borrowed envelope metadata plus a closed `TransportInboxRejection`. It has no body, payload length, arbitrary detail string or plaintext digest. The existing ciphertext-envelope digest construction is unchanged.

Ordinary and rejection admission share one immediate transaction and the existing validation, complete public pin/access equality, active local-recipient check, pruning, exact replay comparison and marker-cap policy. An exact sender/recipient/ciphertext match returns Duplicate before capacity and without a commit or row rewrite; its earlier speculative pruning is rolled back as before. A mismatched tuple remains an actual storage conflict.

Fresh ordinary admission retains its payload and all existing global/per-peer pending count and byte limits. Fresh rejection inserts a terminal row directly with `state='rejected'`, `payload=NULL`, `command_id=NULL` and one fixed detail. There is no temporary pending row or empty dummy payload. Schema15 already supports this shape; no migration or private original/command identity is added.

Rejection consumes a replay marker but no pending-payload capacity, so it can commit while ordinary pending queues are full. The existing 100,000-marker ceiling still applies. Marker exhaustion keeps actual `TransportCapacity`, no ACK and the current relay error disposition. There is no new retry/reconnect/status branch; accepted private ingress still requires its separately reviewed backpressure design.

Existing duplicate rows are never relabeled. This boundary does not erase private plaintext already persisted by an older version or previously admitted ordinary work. Historical retention/cleanup is outside this change.

## Outcomes and errors

The callback retains an explicit private outcome beside the unchanged crypto result. A fresh rejected commit produces transport `EnvelopeAcceptance::New`; an exact replay produces `Duplicate`. The returned zeroizing plaintext is dropped, and rejection does not wake the ordinary inbox processor. No Settings payload becomes a `ReceivedOperation` or reaches ordinary dispatch through this fresh rejection path.

The existing relay ACK follows successful actual receive completion. New/Duplicate describes transport receipt only, not accepted Settings execution, a native action, known application NoEffects or a delivered denial ciphertext. No rejection response is sent.

Rejected-path Store failures are retained as their original typed `StorageError` through one fixed `CryptoError::ReplayStorage` sentinel, then returned as actual `DeviceError::Storage`. Checked receipt-time failures use fixed typed invalid-input errors. The private branch never stringifies a Store/parser error or stores a private body/debug value. Ordinary error compatibility remains unchanged. Missing or inconsistent crypto/guard outcomes fail closed; no absent outcome implies success.

Uncertain storage commits are actual failures, not proof a marker is absent. They do not justify a new application request or simulated rollback. Active-pin/local-recipient failures and corruption/I/O/poison remain their genuine typed errors rather than blanket policy denials.

## Remaining boundaries

No Host mutation guard, Agent, queue, supervisor, provider, native crypto API, relay frame or Apple receive path is introduced or changed. The existing receive worker can detach on caller drop and does not retain an Agent state lease; this increment makes no stronger lifetime, shutdown, cancellation or drain claim. It admits no private executable destination.

Actual accepted remote Settings still needs its reviewed authority, original-request persistence, bounded handoff/publication, backpressure and caller/worker ownership implementation. Apple private ingress remains separate. Supported builds/static analysis and actual disposable encrypted refusal/replay observations must be recorded separately. No full Settings receiver, UI behavior, inference, universal non-disclosure/erasure or production/security readiness follows from this contract.

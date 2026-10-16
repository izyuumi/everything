# Apple rejection boundary for private Settings packets

The iOS receive path classifies authenticated plaintext before the ordinary durable inbox receives it. This is a fail-closed prerequisite for [scoped remote Settings](settings-grants.md), not an accepted Settings reply, session or client. The existing Rust [rejection boundary](private-settings-ingress.md) remains separate. No remote Settings authority, provider work, native action or mutation is admitted.

## Authentication and routing

`DeviceIdentity.receive` retains its existing exact sender/recipient checks, original header expiry, pinned Host signature, AEAD authentication and nonempty 1-MiB plaintext bound. Failures in those steps still throw before classification and create no replay marker. Only successfully authenticated, bounded plaintext reaches `DevicePackets.routing`.

The classifier reuses `WireValue.parse`, including its existing complete Foundation syntax parse, decoded duplicate-key rejection, depth/key bounds and closed field accessors. No parser, dependency, cryptography or canonical-byte comparison is added. The complete outer object must contain exactly `schema_version` and `payload`; version must be the current unsigned 1, payload an object and `kind` a string. Escaped field names and discriminators use the existing decoded values. Duplicate routing fields, including escaped spellings, are rejected by the existing uniqueness check.

| Routing result | Durable path |
| --- | --- |
| Version 1 with `command`, `command_result`, `sync_request`, `sync_batch`, `sync_acknowledgement` or `revoked` | Existing ordinary admission receives the exact original plaintext bytes |
| `settings`, unknown kind/version, malformed or ambiguous routing | Metadata-only terminal rejection |

The six ordinary tags are the actual existing `DevicePayload` inventory. Their body validation and direction-specific handling remain in the existing processor. The classifier does not decode a future Settings body. Parser errors become one closed rejection disposition; their text or input is never retained. No routing result, UUID or callback name creates authentication authority.

## One existing replay transaction

`DurableEnvelopeInbox.rejectAuthenticated` accepts only the encrypted envelope, expected public Host identity and local device ID. It has no plaintext, parsed tree, private payload hash, body length or arbitrary diagnostic parameter. The same `ReplicaStore` implements ordinary and rejected admission through one private `recordAuthenticated` helper and its existing immediate transaction.

The transaction rechecks the actual current Host pin, local identity, recipient/sender and sticky disabled state. It constructs the same ciphertext-envelope digest and compares the original message ID before capacity. An exact existing digest returns Duplicate without modifying that row's payload or completion state. Changed reuse still conflicts. Historical rows are never relabeled to manufacture a privacy result.

Fresh ordinary admission retains the existing 150,000 total-receipt, 128 pending-payload and 16-MiB pending-byte bounds and the exact original payload insertion. Fresh rejection uses only the total receipt budget: it inserts the existing metadata fields with `payload=NULL` and `done=1` in one statement. It can commit while the ordinary pending-payload budget is full, but not beyond the receipt limit or database capacity. There is no temporary pending row, second table, schema change or private Settings original in storage. Existing completed/expired receipt pruning remains unchanged.

The existing ciphertext-envelope digest is not a plaintext digest or an operation receipt. No refusal marker authorizes a Settings caller, completes a mutation, proves NoEffects or verifies an application's state.

## ACK, failures and ownership

`ReplicaService` is unchanged. Its existing delivery handler sends a relay ACK only after the actual receive/admission call succeeds. Actual storage, conflict, capacity or commit failure propagates through that call and prevents the ACK. A successful refusal marker and an exact duplicate are transport dispositions, not Saved or accepted Settings execution. No denial reply is fabricated. Rejected packets supply no pending payload to `pendingInbox`; the unchanged delivery handler may still drain previously pending ordinary work.

No new Task, worker, queue, continuation, session, owner or lifetime registry is introduced. The same CryptoKit actor, inbox actor, receive group, socket and existing stop/join path retain their work. This source makes no stronger caller-drop, cancellation, cleanup or termination guarantee than those existing owners. Authenticated private bytes and parser allocations are bounded memory; compiler/Foundation/OS lifetimes and secure erasure are not guaranteed.

This prospective guard cannot recognize private material falsely embedded in a valid ordinary body. It does not erase private plaintext already stored by an older client, rewrite an existing exact replay or repair a previously malformed inbox. No comprehensive non-disclosure, legacy cleanup or security readiness is claimed.

## Remaining integration and validation

Accepted private Settings still requires the genuine #116 device-parent/grant authority, frozen command/reply contract, immutable originals, reply reservations/publication, expiry, revocation and reconnect handling before any #117 Host controls or restricted chat can use it. Relay ACKs cannot settle those originals; ordinary command/event caches, Mac owner IPC and native-session proof cannot substitute for that boundary. The existing [local iPhone controls](apple-local-device-controls.md), [committed-event notifications](ios-notification-delivery.md), privacy overlay and single delegate are unchanged.

Source validation is limited to actual Swift grammar, exact source/project preservation, documentation links and diff checks. Supported Apple SDK build/analyze, relevant Rust CI and separately authorized disposable encrypted rejection/replay/pressure/cancellation and database-footprint observations remain required. No Apple UI, device/relay roundtrip, database, Host/provider or native runtime observation is established. No new tests, fixtures, source comments or dependencies are introduced; deferred coverage remains [#32](https://github.com/izyuumi/everything/issues/32), and #116/#117 remain open.

## Shared replica transaction quarantine

The actual shared `ReplicaDatabase` now retains a one-way connection-health flag. If an operation or COMMIT throws and its actual ROLLBACK also throws, the synchronous transaction helper sets that flag before returning from the same `ReplicaStore` actor turn. A private `ReplicaTransactionRollbackFailure` retains both the original error and the rollback error. Its localized, description and debug output are fixed strings and expose no SQL, parameters, private payload or diagnostic text. A successful rollback preserves the original error unchanged; no failure fabricates a committed receipt or proves NoEffects.

Every later query or write on that same handle passes through the health check in `rows` before preparing SQL. `execute`, BEGIN, COMMIT, ordinary inbox/outbox operations, metadata and snapshots share that entry. The private database has no escaped raw handle; the existing actor and synchronous transaction/map closures serialize its use. Quarantine therefore blocks ordinary callers as well as future private Settings consumers after rollback uncertainty. It supplies no reset, repair, new connection, cleanup join or recovered outcome. The existing resource deinitializer still calls `sqlite3_close` for its owned handle.

This fence covers that actual shared handle only. It makes no guarantee for separately opened connections or process-restart repair. Swift thrown-error handling does not catch traps, out-of-memory termination or process death, and cancelling an awaiting caller does not prove SQLite stopped or rolled back. Supported SDK compilation, real database rollback faults and cancellation/reconnect behavior remain separate validation; source syntax and preservation do not establish those observations or accepted Settings adoption.

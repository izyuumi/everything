# Settings transport storage foundation

The storage source includes schema17 and shares fresh outbox admission across both existing Rust insertion paths. It reserves a schema boundary for private Settings publication without creating a private receiver, reservation producer, ciphertext publisher or send owner. This increment does not complete [#116](https://github.com/izyuumi/everything/issues/116). The current authenticated private ingress still uses the [rejection boundary](private-settings-ingress.md).

Compilation, bundled-SQLite migration execution, populated upgrades and actual Store behavior remain separate validation gates. Formatting and source inspection do not establish runtime privacy, security or successful remote Settings execution. Automated validation remains deferred to [#32](https://github.com/izyuumi/everything/issues/32).

## Migration ownership

`Store` applies migrations through17 in its existing single immediate migration transaction and foreign-key policy, then records the schema version. Migration17 follows the [permanent Originals foundation](settings-originals.md). It adds exactly two STRICT tables and five triggers, giving268 explicit schema objects after the261-object prerequisite. Constraint autoindexes are separate from that count.

| Object | Responsibility |
| --- | --- |
| `settings_transport_reply_reservations` | Immutable future ciphertext capacity and closed public provenance. |
| `settings_transport_private_outbox` | Immutable provenance for an actual outbox/transport-outbox ciphertext pair. |
| `settings_transport_reply_reservations_immutable` | Reject every reservation update. |
| `settings_transport_reply_capacity` | Reject fresh reservation identity conflicts and projected quota overflow. |
| `settings_transport_private_outbox_immutable` | Reject every private provenance update. |
| `settings_transport_private_outbox_root` | Preserve a tagged ciphertext root while allowing bounded monotonic retry bookkeeping. |
| `outbox_capacity_admission` | Independently check every fresh outbox insertion against queued and reserved capacity. |

The reservation has an immediate peer-device foreign key. Private queued provenance has immediate foreign keys to its peer device, actual `outbox` root and actual `transport_outbox` root; deletion of either ciphertext root cascades its provenance. No existing object or historical row is replaced, rewritten, pruned or marked delivered by this migration.

The two tables constrain canonical nonzero lowercase UUIDs, positive signed-range integer times, expiry after creation and closed Host/Replica public provenance. Encoded provenance is at most2048 bytes; provenance and duplicated scalar metadata together are at most3072 bytes per row. These are logical encoded limits, not physical SQLite, WAL, heap or RSS bounds. Public pins, epochs, grants and parent IDs recorded in a row are correlation data: they neither authenticate a current connection nor mint a caller. Actual cryptographic key validity and current authority require the later real producer.

Families reserve complete ciphertext bytes: denial4096, default49152, inspection90112 and status360448. An actual queued ciphertext is positive and at most its family allowance. The migration contains no private request/reply text, plaintext hash, diagnostics, credentials or new command journal.

## Fresh admission and existing queues

The private storage module contains two helpers used by real existing Store methods. They are visible only within storage; no new public transaction or producer API is exposed.

`check_outbox_capacity` reads actual pending outbox counts and ciphertext bytes plus actual unconsumed reservation counts and reserved bytes. It checks the incoming ciphertext against both global and target-peer limits inside the caller's existing immediate transaction:

| Scope | Queued plus reserved messages | Queued plus reserved ciphertext bytes |
| --- | --- | --- |
| Global | 1024 | 134217728 |
| Target peer | 64 | 16777216 |

`Store::enqueue` and the shared `enqueue_transport` insertion helper both call this check. The latter is used by existing encrypted queue, remote-command completion and replica-sync paths. Their existing input, active-device, local-endpoint and transaction ownership checks remain in their respective paths; ordinary identifiers do not acquire a new UUID restriction. Their actual outbox/transport-outbox insert statements remain unchanged.

Projected counts and byte totals use checked integer arithmetic. Expected insufficient headroom returns the existing typed `StorageError::TransportCapacity("encrypted sender outbox")`. Negative accounting returns a fixed typed conflict. SQL aggregate overflow, decoding, I/O and poison failures retain their actual error cause; trigger messages are not parsed to classify capacity.

Older `Store::enqueue` versions could create queues exceeding these limits. Every existing row is grandfathered, including an over-limit ordinary backlog. Migration does not assert an initial aggregate bound or repair that backlog. Existing drain, expiry, revocation, deletion and monotonic defer operations do not require fresh capacity. A new admission must fit its projected global and target-peer limits; otherwise it is refused until actual releases leave sufficient room. Counts include all pending ordinary and private ciphertext rows; queued private bytes are not also counted as an unconsumed reservation after a future valid transfer.

The two new private tables begin empty. Their own fresh admission guards provide a separate logical bound on newly admitted private capacity metadata; they do not retroactively bound all existing ordinary queues.

## Generic outbox exclusion

`pending_outbox` and `pending_transport_outbox` exclude rows having actual `settings_transport_private_outbox` provenance. They retain their ordinary delivery filters, ordering and limits. No ciphertext from a tagged row is copied into these generic pending results.

`require_ordinary_outbox` checks that same actual table fact before `acknowledge_outbox` changes delivery state or `confirm_transport_outbox` reads a candidate ciphertext. A tagged row returns the existing typed `StorageError::TransportConflict` with the fixed message `private settings outbox requires its owned transport`. No private payload or incoming identifier is formatted into that error. Generic confirmation cannot turn a private row into an ordinary send candidate. The private root trigger also refuses a non-null delivery flag or replacement of its ciphertext identity, recipient, bytes and creation time.

Existing transport acceptance deletion, expiry/revocation deletion and valid defer bookkeeping remain available. Their deletion/cascade behavior is not a new authorization or refund guarantee. This foundation does not implement the future private candidate lookup, current-owner send gate or reply-reservation release policy, and it sends no private row.

## Future producer boundary

No current method in this increment inserts either new private table. Accepted ingress still needs the real authenticated connection, original-request transaction, current grant/epoch checks, same-Authority caller and owned request/worker/result lifetimes. A later reservation-to-ciphertext transfer must delete the exact reservation, insert actual quota-checked ciphertext roots and insert matching immutable private provenance in one immediate transaction, rolling back every step on any failure. There is no new standalone quota service or second management runtime.

Reservation and outbox admission guards reject existing identities before insertion, including replacement attempts through those guarded roots. Within the approved seven objects, private provenance has no insert guard against arbitrary raw-SQL `OR REPLACE`. A future private producer must use plain `INSERT`, retain its actual publication owner and compare the full original reservation/provenance; a replacement row or DTO cannot authorize publication. `transport_outbox` has no new global update-immutability trigger. Cross-row ciphertext/header/recipient/expiry checks and current connection/grant gates remain required from the actual producer and sender. These limitations are not solved by a public metadata row or foreign key.

## Validation scope

The migration is byte-identical to the independently reviewed seven-object SQL17 proposal. The coordinator separately observed one empty-memory migration preparation with system SQLite3.53.1, foreign keys enabled and one immediate transaction: migrations1–17 yielded268 explicit objects, integrity OK and no foreign-key violations. That observation is not a Rust bundled-SQLite, populated migration, quota/transfer, Store-call or encrypted runtime result.

This source slice was limited to direct Rust1.88 formatting, diff and source-preservation checks. It adds no automated tests, fixtures, source comments, dependencies, crypto changes, remote grant activation or private Apple receiver. Exact-head CI, independent source review, real accepted consumers and separately authorized disposable behavior observations remain necessary before broader claims. [Grant storage](settings-grants.md), permanent Originals and the current refusal boundary remain distinct prerequisites.

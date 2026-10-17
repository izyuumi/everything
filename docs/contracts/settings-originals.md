# Permanent paired Settings originals

This storage foundation adds schema 16 and crate-private metadata helpers for issue #116. It does not admit paired callers, activate private packets, create a remote parent, run a Settings agent, or send replies. The actual authenticated caller, Authority, same-kernel producers and transport consumers remain integration prerequisites. These source changes have not yet passed Rust type checking or populated-database/runtime verification.

## Stored identities

`storage/settings_originals.rs` defines a closed `RemoteOriginalIdentity`: either an accepted ChatOpen, Send or tiny memory-preference Update, or a suppressed ChatOpen. Suppression records the actual close and admission facts without inventing an unseen Open interval. It is born closed with the immutable Suppressed terminal and cannot acquire an execution link.

The permanent key is the actual Host, verified actor device and original request ID. It survives transport pruning, grant rotation, reconnection, process restart and loss of private RAM until it is terminal and past its retention window. The row contains only bounded public metadata. Send text, prompts, continuations, raw packet bodies and hashes of private text are excluded. An accepted Send retains its public chat and provider-binding identities. An Update retains the original expected Settings revision and only embeddings enabled/dimensions values.

Native session reservations retain their existing flat version-1 structure and field order. Paired version-2 reservations use a separate closed structure containing the explicit Host owner, full peer public keys, original remote parent, grant/revision, Host/connection epochs and original Open request. `StoredSettingsSessionReservation` is an internal dispatch enum; it adds no serialized wrapper or invented native identity. Historical decoding requires canonical bytes, bounded fields and immutable provenance, without requiring a still-live grant or consulting current time.

Existing `SettingsTurnFence`, `SettingsCleanupAttestation`, fence SHA domain and serialized version-1 fields remain unchanged. A paired turn's original Send association is a separate immutable SQL scalar. Native turns require NULL; paired turns require an exact nonnil original ID. Recovery and cleanup dispatch by the actual stored reservation version. Cleanup requires the original native principal for native rows and explicit Host owner for paired rows, with the existing exact fence hash, phases and wall-floor requirements.

## Transactions and replay

Six crate-private helpers borrow the actual caller's connection or transaction:

| Helper | Result and responsibility |
| --- | --- |
| `read_original_on(&Connection, RemoteOriginalKey)` | Optional bounded canonical record with scalar and linked-root provenance checks |
| `insert_original_in(&Transaction, &RemoteOriginalAdmission)` | Fresh plain INSERT followed by exact readback |
| `close_original_in(&Transaction, RemoteOriginalKey, &RemoteOriginalClose)` | First immutable close, or exact read-only replay |
| `finish_original_in(&Transaction, RemoteOriginalKey, RemoteOriginalTerminal)` | First immutable terminal, or exact read-only replay |
| `reserve_session_in(&Transaction, &StoredSettingsSessionReservation)` | Existing session admission and atomic OriginalChat link |
| `arm_turn_in(&Transaction, &SettingsTurnFence, Option<CommandId>)` | Existing fence admission and atomic OriginalTurn link |

These helpers do not obtain another Store connection, begin or commit a transaction, invoke a provider, acquire authority or allocate a server task. The native Store wrappers keep their signatures, immediate transactions, prevalidation and pre-encoding, replay-before-clock ordering, retention checks and native INSERT statements. Shared writer bodies retain the original unspent fence, global unresolved-turn, eight-turn and namespace checks.

The actual accepted-ingress owner must hold the genuine Host mutation guard and Main Store/Agent/lease ownership, check the live full pin, grant, epochs and original wall/monotonic bounds inside the same immediate transaction after waits, and recheck before commit. The session/turn INSERT guards independently check stored grant and endpoint facts. Historical reads are not fresh admission. No helper or public UUID substitutes for an authenticated paired caller.

Fresh receipt ownership must read the permanent key before inserting. An existing original returns retained facts; losing private RAM never permits another paid Send. `insert_original_in` cannot implement replay through ignore or replace. Exact existing close/terminal values return without a write; different existing values conflict. Missing rows are absence of a record, without a claim that no effects occurred. Unknown remains immutable while a separate real service inspection can report later facts.

There is no link setter. Actual session, turn and operation INSERT triggers install each link in the same statement and require exactly one eligible original. Both operation/fence insertion orders check their full native or paired origin. The new Rust operation decoder checks canonical closed metadata and immutable relationships; the real Settings operation repository remains responsible for request digest and business-state/history validation. No operation producer or Authority is added by this slice.

## Capacity and migration

Original retention is finite: 16,384 globally and 2,048 per actor. Live and non-terminal rows are never evicted. A terminal row may be deleted once 24 hours have passed since its `expires_at` (or `admitted_at` for a suppressed Open); the delete trigger enforces that with SQLite's clock, and transport maintenance and Store opening sweep such rows. After deletion the key may be admitted again; by then any replay of the original request has long failed its own expiry checks. Every admitted row reserves 2,176 logical bytes: 1,024 base, 512 link, 128 close and 512 terminal. The base and link ceilings include their duplicate scalar columns. This is a logical record bound, not a physical database, heap or RSS guarantee. Reducing close/terminal updates consume the original reserved row capacity rather than requiring a fresh Original or reply slot.

Migration 16 reconstructs the six Settings roots with foreign keys enabled inside the existing immediate migration transaction. It preserves native raw values, hidden rowids and the exact presence/high-water values of relevant AUTOINCREMENT sequences. Aborting scratch assertions check the full object inventory and preservation. The migration adds thirteen Originals objects and leaves the 156 unrelated schema-15 objects intact, for 261 explicit objects. Paired-root duplicate guards run before INSERT, including incoming native replacements of existing paired roots, independently of recursive delete triggers. Native-to-native predicates are preserved.

The installed migration is byte-exact to reviewed corrected SQL SHA-256 `786ac5fd0922d8ee79f30fa292dd858034d9df650c0c61a54d93fb2b3750f7e3`. Root's separate system SQLite 3.53.1 empty-memory preparation applied migrations 1–16 in one immediate transaction with foreign keys and integrity checks and observed 261 objects. That evidence does not verify populated migration preservation, bundled SQLite, Rust Store behavior, authorization, cancellation, capacity or accepted transport execution. Rust formatting is the only local source validation performed for this slice; compiler, integration and runtime gates remain pending. Deferred automated verification remains tracked in issue #32.

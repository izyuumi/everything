# Current remote Read query qualification

The Core Read gate accepts the actual opaque
[verified Read Source](settings-read-sources.md) and the original
SettingsReceiptAuthority with a supplied same-Main Transaction. It returns an
opaque QualifiedSettingsReadQuery borrowing that exact Source, capability and
Transaction. Its only public metadata getter is First or Known. It creates no
native SettingsCaller, mutation permission, value snapshot, Original lookup,
receipt/reservation, committed owner, BUILD or accepted route.

The existing Authority query map is shared across AccessStatus, Snapshot and
InspectOriginal. Its closed typed intent includes the complete immutable inner
request, with full peer keys, one cell identity, original first wall/monotonic
floor and deadline, and nondecreasing observed clocks. Limits remain512 global
and64 per full peer across all registered families and states. Reuse of a
device/query UUID with changed family, full peer or inner intent conflicts.
Neither expiry, error, rollback, cancellation nor completion evicts or refunds a
cell. Current connection replacement still requires the actual old owner to
retire and truly drain before the genuine new incarnation can reach Core.

Existing AccessStatus signatures, first clocks, error order and known-inner
response behavior are preserved. New Read First/Known is distinct: a Known Read
cannot allocate another response or slot in the future sole receipt producer.
It means only identical retained nonce history, not proof that a reply or commit
exists. Metadata-only new-outer receipt handling remains a later implementation.

Qualification binds the same Core Agent/StateLease/Main, endpoint, mutation and
attached Host to the actual Source. The original guard-active Source check must
still succeed. The existing strict full Host configuration/key/role and complete
peer registration/access/nonrevocation reader runs before current grant SQL. The
actual Authority then checks the live parent record, exact generation/full peer
and connection, parent cancellation/lifetime, query identity and first clocks.
After all SQL, a final actual Authority observation repeats those checks and
must not regress below earlier samples. No subsequent SQL or renewed clock is
used to manufacture this result.

Current grant identity/revision matches the live parent generation. The grant is
Active and not revoked for either Host or peer. Snapshot requires every requested
key in current readable keys before any value capture; writable keys do not
grant read authority. Inspection retains only the current readable ceiling.
Actual Original/operation lookup, requesting-peer/owner provenance and all
disclosed target/changed-key checks are separate prerequisites before disclosure.
Old Original generation remains DATA and cannot resume old execution.

The fixed qualification window is clipped to the first query, current original
Source, actual parent and grant horizons using original floors. Recheck verifies
the same capability/Transaction/invocation/cell, immutable first clocks, parent
lifetime, grant read facts and fixed horizon. It never registers again, restamps
First to Known or renews the query. Source guard-active checks are prejoin only;
later output needs its own actual joined-owner gate.

Actual Authority, SettingsReadSourceError and pin serde causes remain typed with
fixed redacted descriptions. Only the actual512/64 comparison before insertion
is query capacity; failures after registration leave the cell charged. This slice
performs no SQL writes or rollback and makes no ACK/NoEffects classification.
The caller must later provide the genuine sole Store Immediate transaction;
pointer equality and a Store identity check do not prove arbitrary transaction
origin or successful commit/join. The [scoped DATA](settings-scoped-reads.md) and
[Access receipt](settings-access-receipts.md) contracts retain their boundaries.
Actual receipt/true join/BUILD/projection/seal/send and Apple adoption remain
unimplemented consumers. No compiler, database/runtime or security acceptance is
claimed; #116/#117 remain open and automation stays #32.

# ADR-0013: Permanent shared Store quarantine

Status: Accepted. Fault behaviour is unverified.

## Context

An explicit SQLite rollback may fail and leave the shared connection inside an uncertain transaction. A worker-local failure flag cannot stop other Store clones from using that connection after its mutex is released. Holding the mutex forever would block them instead of returning an error.

## Decision

All clones of a Store share one permanent health latch, checked before and after mutex acquisition. After an observed rollback error, the writer sets it while still holding the mutex and keeps both errors. Later checked borrows return `StorageError::Quarantined`. There is no clearing or recovery API; a process restart is required.

In the remote Settings Access path ([Access transport](../contracts/settings-access-transport.md)) the Store latch is set only on failed rollback. Settings receive itself quarantines (ending the relay session until restart) only on faults whose outcome is uncertain or whose bookkeeping is broken: SQL or storage faults, failed COMMIT, failed rollback, bookkeeping and worker failures. Peer-attributable failures (expiry, out of scope, conflicting query reuse, clock regression, revocation, transport conflict, unavailable clock) are per-message rejections, and capacity is a non-ACKed retry. Neither quarantines.

## Consequences

An uncertain transaction cannot be inherited through checked Store access. The latch grants no authority, proves no rollback or commit result and repairs nothing. A failed rollback stops all checked Store use until restart, so the classification above keeps routine peer errors from triggering it.

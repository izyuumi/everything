# ADR-0013: Permanent shared Store quarantine

Status: Accepted for the unmerged scoped Settings integration; compilation and fault behavior remain unverified.

## Context

An explicit SQLite rollback may fail and leave Main inside an uncertain transaction. A worker-local failure flag cannot stop unrelated Store clones from borrowing that same connection after its mutex is released. Retaining the mutex indefinitely would block those callers instead of returning a useful error.

## Decision

Share one permanent health latch with all clones of the existing Store. Check it before and after connection-mutex acquisition. After an actual rollback error, a writer sets it while still holding that same mutex, preserves both real errors and keeps its original work ownership nonclean. Future checked borrows return a distinct Quarantined error. The latch has no clearing or recovery API; restart and original uncertainty reconciliation remain necessary.

## Consequences

An uncertain transaction cannot be inherited through checked Store access. Main remains one shared connection and uses the existing lock order. The latch grants no authority, proves no rollback/commit result and refunds no Source or permit. It does not repair persisted uncertainty or establish shutdown. Writers must integrate the real under-mutex call before activation; compilation and fault/concurrency verification remain pending.

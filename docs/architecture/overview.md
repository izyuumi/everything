# Architecture overview

The Rust workspace contains an `everything` library and host binary plus an optional `everything-relay` binary. The command-line binaries currently expose help; no native inference loop is yet wired to them.

Integrated library boundaries are versioned [domain events](../contracts/events.md), [native provider contracts](../contracts/providers.md), [tool registry contracts](../contracts/tools.md), [SQLite persistence](../contracts/storage.md), [canonical Markdown wiki indexing](memory.md), and [cryptographic transport primitives](transport.md). Consult [implementation status](../status/implementation.md) for merge and validation state.

SQLite host storage uses one mutex-protected connection. Wiki indexing uses its own rebuildable derived database. These synchronous modules require blocking boundaries when called from asynchronous execution. Provider and tool interfaces are object-safe async contracts; the host supplies execution context and cancellation. Transport crypto binds pinned device identities and does not supply trust registry persistence or a running relay yet.

GitHub roadmap #30 defines remaining dependency gates. The intended integration direction is clients → host commands → native runtime → providers and broker → storage. Optional clients and infrastructure cannot become authoritative for local execution.

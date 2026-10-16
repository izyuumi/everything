# ADR-0004: Scoped SQLite vector indexing

Status: Accepted for issue #21.

## Context

The wiki's existing FTS5 index must continue working without semantic inference. Semantic retrieval needs fixed dimensions, durable compatible profiles, incremental invalidation, and restart-safe rebuilds. The established `sqlite-vec` extension supplies native cosine search without another service. Its official Rust crate exposes only a raw C initialization symbol. Process-wide auto-extension registration would affect unrelated host connections, while dynamic extension loading would add a native-code path and SQL loading authority.

## Decision

Pin `sqlite-vec = 0.1.9`, upstream C revision `e9f598abfa0c06b328d8fe5da9c3760cce74be10`, under the existing default-off `local-embeddings` feature. Compile its published C source with the crate's build script and use the same bundled SQLite as `rusqlite 0.37`. Initialize only the private derived vector-index connection. Never register a process-global SQLite extension or enable `load_extension`.

Keep canonical Markdown and its existing derived FTS index separate from vector storage. Use fixed-dimension `vec0` tables in bounded shadow generations. Bind each generation to the full validated embedding index specification, exact document-input hashes, and the synchronized wiki-source fingerprint. Persist each processed chunk before advancing its cursor. Switch the active generation in one vector-database transaction after synchronizing canonical files again and comparing the source fingerprint. Preserve FTS retrieval when semantic configuration, compatibility, generation completeness, inference, or native index access cannot support the query.

## Essential unsafe boundary

`vector::initialize_extension` is the only new unsafe Rust boundary. The upstream crate declares its symbol as a parameterless function, while the pinned C implementation is `int sqlite3_vec_init(sqlite3 *, char **, const sqlite3_api_routines *)`. The private bridge converts that function pointer to this exact C ABI and calls it while holding an exclusive mutable borrow of a newly opened `Connection`. No statement, raw handle, or concurrent connection operation is active; the connection remains alive throughout initialization. The extension is statically linked against the same SQLite symbols as the bundled driver. The pinned build script defines `SQLITE_CORE`, so the C implementation does not read the null extension-API pointer.

The error output points to a valid stack pointer initialized to null. If SQLite allocates an error string, the bridge frees it exactly once with that SQLite allocator and returns the structured numeric error without reading or publishing the text. It never closes the handle, retains Rust-owned memory in native state, or exposes a pointer to callers. On failure the partially initialized connection is dropped. This narrow boundary is necessary because the established crate offers no safe per-connection API compatible with the current driver. Its C implementation remains trusted native code, not a memory-safety proof.

## Consequences

Enabled builds require a C compiler and include the pinned native extension. Disabled builds do not include that dependency; runtime-disabled consolidation does not construct the model or open a vector database. Initial vector persistence supports the Linux and Apple little-endian platforms and the four EmbeddingGemma 2 dimensions. Full chunk text is never silently truncated; a backend input-limit failure leaves the incomplete generation inactive and FTS available. Search uses native exact scanning rather than claiming an approximate-nearest-neighbor index.

The vector database is disposable derived state in private host storage, not a backup. A separate database cannot transact with canonical files or the wiki index. One wiki owner serializes internal writes, and final synchronization detects observed external changes, but uncooperative editors can still race the final filesystem scan. Source fingerprints describe the indexed snapshot; they are neither publisher authentication nor an operating-system file compare-and-swap. Automated crash, concurrency, native-boundary and retrieval coverage remains deferred to #32.

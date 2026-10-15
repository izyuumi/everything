# Wiki maintenance contract

`memory::consolidate` provides an asynchronous owner around the synchronous wiki and an injected `Arc<dyn ModelProvider>`. It does not select credentials, instantiate an adapter, fetch evidence, or start itself. `Consolidator::open(wiki, journal_path, provider, config)` receives ownership of the wiki. The journal parent must exist; unrelated databases are rejected. `ConsolidationConfig` requires a model identifier, validated `ContextBudget`, and positive model deadline of at most 300 seconds. Available provider token limits must accommodate that budget. A `None` provider is an explicit inference-disabled configuration.

`Consolidator::open_offline(wiki, journal_path)` opens the same owner and journal without a provider, model identifier, context budget or model deadline. It supports direct evidence retrieval, cited query results without an answer, lint, durable job/cancellation/retry operations and validated host-provided plans. Ingest jobs wait for a model until the owner supplies a host plan or explicitly cancels them. It never constructs an inference backend or invents a selected model. Both constructors use the same journal initialization and recovery path. Provider and model configuration remain fixed for the lifetime of an owner; changing them requires stopping its worker and outstanding operations and dropping all owner clones, then reopening the same wiki/journal with the desired constructor and actual provider configuration.

## Job identity and lifecycle

`enqueue(id, MaintenanceRequest)` persists a request and returns `MaintenanceJob`. IDs are nonempty strings of at most 256 bytes without control characters. Request identity is SHA-256 of the deterministic typed request JSON under namespace `everything.wiki-maintenance.v1`, length-prefixed kind `request`, and length-prefixed JSON bytes; lengths are unsigned 64-bit big-endian. Exact retries return `replayed: true`. A changed request under the same ID returns `JobConflict`. Identical ingest requests under different IDs return the original job ID and outcome; query and lint allow deliberate repetitions under new IDs. Source order participates in identity.

`MaintenanceRequest` is tagged by `kind` and rejects unknown fields:

- `ingest { topic, sources }`: topic at most 256 bytes, one to sixteen sources, total text at most 64 KiB. Each `MemorySource { id, locator, title, text }` has a unique bounded ID, title at most 256 bytes, absolute locator at most 2,048 bytes, and evidence text at most 16 KiB. No source is fetched.
- `query { question }`: one line, at most 4 KiB and 64 terms. It returns lexical evidence and optionally a real model answer.
- `lint { after_path, max_pages }`: exclusive canonical-path cursor and a page limit of 1–64. A result cursor is absent when the scan is exhausted.

`run(shutdown)` admits one background worker per service. The caller should retain and await its future during shutdown. Dropping that future cancels its supervised child; the worker lease remains held until cleanup. Queued jobs remain durable across shutdown. `job(id)` returns phase, original digest, optional output/diagnostic, and cancellation-request state.

Phases are `queued`, `generating`, `prepared`, `completed`, `cancelled`, `conflict`, `interrupted`, `waiting_for_model`, and `failed`. `cancel` cancels queued/unprepared work or signals the active provider/lint operation. `cancel` on `prepared` returns `CommitStarted`; the durable write must reconcile. Completion is idempotent and terminal. `retry` explicitly requeues unprepared incomplete work. On a prepared job it clears the diagnostic and reattempts the same plan, preserving its final Markdown and hashes. It never regenerates that plan. A prepared job with a diagnostic is excluded from normal queue claiming until retry or process startup. Startup marks unfinished generation interrupted and makes prepared plans eligible for one reconciliation attempt.

Job rows and original request digests are retained. The API has no purge or backup operation. At 4,096 retained jobs, 128 unfinished jobs, or the bounded private-journal capacity, new work fails explicitly. Completed and cancelled rows do not count toward the unfinished limit. Hosts must surface capacity errors and define a later retention policy rather than silently erase identity records.

## Plans and managed Markdown

`ConsolidationPlan { version: 1, page_path, expected_hash, title, entries }` updates exactly one page. `entries` contains one to eight `PlannedEntry { heading, summary, source_ids, supersedes }` values. Title and headings are single-line plain text at most 256 bytes. Summary is nonempty text at most 4 KiB; carriage-return line endings are normalized before every line is blockquoted. Each entry cites one to sixteen distinct known source IDs and may supersede at most sixteen distinct, active, offered IDs on that same page. Candidate metadata is validated against freshly read Markdown before preparation. A plan cannot create another page when an existing candidate was offered. `expected_hash: null` requires an absent page; replacement requires the exact offered SHA-256 hash.

The service appends managed sections and preserves earlier bytes except a superseded entry's status line. Metadata is unquoted, starts at column one, and reserves these exact prefixes:

```text
Memory-Entry-ID: <entry SHA-256>
Memory-Status: active
Memory-Source: <source SHA-256> <absolute locator>
Memory-Supersedes: <prior entry SHA-256>
```

A superseded status is `Memory-Status: superseded:<successor SHA-256>`. Every managed entry requires one status and at least one source. Unknown or malformed `Memory-` lines are errors. Code blocks and blockquoted summaries are not metadata. Entry IDs hash the typed tuple `(request_digest, canonical_page_path, entry)` under kind `entry` with the same domain-separated length-prefixed encoding as request identity. Source IDs in metadata hash the full typed `MemorySource` under kind `source`. These digests bind caller-provided evidence; they do not authenticate an external publisher or recover deleted evidence. Keep externally required source documents at their locators.

`apply_host_plan(id, plan)` applies the identical validation and durable-write path without inference. It only accepts queued/waiting/interrupted/conflict/failed ingest jobs, and reports `PlanOrigin::HostProvided`. Already completed jobs return the original outcome. Worker-generated output reports `PlanOrigin::Model { provider, model, response_id }` from the actual completed native provider response. Origins must never be inferred from a host-authored summary.

Before modifying Markdown, preparation commits the final bytes, old/new hashes, entry IDs, and origin in SQLite with synchronous `FULL`. Recovery compares the canonical file to those hashes. Matching new bytes cause synchronization and completion; matching old bytes permit the staged atomic write; any other content is preserved with a prepared-job diagnostic. Filesystem mutation, derived-index update, and journal completion cannot share a transaction. The durable staged plan bridges their interruption window. Internal serialization prevents this service from overwriting its own concurrent jobs; external editors must coordinate with the owner because hash comparison and rename are not a filesystem compare-and-swap.

## Results and citation semantics

`MaintenanceOutput` is tagged by `kind`:

- `ingest`: page path, resulting canonical content hash, generated entry IDs, and origin.
- `query`: optional answer, ordered `WikiCitation` evidence, and optional actual model origin. A citation contains chunk ID, page path, content hash at read time, one-based inclusive source line range, and a UTF-8 excerpt truncated to at most 4 KiB. The line range describes the source chunk, which may be longer than the returned excerpt. Null answers indicate lexical retrieval without an available or useful model answer. Supplied model answers must cite one to sixteen distinct IDs actually retained by `ContextBuilder`.
- `lint`: `LintReport { scanned_pages, scanned_bytes, findings, findings_truncated, next_after_path }`. Read bytes include target pages opened for fragment validation. Findings carry page, one-based line, code, and detail of at most 512 bytes. A maximum of 128 findings is returned. Duplicate managed ID checks span the selected page of files, not previous lint jobs.

Provider generation has no tools and consumes one successful, validated completed response with end-turn or stop-sequence finish reason. Tool blocks, extra fields, unknown citations, incomplete streams, output-limit finishes, oversized responses, and malformed JSON fail without preparing a write. Stream deltas support bounded accounting only; completed content is authoritative. Context budget estimates follow the existing [context contract](../architecture/context.md). Credentials, consent to remote model disclosure, runtime admission, and integration with conversation events remain host responsibilities.

Deferred automated verification is tracked in [#32](https://github.com/izyuumi/everything/issues/32). Live model-generated ingestion/query cannot be claimed verified solely from deterministic file/journal inspection.

A disposable Linux executable using `open_offline` with no provider or model configuration observed real lexical excerpts/provenance, cited query output with no answer or model origin, lint, ingest waiting for a model, a validated host-authored Markdown update, exact replay, queued cancellation and reopen persistence. Syscall tracing of that feature-enabled executable observed no Internet sockets or model/runtime file access. Rust 1.88 formatting, all-target/all-feature compilation and strict Clippy passed. This verifies the constructor and existing local maintenance path; it does not verify the separate host composition or live model-generated maintenance.

`retrieve(question, max_results, cancellation)` returns bounded actual evidence without creating a job or calling an answer-model provider. Optional feature-enabled `set_hybrid_search`, `set_hybrid_search_result`, `rebuild_semantic`, and `semantic_status` attach [hybrid query retrieval](hybrid.md) to this same wiki owner without new configuration fields. Query outputs may include serde-defaulted `retrieval` metadata. Existing saved outcomes remain readable. Ingest candidate selection and lint preserve their existing behavior.

`wait_idle()` acquires the actual single blocking-work admission permit and returns after prior admitted reads, writes and native semantic operations settle. It does not stop `run`, cancel model generation outside that admission, or prevent future calls. A host using this barrier before releasing its state lease must first stop new admissions and await its worker and callers. Dropped blocking callers retain their permit until actual cleanup, so a completed barrier never treats caller cancellation alone as native cleanup proof.

# Headless host composition

`host::runtime::HostService` composes one Agent, Store, state lease, global Tool Broker, tool registry, authorizer, and native scheduler. It serves authenticated owner IPC independently of a graphical application. `HostService::start` prepares and recovers without inference. `run` starts an owned supervisor, activates the scheduler after bindings are ready, and retains ownership if the caller drops its waiting future. A live Tokio runtime remains necessary.

The registry contains the actual native filesystem/shell and scoped scheduler tools. Local discovery uses a base catalog and authorizer without `search_tools`, then registers the discovery tool into the final registry and authorizer. This avoids ownership cycles. Scheduler workers retain per-task discovery bindings and captured tool allowlists. Optional memory, embeddings, MCP startup and System One application composition remain in [#68](https://github.com/izyuumi/everything/issues/68).

## Module boundaries

| Module | Responsibility |
| --- | --- |
| `runtime` | Validated startup, one owned supervisor, local transport, shutdown ownership |
| `commands` | Durable inbox admission, idempotent domain application, scheduler handoff, device command recovery |
| `owner` | Read-only metadata and explicit owner settings, credential, pairing, retry and recovery controls |
| `inbox` | Immutable command identity and first effective snapshots, phase receipts and bounded SQLite recovery pages |
| `settings` | Atomic non-secret settings and revision checks |
| `credentials` | Ephemeral session secrets and scoped native credential operations |
| `providers` | Native provider construction for the exact saved conversation configuration |
| `client` | Typed owner IPC calls and bounded durable command waits |

The host inbox is an admission journal, not an agent execution queue. The scheduler is the sole owner of native task admission, concurrency, execution, cancellation, recovery and one-use owner resume decisions. Host command recovery calls only idempotent Agent acceptance and durable scheduler enqueue. It never invokes a native run directly.

## Command application

The host retains full original domain command JSON before typed decoding can lose additive fields. Same-user legacy CLI text/initialization commands have an explicitly narrower identity. Exact duplicates return their original acceptance and first effective execution snapshot. Domain commands cannot adopt IDs from unrelated Agent event or checkpoint journals. See [durable admission](../contracts/host-inbox.md).

An owner mutation gate serializes fresh paired-origin validation, actual Agent domain application, and durable scheduler enqueue with device revocation. It is released before provider execution. The dispatcher examines the whole bounded pending inbox using keyset cursors, so blocked text commands do not hide later task cancellation or permission decisions. If a page exceeds the inbox read budget, the owned blocking worker halves its requested row count down to one; other failures propagate and the original cursor is retained. Fresh and saved-run execution-policy reconstruction resolve workspace paths on the existing blocking worker, which retains the complete Host owner while inspecting the filesystem. It checks scheduler conversation ownership before accepting a different user turn, including the short interval between a committed final assistant event and task-row settlement.

Domain `SteerTask` checks the immutable task conversation before every delivery or replay, then passes the original host-inbox command ID and acceptance time to the native scheduler. The scheduler recovers the original active-command binding from its durable steering event on replay. Delivery uncertainty leaves the host inbox pending for idempotent recovery; it never grants provider retry authority.

Permission decisions use exact operation lookup including an existing resolution. Replaying an accepted command after a crash therefore reaches the original resolution without scanning retained history or deriving private IDs. The inbox remains pending until the scheduler wake handoff succeeds. A changed native uncertainty boundary still needs its own explicit owner decision.

Device transport and pending remote-command journals also use keyset pages. A page cursor identifies the last scanned original row, including rows that produce no returned operation. Each scan pass handles at most the durable 1024 pending-row capacity and uses bounded pages with read-budget reduction. Cursors are ephemeral scan positions, not synchronization acknowledgement. Terminal host results bridge a crash before encrypted remote result enqueue without reapplying the command. Host archival retains that result while its remote journal remains pending.

The existing Agent text API creates the accepted host user message ID and time. The inbox retains the original client message identity and returns the actual host message ID through its application receipt. Task IDs for root text commands derive from the same command UUID. Existing foreground TaskRecord-format rows remain untouched and are reported as legacy ownership; an owner explicitly stops the host and settles or resumes those through the foreground CLI.

## Optional relay

The default host performs no relay connection. An explicit owner setting selects the exact relay origin. Origin edits persist with `restart_required`; a running session continues using its captured origin until restart. The relay worker uses the actual device endpoint's authenticated encrypted transport and reports its connection state through owner IPC. Its failure leaves local execution available. Enrollment of relay device public identities is an explicit operator action separate from application pairing.

## Shutdown and cleanup

Cancellation closes native execution admission and stops command/device dispatch and the optional relay. Owner IPC remains available while the scheduler cancels and drains its owned workers. Normal new commands, retries, task wakes, and work-enabling controls are rejected. Inspection and explicit broker cleanup controls remain available independently of the stopped scheduler actor; task inspection can use the validated durable read helper.

The supervisor records successful scheduler drain once. A shutdown timeout retains ownership and retries; it does not abort the actor or workers. After scheduler drain, a separately supervised broker recovery operation must acquire every admission permit and exclusive execution access before any exit. Owner IPC remains available while this operation waits or retries a busy broker. This proves dropped caller futures cannot leave supervised preparation or persistence running behind the state lease. The host releases that lease only after this proof, no durable Running tool operation remains, and the actual broker recovery report has zero unresolved cleanup requirements. A restart-required flag can remain set after cleanup is confirmed or explicitly attested; it never reopens admission in a draining host or erases unknown native history.

The post-drain proof and explicit reconciliation operation both exclude live broker admissions and execution, then record stranded Running entries as Unknown cleanup state. It does not retry or acknowledge effects. Owner acknowledgement requires an exact current result digest, operator identifier, and nonempty inspection note. Inspection and owner recovery mutations share a gate so a stale report cannot be combined with a later Running-to-Unknown transition to permit premature exit.

The host never aborts an owner mutation while its blocking worker could still commit under the mutation gate. It joins admitted handlers and retains state ownership during cooperative filesystem/native credential calls. The final socket cleanup remains inode-bound. A forced operating-system termination cannot provide graceful cleanup confirmation; normal startup preserves broker and native uncertainty.

## Validation status

The saved-run workspace offload and adaptive inbox-page corrections have source and Rust 1.88 formatting inspection only. Fresh compilation, strict Clippy, required CI and focused runtime observations remain pending; the earlier observations below do not verify these changes.

The full host/CLI source passed Rust 1.88 all-target/all-feature compilation, strict Clippy, formatting and release build. Actual disposable Linux observations verified singleton rejection, exact durable initialization replay and changed-identity rejection, missing-credential task admission and actionable metadata, host survival after CLI exit, actual synchronized history, recurring deadline rejection, clean shutdown/socket removal and restart preservation. Five real ephemeral device pairings delivered 300 encrypted commands through offline endpoint processing; both journal wrappers paginated 256+44+0 unique commands, and a late cancellation beyond a blocked first page reached the host and cancelled the original managed task. An explicitly configured literal-loopback native OpenAI request observed real connection refusal, exact one-use owner retry, replay without a third attempt, restart without automatic retry, and no credential bytes in persistent state. A separate disposable manually inserted abandoned Running journal verified retained owner IPC/state ownership, post-drain reconciliation, wrong-digest rejection, exact owner attestation, and eventual clean exit. That journal exercise verifies recovery wiring, not live process cleanup. Previous real SQLite admission/settings and standalone Unix IPC checks remain described in their contracts. This document does not claim successful model inference, macOS launchd execution, native credential-store runtime verification, or production readiness. Automated coverage is deferred in [#32](https://github.com/izyuumi/everything/issues/32).

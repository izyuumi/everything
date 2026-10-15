# Scheduling controls

## Caller context

`Scheduler::schedule_context(caller, workspace, operation, tool_name).await` requires a scheduler-owned Running caller with a live active worker, its exact captured canonical workspace, no cancelled ancestor, and the exact Running Broker operation for that task, conversation and tool. It returns `ScheduleCallerContext` with read-only access to the task's immutable `ConversationConfig`, restricted `ExecutionAuthority`, active command and operation identities, tool name and cancellation. The context has no public constructor, serialization or model-provided permission override.

Cancellation is derived from the actual active worker. Task cancellation, host shutdown, worker completion and worker failure invalidate it. A context does not grant capabilities, approve an operation, authorize another provider endpoint or guarantee later admission. The controls service rechecks cancellation and the exact Running Broker record immediately before repository mutation. Cancellation after that boundary may accompany committed effects; it does not establish that no effects occurred.

## Service boundary

`ScheduleControls::new(agent, engine, authorizer)` returns an `Arc<ScheduleControls>` and requires an Agent retaining the actual host state lease. Host composition supplies its existing `ScheduleEngine` and shared `Arc<Authorizer>`. Public calls are:

| Method | Boundary |
| --- | --- |
| `query_as_owner(query, cancellation)` | Trusted owner inspection |
| `mutate_as_owner(mutation, authority, cancellation)` | Trusted owner mutation within the supplied effective authority |
| `query_as_agent(query, caller, cancellation)` | Exact native tool and opaque caller context |
| `mutate_as_agent(mutation, caller, cancellation)` | Exact native tool and opaque caller context |
| `close()` | Stop admitting mutations |
| `shutdown().await` | Close mutation intake and wait for admitted work |

These owner methods do not authenticate an IPC caller. The host must enforce its existing owner-only transport and construct authority from validated owner configuration. They must not be exposed as a remote or model authorization bypass. The supplied mutation authority is explicit for that requested future execution, not a global owner ACL derived from changing host defaults. Create and Update require authority for the requested new target; fresh Run requires authority for the current job target. The trusted owner can inspect existing snapshots, disable and delete them without adopting their workspace or delivery scope. Replaying a manual claim requires the host to derive the supplied authority from that exact immutable occurrence target; the repository still validates it after checking the original claim digest. The controls service does not mint broader authority or derive it from a caller's default workspace.

`ScheduleMutation` supports Create, Update, Disable, Delete and Run. `ScheduleQuery` supports List, Show, History and exact occurrence inspection. Drafts carry the existing typed `ScheduleDefinition`; no credentials are included. Inspection returns immutable run snapshots, actual attempt identities/results, cumulative `AttemptUsage` and the current outcome digest. It does not acknowledge Unknown outcomes or retry uncertain native work.

Agent scope requires FullHost, the exact schedule tool in the inherited allowlist, the caller's unchanged model/provider/endpoint/configuration, its physical workspace, nonescalating execution mode and tool allowlist, and either no result delivery or delivery to that caller's conversation. Both previous and replacement targets are checked for edits. The existing Authorizer validates every requested run capability, including capabilities whose use still needs a later exact approval. Agent inspection filters jobs and immutable run snapshots through the same scope.

Each Update, Disable and Delete first binds its authorized read snapshot to the supplied expected revision; the repository CAS then rejects intervening changes. A fresh manual Run binds that revision too. An existing occurrence retains strict original-claim replay semantics: an agent must retain scope over both current job and immutable run targets, while the repository compares the original manual claim before checking the job's later revision. Trusted owner replay uses the immutable occurrence authority described above and does not adopt the changed current job. CRUD CAS conflicts are not generic command replay. Manual claims preserve their original occurrence, command and native task identities.

Calendar calculations use the existing validated cron, IANA timezone, interval and one-shot engine. Enabling an exhausted one-shot is rejected. An edit preserving schedule and enabled state retains its existing next instant; changed schedules derive their next instant from authoritative host time. Deletion is the repository's soft deletion and preserves history.

All SQLite, calendar and result encoding work runs off Tokio workers. Four admitted operations are allowed concurrently, with a five-second admission wait. Blocking work owns the actual Agent, repository, Authorizer and admission permit until completion, including caller drop. Resource destruction releases Store-bearing handles before the retained Agent and admission permit. A successful mutation wakes the real engine even when the caller has disconnected.

`shutdown` waits up to ten seconds and returns `ShutdownPending` if work still owns resources. The host must retain its services and retry drain. Owner read-only inspection remains available after mutation intake closes. Host composition must separately drain its engine, scheduler and Broker before releasing the shared lease.

## Native tools

`ScheduleTools::new()` creates the registry adapter. `register_into(registry)` registers these names and returns the Authorizer scope rules:

- `schedule_create`
- `schedule_list`
- `schedule_show`
- `schedule_update`
- `schedule_disable`
- `schedule_delete`
- `schedule_run`
- `schedule_history`

The exact list is exported as `SCHEDULE_CONTROL_NAMES`. Every tool has Host scope for service `scheduled_jobs`, including reads because definitions and history can disclose private instructions, workspace paths and run output. This scope requires FullHost and exact Broker approval unless an existing explicit grant applies. ReadOnly callers cannot use these controls.

`ScheduleTools::bind(controls, scheduler)` returns a `ScheduleToolBinding` that the host must retain. Registry entries use Weak references; the binding keeps only a Weak controls reference, avoiding a registry/engine/scheduler retention cycle. Dropping an old binding cannot detach a newer binding.

Create and Update accept schedule, instruction, permissions, budget, policies, enabled state and an optional request to deliver to the current conversation. They derive provider configuration and workspace from the real caller context; caller-supplied provider configuration, foreign workspace and arbitrary delivery destinations are rejected. Defaults retain the existing isolated, zero-tool, bounded scheduled-run policy. Create derives its job UUID from the immutable Broker operation UUID. Run derives both manual occurrence and command UUIDs from that operation. Broker exact replay remains the durable tool-effect replay boundary.

Scheduled targets cannot include scheduler-control or schedule-control tools. Host registration must also exclude `SCHEDULE_CONTROL_NAMES` in both scheduled-policy capture and validation before exposing these tools. Cloud worker profiles remain restricted to their existing explicit filesystem capabilities. Occurrence-family budgeting does not currently authorize recursively creating schedules or delegating remote work.

## Bounds and error semantics

Arguments are limited to 64 KiB. Pages contain at most 100 records. The service shrinks repository pages on its existing storage read budget and stops results before their serialized byte budget: 1 MiB for trusted owner calls and 28 KiB for native tool output, retaining headroom under the native loop's 32 KiB canonical result boundary. Continuation cursors identify the last inspected row; an oversized next row is not silently skipped. A single oversized inspection returns `ReplyTooLarge` and does not truncate its definition or result. A known committed native mutation whose full representation is too large returns an explicit compact applied receipt with stable identities.

Semantic validation, denied scope, pre-admission cancellation and revision conflicts are ordinary errors. Storage or worker failure after mutation admission is conservatively `OutcomeUnknown`; the native adapter reports `EffectsUnknown` with cleanup unproven. A cancelled response does not refund a mutation, invent success, or authorize a new operation UUID. Existing Unknown acknowledgement remains owner-only, bound to the real effective UID and exact outcome digest; none of these tools exposes it.

## Validation

Rust 1.88 formatting, default/all-feature workspace/all-target checks and strict all-feature Clippy passed for the leaf. A release-built disposable inspector exercised all eight adapters through the actual Tool Broker, exact Host approval, ReadOnly denial, schema and run-capability rejection, CRUD/calendar validation, stale and future revision refusal, exact manual replay after job revision advancement, soft deletion, pre-admission cancellation, intake closure, drain and reopen. It preserved all owner-visible jobs across byte-bounded pages, 42 scoped native-visible jobs across ten pages, and 30 immutable runs across three history pages without truncation or cursor loss. An additional owner-policy inspection changes workspace and conversation delivery using explicit new authority, disables and deletes stored targets after owner defaults change, replays immutable manual targets across later workspace edits, and retains agent old-target restrictions and fresh-run authority checks.

The real scheduler held one active task while the inspector called its actual Broker adapters. Explicitly copied, valid Running operation records were used only to inspect context identity, operation-state revalidation and active-worker lifetime; they were not model-generated calls or successful tool rounds. Releasing the task's provider-construction gate produced one actual native loopback connection refusal, preserved its request/output reservation and invalidated the retained caller context. The schedule engine remained inactive, so manual runs established durable claims, exact replay and overlap skips rather than completed scheduled inference. Broker drain reported zero Running and zero unresolved operations. Raw driver, source/binary hashes and observations are retained in `/tmp/everything-schedule-control-leaf-inspection` for the implementation handoff.

Owner IPC, CLI registration, exact profile exclusions and host composition are coordinator-owned integration work; this module alone does not verify those flows. Cancellation after transaction admission, fault injection and simultaneous owner-edit races have source review and CAS reasoning rather than exhaustive runtime verification.

No successful live inference, native model-generated tool calls, production readiness or automated race/crash coverage is claimed. Automated coverage is deferred to [#32](https://github.com/izyuumi/everything/issues/32).

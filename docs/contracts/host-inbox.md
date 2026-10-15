# Durable host command admission

`host::inbox::HostInbox` stores accepted commands in SQLite migration 7. It does not execute inference or own a task queue. The native scheduler owns execution; the host repairs the gap between durable admission, domain application, and scheduler enqueue.

## Identity and authority

`InboxInput::Domain` retains the full original command JSON, including additive fields unknown to the current typed domain contract. `InboxInput::Owner` accepts the narrower same-user `OwnerCommand::Text` and `Initialize` identities used by the existing CLI. Its original JSON includes explicit requested execution options. These owner variants must never be accepted from a paired device.

The request digest covers input kind, full original JSON, and immutable authenticated origin. A paired origin retains the exact pinned public registration. New domain admissions reject IDs already present in Agent event or private checkpoint journals. The journal does not reserve IDs inside either Agent table. An explicit owner text variant may adopt a matching legacy conversation/command/text only while no scheduler-incompatible foreground task row exists. The dispatcher preserves any existing native authority and configuration rather than widening it through new options. Existing foreground TaskRecord-format rows remain unchanged and are reported as legacy foreground ownership. The owner can explicitly stop the host, resume or settle that foreground task, and then return to the host; admission never rewrites private scheduler definitions or automatically stops tasks.

Lookup and exact identity comparison precede capacity checks and effective default computation. A duplicate reuses its original acceptance time and first effective options snapshot. Explicit changed options or additive metadata under the same command ID conflict. Client timestamps never determine scheduling order or deadlines.

Remote admission and application recheck local endpoint revocation, active pinned sender registration, and the existing authenticated pending remote-command journal with its full original-envelope digest. The host must also preserve `DeviceEndpoint` command completion and reply acknowledgement semantics. An inbox record alone cannot authorize a revoked device or reexecute an already terminal remote command.

## Phases

| Phase | Meaning |
| --- | --- |
| `accepted` | The bounded original input and immutable first effective options are committed |
| `applied` | Stable domain IDs are recorded after idempotent application and durable scheduler enqueue where required |
| `completed` | Command application has a durable immutable result; this does not imply an agent task has finished |
| `rejected` | Application has a durable immutable rejection |

`CommandAccepted` IPC is valid after the reservation transaction commits. Client disconnect after that point does not cancel admitted work. Recovery reapplies only idempotent Agent acceptance and enqueue operations; it must not start another native run directly. `AppliedReceipt` maps to actual conversation, task, message, and operation IDs. Original client user message identity remains in the envelope; the existing Agent text acceptance API creates the accepted host message ID and timestamp, which the receipt identifies explicitly.

`mark_applied` and terminal `finish` compare immutable receipts/results on replay. Terminal result digests survive archival. After seven days, archival may remove original input, options, receipt, and result while preserving command identity, origin, acceptance time, and digests. There is no command ID reuse through archival.

## Bounds and recovery enumeration

Original JSON is at most 256 KiB; effective options and terminal results are each at most 64 KiB; origins are at most 16 KiB; applied receipts are at most 4096 bytes. Admission permits at most 1024 pending commands and 100,000 retained identities. The 128-MiB retained JSON budget reserves room for pending receipts and terminal results. Capacity failures leave existing identities intact.

`pending_after(cursor, limit)` returns 1–256 records ordered by immutable `(accepted_at, command_id)`. `PendingCursor` supports draining past blocked commands instead of repeatedly selecting the same oldest prefix. One returned page is additionally bounded to 16 MiB. Register a host wake listener before scanning; startup must scan durable pending records. Notifications are hints and are not a substitute for SQLite state.

All APIs are synchronous SQLite operations. Async callers use bounded blocking workers and retain state ownership until those workers finish. Effective options are an opaque validated JSON persistence boundary; the host must decode them into the actual captured scheduler policy before application.

## Observed validation

A disposable executable used the actual Agent, Store, host inbox, settings repository, and crypto device endpoint. It observed real schema-6-to-7 migration and integrity check, original additive metadata preservation, changed-input conflicts, exact replay retaining the first acceptance/options, immutable applied/completed results, legacy owner adoption versus full-domain namespace rejection, private checkpoint namespace rejection, 301 pending records paginated as 256 then 45 without overlap, persisted restart recovery, and revoked paired-device application rejection. No model request was made. These observations do not verify the unfinished host dispatcher, scheduler composition, or cross-platform service lifecycle. Deferred automated assertions remain in [#32](https://github.com/izyuumi/everything/issues/32).

Native steering application preserves the inbox command ID as the delivery ID and the original host acceptance time. It checks the target task conversation even on terminal replay, then uses the scheduler’s original event binding. Mutation uncertainty retains Accepted state for idempotent recovery rather than manufacturing a terminal rejection. A completed steering receipt identifies durable input delivery, not an additional provider attempt or completion of the target task.

## Explicit discard of unapplied queued work

The same-user owner operation `discard_unapplied` carries the original command ID, exact expected acceptance time, operator identifier and reason. Its IPC request ID becomes the immutable discard request identity. The host holds its mutation gate and rejects this action during drain. `HostInbox::discard_unapplied` performs one immediate transaction over the original record and application markers.

Only Accepted text, SendMessage, root CreateTask and initialization inputs are eligible. Applied commands and all controls, permission decisions, cancellation and steering are excluded. Any public Agent command journal, private checkpoint command journal or derived root task admission prevents discard. Initialization also requires the absence of its conversation row. The action never clears another command's retained native pending turn, cancels an admitted task, or erases original identity/origin.

A successful action records Rejected with the exact operator/reason/request ID, original request digest and acceptance time. The same request returns its original receipt; changed intent or request identity conflicts. Terminal identity digests keep this check available after payload archival. Original command replay remains the same durable rejection and cannot create a task or inference. Dispatcher and receipt notifications let normal paired-device result recovery deliver the rejection. The submitting paired device can receive the recorded rejection reason and operator metadata; choose those fields accordingly.

Use this action for known queued IDs when a cancelled conversation retains its prior pending turn. It frees pending capacity without treating an uncertain or partially applied command as cancelled. No command is discarded automatically.

Rust 1.88 format, all-target/all-feature compilation and strict Clippy passed. A disposable actual host and Store walkthrough filled 1024 pending reservations behind a managed task, observed overflow rejection, discarded one through real owner IPC, preserved exact replay and rejected changed intent, admitted one replacement, and preserved the terminal result after reopen. Real accepted-user/private-checkpoint markers, a control input and an initialization with an existing conversation were refused. The accepted-user guard was exercised before any TaskRow admission. No provider inference or tool execution succeeded in this walkthrough.

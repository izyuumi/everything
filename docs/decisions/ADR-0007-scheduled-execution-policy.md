# ADR-0007: Scheduled execution policy and terminal delivery

## Status

Accepted and integrated through #103/#105 after exact-source Linux/macOS Rust and Rust 1.88 CI. Historical focused walkthroughs remain qualified in the [execution contract](../contracts/scheduled-execution.md#focused-current-main-validation); current-source runtime and successful inference remain unverified. This follows [ADR-0005](ADR-0005-scheduling-calendar-and-admission.md); original schema-eight serialized policies retain their conservative allocation.

## Context

A durable occurrence must retain its captured authority and deadline across restart without turning uncertain provider or tool effects into automatic paid retries. Optional publication must not inject scheduled output into an active user turn. An Unknown outcome cannot safely release default overlap merely because local workers stopped.

## Decision

Use the existing native Scheduler and Tool Broker. Reserve each serial attempt and its exact policy before Agent admission. The first attempt uses the occurrence's original TaskId; later attempts use distinct identities within the same immutable occurrence. All attempts share original claim-time timeout and cumulative native-request, tool and reserved-output ceilings. Full output maxima are charged before ProviderInFlight and never refunded. Existing runtime-control tools stay excluded until aggregate child budgets exist.

Automatic retry requires the actual actor's typed ProviderUnavailable retirement proof: exact inactive task and command, no consumed or pending controls, no request, no round, zero native counters and no broker operations. Persist retirement identity before calling that proof and settlement/backoff before reserving a successor. Generic failure, cancellation and uncertainty cannot authorize retry. A distinct never-paid attempt may capture fresh local inputs; an active native resume retains its exact Definition. Explicit classifier approval does not transfer to a new task.

Capture the original task's native counters and phase in its terminal checkpoint before conversation reuse; bind them to the original task, command, configuration and policy. Use that durable evidence for settlement. Legacy unavailable accounting consumes full conservative allowances with known=false; unrelated current summaries do not change original outcomes. Preserve the separate exact unused-provider retirement proof for automatic zero-effect retry.

Check the original durable native phase, typed waiting reason and immutable Running/Unknown tool journal independently before classifying any terminal result. Tool cleanup acknowledgement is separate from knowing an effect outcome. Default overlap admits one Claimed, Running or unacknowledged Unknown occurrence across all generations and manual triggers; explicit Allow is bounded to sixteen. Keep Unknown outcomes immutable and occupying overlap. A trusted owner may append an acknowledgement bound to the exact occurrence, complete outcome digest, command identity, effective UID, note and time. That acknowledgement changes only scheduling overlap admission. It does not erase history, resume work, clear broker quarantine or certify an external effect stopped.

Reserve optional conversation delivery before effects and make its full bounded notification ready atomically with terminal settlement. Publish one append-only ScheduledRunFinished event only while the destination has no pending turn. Compare the complete original request on replay before checking current busy state. Commit event, command journal and unchanged conversation checkpoint together; acknowledge the outbox against that complete journal. No message, model turn, native checkpoint or configuration changes during publication.

## Consequences

Factory retries are bounded and fail closed, while paid retries remain explicit owner decisions. Unknown can block future scheduling until owner acknowledgement. Busy or missing destinations retain bounded pending delivery with visible diagnostics. Capacity-blocked legacy backfill keeps its preparation diagnostic while existing ready deliveries drain; only actual backfill progress clears it. Attempt, policy, result, acknowledgement and delivery quotas can block new admission; retained history is not silently pruned. The implemented [owner controls and native tools](../contracts/scheduling-controls.md) share the leased host, Authorizer, Scheduler and global Broker. The [CLI](../development/scheduling-cli.md) preserves exact manual-claim and acknowledgement identities; the optional [status view](../development/schedule-status.md) is read-only. These surfaces do not grant credentials, acknowledge uncertainty automatically or govern host lifetime. No second execution service or backup subsystem is introduced. Automation for crash, race and resource boundaries remains deferred to issue #32; local walkthroughs do not establish production readiness.

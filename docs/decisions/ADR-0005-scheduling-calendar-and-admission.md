# ADR-0005: Scheduling calendar and durable native admission

Status: Accepted and integrated through #97/#103/#105 after exact-source Rust CI. Historical static and disposable observations remain qualified in the [engine contract](../contracts/scheduling-engine.md). [ADR-0007](ADR-0007-scheduled-execution-policy.md) records the implemented schema-nine follow-on for serial attempts, exact output reservation, owner uncertainty acknowledgement and terminal delivery. Successful inference and current-source lifecycle/runtime acceptance are not implied.

## Context

The host already has durable task execution and an immutable schedule repository. A timer must handle local calendar schedules, process restarts, clock changes and uncertainty without creating a second runtime or repeating external work. Library gap/fold heuristics can differ across fixed and wildcard cron patterns. A claim and native command admission also use distinct SQLite transactions.

## Decision

Use established Croner 4 calendar search and chrono-tz mappings. Skip nonexistent local times and choose the earliest UTC mapping of repeated local times uniformly. Once and anchored intervals use UTC millisecond arithmetic. Treat calculator bounds and unsupported cron years as explicit blocked errors.

Use one host timer with Tokio sleeps, a fixed one-second realtime audit and edit/native-event hints. Eligibility always uses sampled realtime. Allow five seconds of normal lateness; captured Latest may additionally allow one most recent occurrence within its window. Atomically collapse older missed prefixes into terminal missed-range history and advance the due pointer; do not backfill one row per missed second.

Reserve the first attempt's isolated conversation, creation/input commands, native task, accepted timestamp and captured execution policy before Agent admission. Reuse those exact identities through every crash/retry of admission. Run the actual existing native scheduler. Preserve the original occurrence deadline through queue/offline/approval waiting and enforce it at the native provider boundary. Do not automatically retry uncertain provider or tool effects.

Filter native child-task and schedule controls until aggregate family budgets exist. The original schema-8 admission conservatively allocated output by limiting request count to the captured total-output ceiling divided by per-response maximum. Preserve those serialized policies unchanged. New schema-9 policies use the exact remaining output-reservation ceiling under ADR-0007. Keep Unknown outcomes occupied for overlap until the implemented exact durable owner acknowledgement; never rewrite immutable outcomes to clear uncertainty.

The defaults have these specific meanings:

| Boundary | Decision |
| --- | --- |
| Missing DST wall time | Skip that nominal cron occurrence; do not shift it to a different wall time |
| Repeated DST wall time | Use the earlier UTC mapping once, including wildcard patterns |
| Restart or late wake | Dispatch only the most recent occurrence within five seconds, or the explicit Latest window; collapse an older prefix into one missed-range outcome |
| Overlap | Default one active or unacknowledged Unknown occurrence across generations and manual triggers; explicit Allow is bounded to 1–16 |

The one-second realtime audit and monotonic/native execution guards address different clock boundaries. They do not establish hard realtime dispatch or detection of unobserved clock movement during downtime.

## Consequences

Cron schedules use an explicit timezone and deterministic DST behavior. The clock audit has bounded adjustment latency and small periodic read overhead. There is no promise of hard realtime dispatch or interruption inside one synchronous library call. Missed-range history is summarized rather than pretending every omitted occurrence has an individual record.

Admission survives partial conversation creation, command acceptance and enqueue using the same durable first-attempt intent. The engine has a recover-only startup and explicit activation boundary. Native execution, credentials, permissions and cleanup remain centralized. The implemented follow-on in ADR-0007 supplies bounded factory-safe retries, cumulative output reservation, exact owner acknowledgement and idle-only result delivery without replacing these original admission identities. [Owner controls](../contracts/scheduling-controls.md), the [CLI](../development/scheduling-cli.md) and optional [client status](../development/schedule-status.md) use those contracts; Apple runtime acceptance remains unverified.

Automated calendar, race, migration, crash and budget coverage is deferred to #32. Static and disposable manual validation do not establish successful live inference or a security audit.

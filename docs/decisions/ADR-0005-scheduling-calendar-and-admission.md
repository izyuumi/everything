# ADR-0005: Scheduling calendar and durable native admission

Status: Accepted and implemented for issue #51; local static and disposable manual validation recorded in the [engine contract](../contracts/scheduling-engine.md).

## Context

The host already has durable task execution and an immutable schedule repository. A timer must handle local calendar schedules, process restarts, clock changes and uncertainty without creating a second runtime or repeating external work. Library gap/fold heuristics can differ across fixed and wildcard cron patterns. A claim and native command admission also use distinct SQLite transactions.

## Decision

Use established Croner 4 calendar search and chrono-tz mappings. Skip nonexistent local times and choose the earliest UTC mapping of repeated local times uniformly. Once and anchored intervals use UTC millisecond arithmetic. Treat calculator bounds and unsupported cron years as explicit blocked errors.

Use one host timer with Tokio sleeps, a fixed one-second realtime audit and edit/native-event hints. Eligibility always uses sampled realtime. Allow five seconds of normal lateness; captured Latest may additionally allow one most recent occurrence within its window. Atomically collapse older missed prefixes into terminal missed-range history and advance the due pointer; do not backfill one row per missed second.

Reserve the first attempt's isolated conversation, creation/input commands, native task, accepted timestamp and captured execution policy before Agent admission. Reuse those exact identities through every crash/retry of admission. Run the actual existing native scheduler. Preserve the original occurrence deadline through queue/offline/approval waiting and enforce it at the native provider boundary. Do not automatically retry uncertain provider or tool effects.

Filter native child-task controls until aggregate family budgets exist. Conservatively allocate initial output by limiting request count to the captured total-output ceiling divided by per-response maximum. Keep Unknown outcomes occupied for overlap until a future explicit durable owner acknowledgement; never rewrite immutable outcomes to clear uncertainty.

## Consequences

Cron schedules use an explicit timezone and deterministic DST behavior. The clock audit has bounded adjustment latency and small periodic read overhead. There is no promise of hard realtime dispatch or interruption inside one synchronous library call. Missed-range history is summarized rather than pretending every omitted occurrence has an individual record.

Admission survives partial conversation creation, command acceptance and enqueue using the same durable first-attempt intent. The engine has a recover-only startup and explicit activation boundary. Native execution, credentials, permissions and cleanup remain centralized. Exact retries, aggregate output reservation, owner uncertainty acknowledgement, result delivery and client operations remain later scoped work; no placeholder implementation is supplied for them.

Automated calendar, race, migration, crash and budget coverage is deferred to #32. Static and disposable manual validation do not establish successful live inference or a security audit.

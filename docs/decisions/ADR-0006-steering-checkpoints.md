# ADR-0006: Native steering checkpoints and explicit child waits

Status: accepted for the Agent delivery primitive and Scheduler composition; production Host wiring remains separate work.

## Context

The native kernel retains a captured conversation while model and tool futures run. Appending steering concurrently changes the event head and checkpoint revision, so an ordinary CAS commit would reject a valid completed response after a potentially paid request. Replacing the immutable task definition would also change context bindings, authority or budgets. Child completion must not reopen a terminal parent or hold a broker permit while waiting.

## Decision

Store typed bounded inputs and a consumed cursor in the existing private conversation checkpoint, outside the native definition. Public acceptance events provide exact replay identity after consumption. Use one short shared per-conversation write gate for acceptance and native commits. Merge only provable input-acceptance deltas; reject all other concurrent mutation. Rebuild unsent requests when input arrived during context preparation, and decide final completion under that same gate.

Consume inputs as additional User history only after complete known tool-result rounds. Preserve the original command, constraints, context binding, opaque continuation, limits and counters. Waiting for children is explicit and bounded. A wait yields the native stream after the broker operation has returned; trusted scheduler delivery identifies the exact parent command, wait and immutable child terminal record. Unknown child outcomes remain a typed owner stop.

## Consequences

No additional SQL inbox or Agent dependency on Host or Scheduler is required. Accepted input survives restart and conflicts cannot trigger a blind retry of inference or external effects. A turn may commit an interim final assistant response and continue with accepted steering under the same budget. Completed parents remain terminal. The host and scheduler must repair durable admission-to-delivery handoffs and preserve uncertainty when delivering child outcomes. Full crash and streaming automation remains deferred to #32.

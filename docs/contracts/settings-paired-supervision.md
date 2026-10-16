# Paired Settings supervision

The private paired job slice extends the existing Settings supervisor in `src/host/settings/agent.rs`. It is not connected to accepted relay delivery. Reply projection, permitted encryption/publication, Original terminal bookkeeping and reservation cleanup remain integration prerequisites. The slice exposes no result-release or successful-cleanup substitute.

## Intake and bounds

`SettingsAgentRuntime::try_enqueue_paired` synchronously transfers one actual owned `PairedRuntimeInput`. Bind carries `OwnedCommittedSettingsBind`; DirectUpdate carries `OwnedCommittedSettingsCommand` and an actual caller-owned cancellation token; Historical carries `OwnedHistoricalSettingsCommand`. The heavy requests are boxed, never reconstructed from public IDs. These private types and the enqueue errors are not serialized or logged.

Native requests and paired inputs share the original capacity-eight `mpsc` channel. A short mutex protects paired admission, closure and the lifetime accepted count; no await, SQL, file or service work runs under that gate. Full and Closed return the exact unconsumed input. Successful enqueue records admission and clears quiescence before transferring ownership into the channel. The caller must retain a returned input for genuine downstream handling; a queue error is not an operation outcome.

The existing transport ownership limits remain applicable: 32 private owners globally and four per peer, with its existing byte accounting. This supervisor slice additionally retains at most 32 accepted paired inputs for its entire lifetime. It does not recycle that allowance while the actual result and cleanup successor is absent. This is a bounded retention policy, not a process-memory or execution-time guarantee.

## Registration and real joins

The same existing `JoinSet` now returns native turns or paired results. Before spawning a paired service future, the supervisor stores its kind and the actual original creation/invocation identity. The synchronous spawn is associated with its real Tokio task ID before another supervisor iteration. The supervisor retains the same heavy `SettingsAgentComponents`, Agent and state-lease ownership outside the job.

Bind invokes the existing `SettingsService::install_paired_parent`. DirectUpdate invokes the existing `SettingsService::update_paired`, using a child of the supplied cancellation token. The outer service futures are registered directly in the existing task set. Normal stop does not abort, timeout or select-drop them. Their actual service implementations await the blocking handles they started; a cancellation request does not prove those handles joined.

Historical inputs are retained directly and never enter an effect method. Paired completions never run native turn cleanup. The native single-turn semaphore, seven tools, profile/fence handling and native-only another-turn check remain in place.

## Returned ownership

A genuinely joined result must match its registered Tokio task ID, kind and original creation/invocation identity. Bind admission is consumed once into the actual parent result and original request. Update outcome is consumed once into the actual service inspection/error and original request. Returned inspections retain their actual phase; they are not mapped to Applied or successful delivery.

Named result records place the service result and cleanup errors before the original request, preserving request ownership through result destruction. If Update extraction returns the boxed outcome because its original Arc is not unique, the supervisor retains that entire box and latches a terminal fault. It does not retry extraction or repeat the effect. Unmatched outputs and actual worker errors are retained as failure evidence.

No output consumer is exposed in this slice. Joined Bind/update results, historical inputs and inputs observed after drain stay privately owned. Consequently any accepted paired input keeps runtime quiescence false, including after every real worker has joined. A counter reaching zero, disappearance of a network waiter or an actual transport ACK cannot substitute for the missing cleanup consumer.

## Stop and unexpected loss

Normal drain closes paired intake, cancels original derived Update tokens and closes actual returned Bind parents. A Bind that finishes during drain is closed when its real result is adopted. Each parent-close attempt is made once; a real error is retained in its result record and makes paired failure permanent. The request/result owners remain retained. No parent-slot reclamation or Original/reservation cleanup is implied.

A wrapper panic, unmatched completion, ownership invariant failure or poisoned intake latches the monotonic paired fault. Native Settings is quarantined through the existing failure path. No paired input is dispatched after a draining or failed state is observed. The runtime never reports quiescence while accepted paired ownership or this fault remains.

If the whole supervisor future is dropped after paired admission, its Drop path first closes intake and marks permanent nonclean state. It retains the actual task set, queued receiver, result/input records, actual join errors, native session owners and outside Core/Agent lease in a bounded terminal allocation. The task set is not dropped or aborted by that path. There is no rescue task, automatic retry, timeout or clean-recovery claim. An unwinding job may already have consumed or destroyed its request; retained correlation and the outside lease do not prove that request survived or that a detached inner handle joined. Process-wide runtime loss also cannot manufacture such proof.

## Validation and integration limits

This source slice requires coordinator review and current compiler/static CI after genuine consumers are integrated. Standalone Rust 1.88 formatting and source-preservation checks do not verify types, runtime joins, relay acceptance, encrypted publication, populated storage, provider behavior or security. Accepted relay delivery must remain disabled until actual result projection, publication and cleanup ownership are reviewed and integrated. Deferred automated coverage remains tracked by issue #32; issue #116 remains open.

# Paired Settings supervision

The private paired job slice extends the existing Settings supervisor in `src/host/settings/agent.rs`, with affine result custody in its private `agent/handoff.rs` child. It is held and not connected to accepted relay delivery. The real background pump, registered started build, encryption/publication, Original terminal bookkeeping, parent retirement and reservation cleanup remain integration prerequisites. No successful-cleanup or result-release substitute is provided.

## Intake and bounds

`SettingsAgentRuntime::try_enqueue_paired` synchronously transfers one actual owned `PairedRuntimeInput` and returns one `PendingPairedResult`. Bind carries `OwnedCommittedSettingsBind`; DirectUpdate carries `OwnedCommittedSettingsCommand` and an actual caller-owned cancellation token; Historical carries `OwnedHistoricalSettingsCommand`. The heavy requests are boxed, never reconstructed from public IDs. These private types and the enqueue errors are not serialized or logged.

Native requests, paired inputs and lean adoption controls share the original capacity-eight `mpsc` channel. A short mutex protects closure, a checked monotonically increasing `u64` next-record allocator and a separate live count capped at32. Channel capacity is reserved before accepting an input. ID exhaustion closes intake and faults permanently while returning the unchanged input; IDs never wrap or reuse. Full and Closed return the exact unconsumed input. Successful enqueue records the live owner and clears quiescence before transferring the input, its completion sender and fresh handoff correlation into the original channel. No await, SQL, file or service work runs under the intake gate.

The existing receive limits remain32 private owners globally, four per peer and the same byte accounting. Live paired records are not decremented or retired in this slice, including after custody adoption. Consequently the effective accepted allowance remains32 until genuine cleanup exists. No counter, task completion, dropped waiter, channel acknowledgment or network acknowledgment refunds private accounting or proves cleanup.

## Registration and real joins

The same existing `JoinSet` returns native turns or paired results. Before spawning a paired service future, the supervisor stores its kind, actual original creation/invocation identity, fresh handoff Arc and one completion sender in its existing `paired_records` map. The synchronous spawn is associated with its real Tokio task ID before another supervisor iteration. The supervisor retains its outside heavy `SettingsAgentComponents`, Agent and state lease.

Bind invokes the existing `SettingsService::install_paired_parent`. DirectUpdate invokes the existing `SettingsService::update_paired`, using a child of the supplied cancellation token. The outer service futures are registered directly in the existing task set. Normal stop does not abort, timeout or select-drop them. Their actual service implementations await the blocking handles they started; a cancellation request does not prove those handles joined. Historical inputs never enter an effect method and can be offered only through their actual retained identity.

A genuinely joined Bind/Update result must match its registered Tokio task ID, kind and original creation/invocation identity before handoff. The actual service-owned `PairedParentAdmission` now declares and returns its parent result before its charged request. Named retained records likewise put service results and close errors before the original request. Update extraction still retains the complete boxed `PairedUpdateOutcome` and faults if its actual original Arc is not unique; it does not repeat the effect or reconstruct an owner.

Paired completions never run native turn cleanup. The native single-turn semaphore, seven tools, profile/fence handling, provider path, session owners and native-only another-turn check remain in place.

## Affine result custody

Only a verified joined Bind/Update or the actual nonexecutable Historical record can create `PairedResultHandoff`. Its private Ready payload stores actual service-result/close metadata and record correlation before the existing `SettingsReplyBuildRequest`. Successful Bind owns the actual installed parent first and real boxed committed request last. Update retains its actual inspection or structured error; Historical retains its original request. An actual failed Bind parent is a separate full error/request payload and cannot become Bound or a build request.

The single completion sender offers the whole noncloneable, nonserialized handoff to its one pending receiver. Send failure retains the exact returned handoff in that same Supervisor record. A receiver or waiting future can disappear without releasing the record, Source or private charge. If a delivered handoff is dropped, its Drop latches the monotonic paired fault and retains the complete actual payload, correlation and typed abandonment cause in a bounded terminal allocation. It does not merely retain a flag or abort a worker. At most one such heavy owner exists for each of the at most32 live records.

The lean correlation contains the actual record index, original creation/invocation Arc, fresh handoff Arc and original intake Arc. It contains no Source, Core, Agent, HostInner or callback. Integer lookup alone supplies no permission. The payload's request identity is checked against this original Arc before borrowing for adoption; existing intake closure/fault is observed before and under its short gate. These are custody checks, not current Settings authorization or renewed clocks.

## Same Work adoption

The original connection owner delegates `try_adopt_paired_result` to its same receive Work and actual connection identity. Work borrows the handoff's existing build enum and reserves its existing single slot using the unchanged final witness, Source context, connection, creation/stamp association and once-ever claim checks. Rejected Bind, Closed, Busy, AlreadyClaimed and failed bookkeeping retain the entire same handoff in the real failure aggregate. No Source, result or parent is detached for a fallible reservation.

Success stores the whole handoff in the existing full Work record, with the original noncloneable permit last. It does not clone capabilities, create another map/queue/runtime or project bytes. Work alone constructs an opaque `PairedWorkAdoption` from an actually installed full record. Its synchronous acknowledgment uses the same mpsc8; Full and Closed return that same receipt for retention in the record. A receipt for another actual intake is refused before enqueue. The Supervisor requires its exact offered record, original creation Arc, fresh handoff Arc and original intake Arc before marking WorkOwned. The receipt is custody acknowledgment only; it neither retires a live record nor asserts current authority, parent closure, delivery or operation success.

Existing memory-only unstarted withdrawal refuses a supervised record before taking it, clearing the slot or dropping any owner. The full record and actual permit remain retained and nonclean. A later reviewed genuine cleanup consumer is required before such ownership can be retired; there is no started-build release or finish(boolean) API here. Dropping the pending ticket or a lean adoption receipt cannot make either owner clean.

## Stop and unexpected loss

Normal drain closes paired intake, cancels original derived Update tokens and closes real Bind parents still held directly by the Supervisor. A Bind that finishes while draining remains there and receives that same close attempt. Once the parent has entered an offered, retained handoff or WorkOwned record, the Supervisor has no direct parent handle to close. It reports that Bind drain as nonclean and faults; forwarding or dropping a handle is not a close operation. The original connection cancellation, parent lifetime and later Core current checks remain authoritative; no deadline is renewed.

A wrapper panic, unmatched completion/control, ownership invariant failure or poisoned intake latches permanent paired failure. Original service errors and complete ownership-invariant outcomes remain retained. An unwinding job may already have destroyed its request; outside correlation and the Core lease do not prove that request survived or that an inner handle joined. No further paired input is dispatched after observed drain/failure.

Whole Supervisor Drop after paired admission closes intake, marks permanent nonclean state and retains its actual task set, queued receiver, records/completion senders, unmatched inputs/results/adoptions, actual join errors, native session owners and outside Core/Agent lease in the existing terminal allocation. Whole Work Drop retains its actual adopted handoff, original permit, task set and context in its existing terminal allocation. A handoff outside those owners has its own affine retention path. There is no rescue task, retry, timeout, live-count decrement, automatic parent retirement or clean-recovery claim.

## Validation and integration limits

The concrete enqueue/result/adoption definitions are held pending Root and independent review and actual integration consumers. Standalone Rust1.88 formatting/parsing and exact source-preservation checks do not verify compilation, runtime joins, relay acceptance, encryption, populated storage, providers or security. Supported strict exact-head CI and actual disposable observations remain pending. Accepted relay delivery stays disabled until the real pump, current projection, publication, started-build lifecycle and genuine cleanup are integrated. No source comments or automated tests are added; deferred automation remains #32 and issue #116 remains open.

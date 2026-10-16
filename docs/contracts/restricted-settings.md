# Private Settings runtime contracts

These contracts describe the #113 implementation merged in PR #143 after exact-tree Linux/macOS compilation/static/release and Rust1.88 checks passed. Disposable behavior verification remains pending; later integration heads require their own checks. The existing [typed Settings contract](settings.md) and its native action/confirmation authority remain authoritative.

## Private native frames

AgentOpen authenticates an existing native proof and creates an actual manager-owned child. Its reply contains a nonnil chat ID, provider, exact model/recipient, binding SHA-256, ordered seven provider tool names and original expiry. It contains no credential, private proof, provider continuation or ordinary conversation identity.

AgentSend carries that proof, exact chat ID, original turn CommandId, explicit accepted binding digest and UTF-8 text. Raw text is limited to 8 KiB; the existing complete encoded private input limit is 32 KiB, so escaping can require a smaller input. Changed reuse of a turn ID conflicts. Exact replay returns original acceptance without appending input or sending another request. The manager permits one Settings turn across at most two live/closing chats and eight turns per chat.

AgentPoll and AgentClose authenticate the retained native cleanup proof, including expired-session cleanup where supported by the original Authority. They must match the manager's original native parent. Replies contain a bounded status snapshot, at most 16 KiB of completed assistant text, up to eight unique authoritative operation IDs and original conservative counters. They expose no raw private journal or continuation. The actual LocalResponse validation checks request/result correlation and the complete encoded 2 MiB wrapper. Oversized data is rejected; opaque state and completed text are not truncated to fit.

Closing a child preserves its native parent and siblings. Closing the native parent cancels its owned children through the existing Authority. No native client controls Host lifetime.

## Sealed execution and storage

SettingsProfile, SettingsToolCapability and the internal authority constructor are crate-private and cannot be minted by deserializing an execution mode, tool ID or UUID. The actual Broker chooses the selected RAM journal, shares global permits/exclusion/quarantine, and passes the sealed context only to registered private entries. General contexts cannot execute those entries; private contexts cannot execute General entries.

The RAM Store configures 4096-byte pages, an 8192-page hard cap, memory journal/temp storage and disabled cache spill before sensitive insertion, then verifies the fileless connection and these settings on capacity admission. Two stores bound main database pages to 64 MiB. This is not a process-RSS bound: journals, caches, bounded Rust values, native parsers and dependencies consume additional memory.

The following encoded limits are enforced in source: response 512 KiB, continuation state 256 KiB, arguments 16 KiB/call, event 192 KiB, append 512 KiB, checkpoint 2 MiB and Broker request/record 64 KiB each. Assistant text is bounded while provisional deltas are accumulated and again before accepted response mapping. Four calls/round, eight calls/turn and parallelism one are immutable ceilings. The native adapters retain their existing 32 MiB response and 64–128 MiB parser ceilings.

Admission checks the actual serialized composite checkpoint, including the duplicated continuation binding, then quotes future result/replay storage and verified SQLite page headroom before payment or tool effects. Metadata outcome slots are preallocated outside SQLite, bounded and reserved before admission. A page/serialization failure never authorizes the old ProviderInFlight checkpoint to resume. Numerical bounds and candidate runtime observations still require combined validation.

## Durable fence semantics

A session reservation is immutable and nonsecret. At most 1024 reservations and eight original turns/reservation are retained; unresolved state is never evicted. An original turn carries one command/task identity, original deadline, nondecreasing wall/counter/token floors and at most eight sorted mutation links. New attempts can reset completion flags only with their new original attempt ID and increased attempt count.

| Phase | Meaning |
| --- | --- |
| Armed | Original turn admitted; no terminal cleanup is certified. An existing fence stays inspectable here after a crash, local bookkeeping failure or an unjoined worker. |
| Settled | Trusted orchestration reconciled the original local work and actual known counters/receipts. This is not a generic no-effects claim. |
| StoppedUncertain | Actual local native/provider resources and Broker/service work drained, zero original tool calls/links, no provider terminal observation. Full conservative charge is retained; completeness flags are false. |
| StoppedUnusable | The same strict local drain and zero-tool/link conditions hold, and the actual provider terminal was observed. Provider completeness stays true; usage completeness reflects actual valid terminal usage independently of response usability. |
| Unknown | Actual local completion is observed but original effect/receipt uncertainty remains. It cannot be rearmed or silently rewritten to Settled. |

Stopped states permanently retire this chat and turn. They do not claim that the remote provider stopped, refund charges or authorize replay. A distinct fresh chat requires fresh explicit disclosure acceptance. Missing worker processes after restart are not proof that originals were joined. Terminal states are immutable except exact replay.

Provider usage is observed inside the native adapters before cancellable event yields or full content mapping errors and is bound to the original attempt. Full conservative pre-stream floors never decrease. Actual request-preparation/output/stream lifetime is a separate drain obligation; a dropped stream future or JoinHandle alone is insufficient.

Only trusted actual provider interruption/shape boundaries can make a failed native operation eligible for a stopped state. Generic storage, checkpoint, quote, preparation-accounting or worker failures preserve Armed/quarantine. Known invalid provider accounting remains distinguishable from a failed local bookkeeping write.

## Owner uncertainty acknowledgement

`AcknowledgeSettingsUncertainty` carries only command ID, exact fence SHA-256 and `lost_ram_uncertainty_accepted: true`. The principal comes from the actual authenticated LocalOwner caller. The original fence digest uses domain `everything.settings-agent.turn-fence.v1` followed by NUL and the bounded canonical encoded fence.

Admission requires the exclusive leased Host, closed intake, actual native/provider/Broker/service drains and the global Broker's original admission/exclusion gate. Inspection validates existing linked service receipts against their exact original child/native parent/principal; absence retains Unknown. Any genuine private ToolCall requiring cleanup needs its existing exact-row/digest acknowledgement first. No missing ordinary row is synthesized.

The acknowledgement is a separate immutable record of at most 512 encoded bytes, one per original fence, bounded by the 8192-turn lifetime capacity. Exact replay returns its original timestamp; changed identity/digest/principal/acceptance conflicts. It does not modify the original phase, counters or effect disposition. Lost-RAM chat is never reconstructed or retried. Cleanup permits the draining Host to finish and restart; it does not silently reopen a retired chat.

# Private Settings runtime contracts

These contracts describe the restricted runtime and the #115 backend seams. The new assistant seams have source-only formatting verification; combined Rust gates and disposable behavior verification remain pending for those changes. The existing [typed Settings contract](settings.md) and its native action/confirmation authority remain authoritative.

## Private native frames

AgentOpen authenticates an existing native proof and binds the actual outer private-frame request ID before creating a manager-owned child. Its reply contains a nonnil chat ID, provider, exact model/recipient, binding SHA-256, ordered seven provider tool names and original expiry. It contains no credential, private proof, provider continuation or ordinary conversation identity.

AgentSend carries that proof, exact chat ID, original turn CommandId, explicit accepted binding digest and UTF-8 text. Raw text is limited to 8 KiB; the existing complete encoded private input limit is 32 KiB, so escaping can require a smaller input. Changed reuse of a turn ID conflicts. Exact replay returns the retained original disposition without refreshing context, appending input or sending another request. The manager permits one Settings turn across at most two live/closing chats and eight turns per chat.

AgentPoll and AgentClose authenticate the retained native cleanup proof, including expired-session cleanup where supported by the original Authority. They must match the manager's original native parent. Replies contain a bounded status snapshot, separate committed and provisional text fields of at most 16 KiB each, up to eight unique original mutation links and original conservative counters. A link can precede its service record and does not establish a completed effect. They expose no raw private journal or continuation. The actual LocalResponse validation checks request/result correlation and the complete encoded wrapper. The wrapper is also measured against 200704 bytes for Status, 32768 for Open metadata and 65536 for scoped operation/details, within the existing 2 MiB transport limit. Oversized data is rejected; opaque state and completed text are not truncated to fit.

Closing a child preserves its native parent and siblings. Closing the native parent cancels its owned children through the existing Authority. No native client controls Host lifetime.

## Original Open recovery

AgentOpenStatus and AgentCloseOpen carry the authenticated cleanup proof and the original Open request ID. The same supervisor retains at most 1024 non-evicted Open records. Every insertion and transition measures the complete encoded original ID plus typed metadata against 512 bytes. Records contain only the native parent, original ID, state and optional child ID; they contain no proof, error string, chat text, Status or provider handle.

The nested OpenState has the `state` tag with not_accepted, ready, closing, closed, unavailable or unknown. Ready carries the already retained Opened disclosure; it does not recapture a provider. Replaying Open after closure cannot create another child. A different native parent conflicts. OpenStatus absence means only not_accepted at that observation. CloseOpen absence first reserves a closed tombstone without a child, preventing a later original Open from creating one. Capacity exhaustion is an error and never claims suppression.

Actual Preparing/Unknown child and private Session ownership is retained separately, counts toward the same two-chat ceiling and blocks quiescence until the real cleanup completes. Losing a pipe reply does not abort manager-owned preparation. The bounded RPC queue and metadata do not provide a hard operating-system or keyring time bound: Status/Close can wait for the existing supervisor-owned preparation to finish. No restart path recreates the old native proof, child or inference.

## Ephemeral live text and next-turn context

Status adds checked per-chat text_revision, optional provisional and optional context observation. Provisional contains the actual attempt_id, state streaming or interrupted, and bounded cumulative text. The same turn worker consumes actual ModelAttemptStarted, ProvisionalText and Committed events. One latest-value watch cell coalesces display snapshots without a chunk queue or another inference worker. Attempt reset, original-turn reset, committed replacement and interruption increment the revision. It never wraps; a bookkeeping failure fails closed.

assistant_text contains only actual Committed text. A commit clears provisional; a failed or cancelled stream retains partial text as interrupted. Native phase and final service outcomes remain separate from model text. At finish the last presentation transfers into terminal Status and the watch text is cleared. A presentation failure explicitly disables stopped-state classification even if another provider marker was observed, preserving the original Armed/quarantine rules. Dropping the stream still requires the existing actual provider/native, Broker and service joins.

A new original Send first reserves its exact input identity, then uses the actual child Get caller on the same SettingsService Core. The owned read is awaited before begin_turn, the Armed fence and native execution. A genuine Get failure retains UnavailableBeforeStart; exact replay returns that disposition without another read. Separate local presentation/serialization/bookkeeping failures are not classified as that read failure. Before a worker starts, these failures explicitly retain Unknown status and LocalFailureBeforeStart through real child closure; they cannot archive a Running status without a worker. They retain failure and quarantine semantics.

The real validated redacted Host snapshot is encoded at most 32768 bytes and supplied as untrusted data in immutable TaskContext.state with a checked prefix of at most 256 bytes. Status exposes only its actual revision, observed_at and stale fields. This is a point-in-time observation: concurrent changes after capture may precede the request, and later tool reads/CAS remain authoritative. Each new original Send captures once; a replay or later round does not refresh it. The existing pre-stream source/provider generation guard remains unchanged.

Actual request/context/checkpoint admission includes this state. The conservative new F contribution is at most 2 * 32768 + 1024 = 66560 encoded bytes, and the extra nested request representation is at most 4 * 32768 + 2048 = 133120. The unchanged composite gate requires actual F + 863690 <= 2097152, or F <= 1233462. No limit is raised to fit a snapshot or history. The SQLite page margin remains a capacity estimate, not a process-RSS guarantee.

## Original-child operation adoption

AgentInspectOperation and AgentActionDetails carry the actual native proof, chat ID, original turn ID and original operation ID. Details additionally carries its exact native reference. These are same-Core scoped reads; UUIDs do not mint a caller or authorize a mutation.

The leased ordinary Store must contain the actual immutable session reservation with this Host, principal and native parent, plus that original turn's fence with exact child identity and operation membership. A recorded operation must match the same original request, principal, parent, Host scope and SettingsAgent origin. Cleanup Inspect can return historical metadata; a verified link without a service row returns lookup state not_recorded. That state does not claim rejection, no effects or completed cleanup. Foreign identities and storage failures remain errors.

Details requires a live native caller, the exact currently-awaiting Action/Confirmation reference, original revision/expiry and live child checks. Only an actual Patch, Device Pair/Revoke or Native OpenSection target is returned. The result carries original chat/turn/operation identity plus the validated inspection and target. It cannot create credential, notification, owner-replacement or pairing-approval handles. Existing native confirmation, effect admission, cancellation and finalization remain mandatory. No general owner operation is forwarded.

## Sealed execution and storage

SettingsProfile, SettingsToolCapability and the internal authority constructor are crate-private and cannot be minted by deserializing an execution mode, tool ID or UUID. The actual Broker chooses the selected RAM journal, shares global permits/exclusion/quarantine, and passes the sealed context only to registered private entries. General contexts cannot execute those entries; private contexts cannot execute General entries.

The RAM Store configures 4096-byte pages, an 8192-page hard cap, memory journal/temp storage and disabled cache spill before sensitive insertion, then verifies the fileless connection and these settings on capacity admission. Two stores bound main database pages to 64 MiB. This is not a process-RSS bound: journals, caches, bounded Rust values, native parsers and dependencies consume additional memory.

The following encoded limits are enforced in source: response 512 KiB, continuation state 256 KiB, arguments 16 KiB/call, event 192 KiB, append 512 KiB, checkpoint 2 MiB and Broker request/record 64 KiB each. Assistant text is bounded while provisional deltas are accumulated and again before accepted response mapping. Four calls/round, eight calls/turn and parallelism one are immutable ceilings. The native adapters retain their existing 32 MiB response and 64–128 MiB parser ceilings.

Admission checks the actual serialized composite checkpoint, including the duplicated continuation binding, then quotes future result/replay storage and verified SQLite page headroom before payment or tool effects. Metadata outcome slots are preallocated outside SQLite, bounded and reserved before admission. A page/serialization failure never authorizes the old ProviderInFlight checkpoint to resume. Numerical bounds and candidate runtime observations still require combined validation.

## Durable fence semantics

A session reservation is immutable and nonsecret. At most 1024 reservations and eight original turns/reservation are retained; unresolved state is never evicted. An original turn carries one command/task identity, original deadline, nondecreasing wall/counter/token floors and at most eight sorted mutation links. New attempts can reset completion flags only with their new original attempt ID and increased attempt count.

The actual worker registry and JoinedTurn retain the original turn CommandId for both normal and panic cleanup. Foreground reconciliation names that exact command. Provider observation, receipt correlation and final status/fence metadata require that original child and command; an older Settled profile fence cannot supply the new turn's counts or operation links. Missing/mismatched RAM state keeps Unknown/quarantine and all actual worker joins, never rewrites the old fence or infers no effects. A failed begin_turn can leave the prior RAM fence, including after a new durable arm but before RAM replacement; that original durable uncertainty remains inspectable.

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

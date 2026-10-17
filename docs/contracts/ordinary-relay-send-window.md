# Ordinary relay send window

The existing `DeviceEndpoint` relay loop owns one `OrdinarySendWindow` with at most 16 message cells. Preparation, a prepared packet, an awaited socket send and a sent message awaiting relay acceptance all occupy that same map. This implementation selects `TransportOutboundSelection::OrdinaryOnly` with or without a settings transport owner. Binding a settings owner to a connection does not enable private settings delivery.

## Selection and exact loading

The metadata-only [outbound selector](transport-outbound-selection.md) supplies one candidate excluding the window's current IDs. The window registers the actual input and its private slot before spawning the blocking preparation worker or loading ciphertext. `Store::load_ordinary_transport_candidate` matches the selected message ID, recipient, creation time and retry time, and rechecks pending status, readiness, transport expiry, active peer and private-row exclusion. It returns no row when that exact snapshot no longer matches; it never probes private ciphertext or substitutes another row.

The loader checks the borrowed SQLite value is a nonempty BLOB of at most the existing 2 MiB transport envelope bound before calling the existing `outbox_row` decoder. Existing row conversion, attempt range and checked Store-connection behavior remain in use. The result's constructor stays inside Store and the bounded row moves into the actual worker.

This path adopts the selector's canonical nonnil UUID contract. Older generic storage APIs accept broader bounded identifier strings. Malformed selected identifiers now fail explicitly. The old batch reader's aggregate read-budget pagination and page-halving fallback do not apply to this exact single-row load. These are explicit compatibility differences; malformed or oversized rows are never silently skipped or treated as an empty outbox.

## Preparation and send ownership

The window retains the original `spawn_blocking` handle and its task ID before awaiting it. Awaiting borrows that same registered handle; no cancellation selection, abort, reference count or `is_finished` observation substitutes for a join. The complete actual result is registered before slot, endpoint or task-error association checks. The worker's temporary raw row and serialization values are destroyed before it returns. Its only packet result is the decoded bounded envelope.

Preparation keeps the existing local endpoint, pinned peer and header checks; retry delay remains `1000 << min(attempts, 8)` milliseconds. The original `NotFound`-only deferral fallback remains exact. An actual deferral `StateConflict` is still an error. The fresh confirmation still serializes the decoded envelope again, reads a fresh clock and peer registration and calls the existing exact-byte confirmation API. It does not replace that confirmation with a raw-byte comparison that would accept additional encodings.

No row, a revoked peer, false confirmation or cancellation before send produces a worker-local `NotSent`. Only the true original join makes that result usable. It means no socket send for this attempt; a deferral may already have occurred. Cancellation after preparation and before socket admission destroys the actual joined packet without sending it.

The actual `RelayClientMessage::Send` remains in `SocketOwned` while the existing `RelayConnection::send` borrows it across its await. The actual returned result is registered before further checks. The packet is destroyed before extracting the same private slot and starting the 15-second acceptance timer. Input exclusivity is checked only after the original join, the actual send return and packet destruction. Failed exclusivity retains the entire returned input. It never proves task completion or reconstructs a slot.

## Acceptance, expiry and normal teardown

An Accepted frame can remove only a matching `AwaitAccepted` cell from this window. That actual removal creates the private `OrdinaryAccepted` receipt consumed by the existing Store acceptance call. Removal still precedes the awaited database operation, preserving the ordinary durable retry behavior. Relay acceptance represents relay custody, not client delivery or execution success.

Maintenance expiration of an ID absent from this window is a no-op. An ID present in an unfinished phase is a bookkeeping fault, with its actual owner retained. Only fully returned `AwaitAccepted` cells expire normally. The original 15-second timeout starts after the awaited send and packet destruction. Heartbeat, ordinary incoming delivery, acknowledgments, retry classifications and durable outbox deletion remain on the existing relay pump.

Every normal relay-session outcome enters `finish`. The actual primary outcome is registered before any cleanup await. Remaining original preparation handles are joined and their complete outputs registered before destruction. A clean finish destroys packet/result owners, clears ordinary slot metadata and returns the original outcome. An observed network failure after the actual send returns remains eligible for the existing retry behavior; it does not quarantine the endpoint.

## Catastrophic retention and session lifetime

A bookkeeping failure, unjoined owner or dropped unfinished populated window retains the complete bounded map, original handles, outputs, inputs, completion outcome and typed failure cause in terminal ownership. Retention occurs before the real endpoint quarantine latch is set. Terminal ownership contains no session mutex guard or Host guard. There is no reset, private accounting refund, task abort, guessed completion or automatic recovery path. The monotonic endpoint fence prevents a successor window in that endpoint clone group and participates only as a negative condition in Host transport draining.

Both outer relay entry points acquire the original endpoint session mutex once. The locked loop checks outbound health before connection admission, reconnection and every normal Stopped path. The settings wrapper holds that same actual guard through `close_and_wait` and its final health check and Stopped publication. Its existing failed-drain pending state also retains the guard. No new session lock or runtime is introduced.

These are source ownership and protocol-preservation claims. Direct formatting and static source checks do not verify compiler acceptance, SQLite execution, cancellation timing, socket behavior or an Apple roundtrip. No automated tests were added. Supported CI and disposable runtime observations remain required. Private ciphertext preparation, current socket admission, send ownership, Bind creator retirement and composite private draining remain separate dependencies.

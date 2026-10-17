# Apple AccessStatus client

The iOS Settings screen exposes one remote Settings control: an explicit read-only "Check Host access" button (`apps/Apple/iOS/SettingsView.swift`). It sends one `AccessStatus` query over the existing [closed wire](apple-remote-settings-wire.md), pinned identity, `ReplicaService` and relay connection, and displays access metadata only: readable/writable keys, configured provider/model/recipient consent and configured revision, with freshness. It sends no model request, native Owner operation, Snapshot, update or chat, and never starts Bind.

The Host in this tree answers only `AccessStatus`; every other Settings kind is recorded as rejected ([Host Access transport](settings-access-transport.md)).

## Gesture and original

- A one-use MainActor gesture captures `ContinuousClock` before wall time, one query UUID, the exact Service and the displayed connection token before the owned Task. These are timing/correlation data, not authority.
- Service intake requires the same Service, token, current scope and an empty pending cell. A retained identical input returns its original handle; a spent input cannot mint another original. Each new check needs a new gesture.
- Request lifetime is at most 30,000 ms from the gesture capture. No clock is restamped at intake, preparation or reconnect. Query UUID, encrypted request message ID and reply envelope ID are distinct identities.
- Hide, cover, tab/path change, scene inactivity, disconnect, disable and Host/scope replacement retire the UI generation, stale the copied display and cancel the owned Task. Scene/disconnect/disable join that Task after Service stop.

## Send window

- One outbound map counts ordinary sends and the single private original together, total 16. A full window refuses the check; it is not queued.
- The encrypted message ID is bound once after preparation and must not collide with the query UUID, any registered outbound ID or a pending incoming ACK. Store Host/local/disabled checks and the original clocks are repeated before the single send.
- Private sends use no durable outbox row, re-encryption or retransmission. Private `Accepted` settles only that RAM cell and never runs ordinary Accepted SQL; it is relay custody, not permission or Saved.
- A sent private cell whose original is cancelled, retired or expired is released on the next pump pass once its send has returned; the connection stays open. If it was not yet Accepted, its encrypted ID moves to a bounded set (at most 16) so a late Accepted is consumed silently; a full set falls back to the timeout reconnect. Unsent cancellation releases after preparation returns. A one-way send error closes the connection through its task group.

## Receive and ACK

- The receiver authenticates the full pinned Host/local identity and frame, decodes strictly, checks the encrypted-ID and query correlations, commits metadata-only replay state, then rechecks Store, scope and clocks before adopting a genuine Fresh reply. Duplicates and unrelated replies cannot adopt.
- Clock skew: the reply's sealed creation time may be up to 5 s (`settingsClockSkewMilliseconds = 5_000`) before the request's wall floor; its expiry must not exceed the request expiry. The Host applies the same 5 s allowance to the request's `issued_at`.
- Per-reply failures stay local. A Settings delivery that fails verification or qualification (identity, size, expiry, invalid, signature, conflict, unavailable) is refused for that delivery only: after re-checking the Store's active Host and local identity, the receiver ACKs it and keeps the relay runner and chat sync running. The pending request is not failed by an unattributable delivery; it completes on a genuine reply or is released at its own deadline.
- Store, revocation and integrity failures, including a failed re-check, keep the terminal path. Queue-full is not ACKed. Failures never become denial or freshness.
- Unsupported Settings forms still go through the [pre-cache refusal](apple-private-settings-ingress.md); no plaintext enters ordinary storage.

## Display

One bounded RAM observation with its original clipped wall/monotonic fences and weak scope. Stale or absent is not denial. `grantUnavailable`, local unavailability, timeout, cancellation and interruption stay distinct. Copies go stale on lifecycle invalidation or refresh failure. No writable control or permission is inferred from the labels.

Bind, Snapshot, Original inspection and Update associations are released with their scope; their clients remain in source without a Settings screen control (see [Bind](apple-settings-bind-client.md), [Snapshot](apple-settings-snapshot-client.md), [inspection](apple-settings-original-inspection-client.md), [Update](apple-settings-update-client.md)).

## Validation

Local compile/static checks only. No device, relay round-trip, Host reply or UI behaviour has been observed. Automated coverage is deferred to [#32](https://github.com/izyuumi/everything/issues/32).

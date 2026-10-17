# Ordinary relay send window

`src/devices/session/outbound.rs` owns one `RelaySendWindow` per relay session. It carries ordinary outbox messages only. Private Settings replies are sent by the [Settings Access path](settings-access-transport.md), never through this window.

## Selection and loading

When the window has room, the session asks `Store::next_transport_outbound_candidate` for one metadata candidate, excluding IDs already in the window. Candidate selection always excludes rows with private Settings provenance. `Store::load_ordinary_transport_candidate` then loads that exact row (same ID, recipient, creation and retry time, still pending, ready, unexpired, active peer, not private). A row that no longer matches yields no send. Selected IDs must be canonical non-nil UUIDs; a malformed ID fails explicitly.

## Limits and states

| Item | Value |
| --- | --- |
| Messages in the window | 16 (preparing, prepared, on the socket or awaiting `Accepted`) |
| `Accepted` timeout | 15 s after send; exceeding it ends the session with Timeout |
| Retry delay | `1000 << min(attempts, 8)` ms |

Each message is prepared in a blocking task whose handle is joined before its result is used. Preparation checks the local endpoint, pinned peer and envelope header, defers the row by the retry delay and reconfirms it. No row, a revoked peer, failed confirmation or cancellation yields no send for this attempt.

After a successful socket send the cell moves to AwaitAccepted. Relay `Accepted` for that ID removes the cell and calls `accept_transport_outbox`. `Accepted` for an ID the window is not awaiting is a protocol error, except Settings reply IDs, which the session consumes first (tracked for at most 15 s). Maintenance expiry removes an AwaitAccepted cell; expiry of any other state is a fault.

## Failure

A socket error ends the session and leaves the row for the existing durable retry. A bookkeeping mismatch, an unjoined preparation at finish, or dropping a window with unfinished cells keeps the cells retained in memory and permanently quarantines the endpoint's outbound path (see [relay health](ordinary-relay-health.md)). A quarantined endpoint refuses new windows and sends with `DeviceError::OutboundQuarantined`.

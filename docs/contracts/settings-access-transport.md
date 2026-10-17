# Remote Settings Access transport (Host)

The Host answers exactly one remote Settings family over the relay: the read-only `AccessStatus` query ([request](settings-access-status.md) and [reply](settings-access-replies.md) DATA). Every other Settings kind is refused. This contract covers the Host path from relay delivery to the sent reply. The Apple side is in [the Access client](apple-settings-access-client.md).

Owners: `src/devices/session.rs` (`relay_session_loop`), `src/devices/settings_connection.rs`, `src/devices/settings_ingress.rs` and `settings_ingress/{access,wire,work,work/build}.rs`, `src/devices/settings_publication.rs`, `src/host/settings/service/paired*.rs`, `src/host/settings/authority/paired.rs`, `src/storage/settings_transport.rs`, `src/storage/settings_publication.rs`, `src/storage/transport.rs`. Storage tables and quotas are in [transport storage](settings-transport-storage.md).

## Startup

The transport owner, capability install and transport registration complete before `SettingsService::bind_paired_transport`, so the service is never bound to an owner a later failing step dropped. If Settings setup fails, the ordinary relay runs without Settings.

## Connection

On each relay connect, `HostSettingsTransportOwner::connected_relay` creates one `SettingsConnectionOwner`. It captures the authenticated [socket identity](relay-socket-identity.md) and a fresh connection epoch, and connects Work. `RelayConnection::send`/`receive` refuse a socket whose identity is no longer live.

## Receive

Every relay Delivery, ordinary or private, is admitted to Work one frame at a time. The relay socket is not read while a delivery is in admission. Work opens the envelope under the Settings mutation guard.

- Ordinary payloads take the existing durable inbox path.
- A Settings packet over 32 KiB is rejected as `invalid_packet`.
- Any Settings kind other than `AccessStatus` is rejected as `settings_unavailable`.
- `AccessStatus` is admitted in one IMMEDIATE transaction: private-request ledger charge, verified source, current authority (pins, grant head, Access query cell), reply reservation and processed inbox marker, with a recheck before COMMIT.

The relay ACK is sent only after the durable receipt (ordinary, rejected, committed or duplicate) is joined. A delivery that is not ACKed may be redelivered by the relay.

| Limit | Value |
| --- | --- |
| Private requests in flight | 32 total, 4 per peer, 1 MiB total bytes |
| Private request plaintext | 32 KiB |
| Request lifetime | at most 30 s |
| Clock skew | `issued_at` may be up to 5000 ms (`SETTINGS_CLOCK_SKEW_MS`) ahead of the Host wall clock |
| Access query cells (RAM) | 512 per connection, 64 per peer |
| Queued published replies | 32 |
| Reply reservation | default 49,152 bytes (32 KiB plaintext); denial 4,096 bytes (2 KiB plaintext) |

Query cells whose original deadline has passed are evicted before the capacity check. All cells are cleared when a new connection registers its first query.

## Failure classification

| Cause | Result |
| --- | --- |
| Expiry, out of scope, conflicting reuse of a query id, clock regression, revoked device or endpoint, transport conflict, unavailable clock | Terminal rejection marker recorded; ACKed |
| Unsupported Settings kind or oversized packet | Terminal rejection marker recorded; ACKed |
| Capacity (ledger, query cells, reply quota) | Nothing recorded; not ACKed |
| SQL or storage fault, failed COMMIT, failed rollback, bookkeeping or worker failure | Settings receive is quarantined |

Quarantine fails Work, which ends the relay session with an error; the transport drain then reports failure and a process restart is required. A failed rollback also latches the shared Store per [ADR-0013](../decisions/ADR-0013-permanent-store-quarantine.md).

## Reply

Work builds replies with one build slot; committed queries wait in FIFO order, bounded by the private ledger. The build worker, under the mutation guard, qualifies current access, seals, consumes the reservation, inserts the private outbox row and the outbox row, captures a comparison, rechecks and COMMITs.

- A publication that rolled back cleanly (expired, authority changed) drops that query; its reservation is swept on expiry.
- An uncertain or failed rollback or commit, or a panic, quarantines.

A published reply moves into the queued registry and the session is told `ReplyQueued`. The session takes it, acquires the mutation guard and, in a blocking task, loads the private ciphertext in a Deferred transaction that rechecks current authority before and after reading and verifying the stored envelope. It then sends the frame on the same relay socket.

After the send attempt, successful or not, or immediately when the load fails with a rolled-back cause (expired, revoked, swept), the private outbox rows are deleted in an IMMEDIATE transaction or confirmed absent, and the reply owner is retired. Retirement releases the private, byte and peer charges. There is no resend. A retirement failure quarantines.

The relay `Accepted` for a sent Settings message id is consumed (tracked for at most 15 s), not treated as a protocol error. Ordinary messages keep their [send window](ordinary-relay-send-window.md) behaviour.

## Connection end and shutdown

When the connection ends, the owner is dropped: the connection identity is retired, intake closes, a pending frame is dropped without ACK, and waiting queries and an unstarted build are withdrawn. `drain_connection` joins jobs, retires every queued reply (rows deleted) and requires a complete ledger before the next connect. A failed drain makes the relay report Failed and Host shutdown return an error; shutdown does not hang.

## Not implemented

Bind, Update/command, permanent Originals, Snapshot and Original-inspection reads, paired Settings supervision and history, and parent registration are not in this tree. Their Swift clients exist without a responding Host; the Host records their packets as rejected.

## Validation

Local compile and static checks only. No runtime, device, relay round-trip or security verification has been performed. Deferred automated coverage is tracked in [#32](https://github.com/izyuumi/everything/issues/32).

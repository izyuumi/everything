# Remote Settings transport (Host)

The Host answers one scoped remote Settings request family over the relay through a single pipeline. Request kinds are `access_status`, `snapshot`, `changes`, `update`, `host_action` and the session kinds `session_open`, `session_send`, `session_poll` and `session_close` ([request DATA](settings-access-status.md), [remote session](settings-remote-session.md)); replies are in [reply DATA](settings-access-replies.md). Authority comes only from the verified envelope sender, the live pin and the owner-created [grant](settings-grants.md). There is no OwnerQuery/OwnerOperation forwarding. The Apple side currently sends only `access_status` ([Access client](apple-settings-access-client.md)).

Owners: `src/devices/session.rs` (`relay_session_loop`), `src/devices/settings_connection.rs`, `src/devices/settings_ingress.rs` and `settings_ingress/{access,wire,work,work/build}.rs`, `src/devices/settings_publication.rs`, `src/devices/settings_access_reply.rs`, `src/host/settings/service/paired*.rs` (including `paired/remote.rs`), `src/host/settings/authority/paired.rs`, `src/storage/settings_transport.rs`, `src/storage/settings_publication.rs`, `src/storage/transport.rs`. Storage tables and quotas are in [transport storage](settings-transport-storage.md).

## Startup

The transport owner, capability install and transport registration complete before `SettingsService::bind_paired_transport`, so the service is never bound to an owner a later failing step dropped. If Settings setup fails, the ordinary relay runs without Settings.

## Connection

On each relay connect, `HostSettingsTransportOwner::connected_relay` creates one `SettingsConnectionOwner`. It captures the authenticated [socket identity](relay-socket-identity.md) and a fresh connection epoch, and connects Work. `RelayConnection::send`/`receive` refuse a socket whose identity is no longer live.

## Receive

Every relay Delivery, ordinary or private, is admitted to Work one frame at a time. The relay socket is not read while a delivery is in admission. Work opens the envelope under the Settings mutation guard.

- Ordinary payloads take the existing durable inbox path.
- A Settings packet over 32 KiB is rejected as `invalid_packet`.
- A Settings packet that does not decode to one of the nine request kinds is rejected as `settings_unavailable`.
- A decoded request is admitted in one IMMEDIATE transaction: private-request ledger charge, verified source, current authority (pins, grant head, query cell), reply reservation and processed inbox marker, with a recheck before COMMIT. A device without an Active grant gets a `denial` reservation; otherwise `default`.

The relay ACK is sent only after the durable receipt (ordinary, rejected, committed or duplicate) is joined. A delivery that is not ACKed may be redelivered by the relay. A relay ACK is transport receipt, not mutation success.

| Limit | Value |
| --- | --- |
| Private requests in flight | 32 total, 4 per peer, 1 MiB total bytes |
| Private request plaintext | 32 KiB |
| Request lifetime | at most 30 s |
| Clock skew | `issued_at` may be up to 5000 ms (`SETTINGS_CLOCK_SKEW_MS`) ahead of the Host wall clock |
| Query cells (RAM) | 512 per connection, 64 per peer |
| Queued published replies | 32 |
| Reply reservation | default 49,152 bytes (32 KiB plaintext); denial 4,096 bytes (2 KiB plaintext) |
| Remote change page | at most 64 rows and 24 KiB |
| Paired-origin journal identities | 32 per device, 128 total |

Query cells whose original deadline has passed are evicted before the capacity check. All cells are cleared when a new connection registers its first query.

## Failure classification

| Cause | Result |
| --- | --- |
| Expiry, out of scope, conflicting reuse of a query id, clock regression, revoked device or endpoint, transport conflict, unavailable clock | Terminal rejection marker recorded; ACKed |
| Unsupported Settings kind or oversized packet | Terminal rejection marker recorded; ACKed |
| Capacity (ledger, query cells, reply quota) | Nothing recorded; not ACKed |
| SQL or storage fault, failed COMMIT, failed rollback, bookkeeping or worker failure | Settings receive is quarantined |

Quarantine fails Work, which ends the relay session with an error; the transport drain then reports failure and a process restart is required. A failed rollback also latches the shared Store per [ADR-0013](../decisions/ADR-0013-permanent-store-quarantine.md).

## Reply build

Work builds replies with one build slot; committed requests wait in FIFO order, bounded by the private ledger. The build worker holds the Host mutation guard for the whole build, except while a session request waits for the restricted runtime.

1. `SettingsReceiptAuthority::prepare_remote_reply` (generation-bearing kinds only: snapshot, changes, update and the session kinds) rereads current access with the reservation still present. Without an Active grant it prepares nothing and the reply is the `grant_unavailable` denial. With an Active grant it compares the request `generation` (grant id, grant revision, Host epoch, connection epoch) with the current one; a mismatch becomes `grant_changed`. Otherwise:
   - Snapshot: every requested key must be readable; the existing Host snapshot is filtered to the requested keys.
   - Changes: the existing change-hint page, with each hint's keys intersected with the readable set and empty hints skipped.
   - Update: every changed key must be writable. A paired caller is minted for this one request and the existing update path runs with the already held guard: original request-ID replay first, revision CAS, journal identity, file write and receipt. See [paired authority](paired-settings-authority.md).
   - Session kinds: the grant must carry `ConfiguredProvider` session consent (otherwise `outside_scope`). The build then releases the guard, runs the request against the restricted runtime through the device's paired session parent (bounded by the request deadline minus 2 s) and reacquires the guard for publication. See [remote session](settings-remote-session.md).
   Service errors become closed denial reasons (`conflict`, `outside_scope`, `write_blocked`, `capacity`, `invalid`, `outcome_unknown`, `unavailable`). When the reread finds no Active grant, the device's paired session parent is retired.
2. Publication: under the same guard, qualify current access again, project the reply (the prepared payload must carry the current generation), seal, consume the reservation, insert the private outbox and outbox rows, capture a comparison, recheck and COMMIT ([publication](settings-reply-publication.md)).

- A preparation or publication that failed before or rolled back cleanly (expired, authority changed) drops that request; its reservation is swept on expiry. A mutation that already committed stays committed; the client recovers its receipt by resending the same `request_id`.
- An uncertain or failed rollback or commit, or a panic, quarantines.

A published reply moves into the queued registry and the session is told `ReplyQueued`. The session takes it, acquires the mutation guard and, in a blocking task, loads the private ciphertext in a Deferred transaction that rechecks current authority before and after reading and verifying the stored envelope. It then sends the frame on the same relay socket.

After the send attempt, successful or not, or immediately when the load fails with a rolled-back cause (expired, revoked, swept), the private outbox rows are deleted in an IMMEDIATE transaction or confirmed absent, and the reply owner is retired. Retirement releases the private, byte and peer charges. There is no resend. A retirement failure quarantines.

The relay `Accepted` for a sent Settings message id is consumed (tracked for at most 15 s), not treated as a protocol error. Ordinary messages keep their [send window](ordinary-relay-send-window.md) behaviour.

## Authority boundaries

- Existing pairing grants nothing; only the owner grant routes create, reduce or revoke a grant. A reduction or revocation is visible to the next preparation, including requests already admitted and waiting for the build slot.
- Writable keys are limited to the grant's writable set, which grant storage limits to `memory.embeddings.enabled` and `memory.embeddings.dimensions`. Protected, read-only, device and OS keys are refused.
- `host_action` never executes anything: it returns `host_action_required` for credential entry, pairing invitation or approval, device revocation, grant changes, execution policy, recipient consent and lifecycle.
- A remote session only uses the restricted runtime's list/get/update tools within the grant's readable and writable keys; action and device tools are refused. Grant changes and device revocation retire the device's session parent.
- Settings remote packets never carry credentials, invitations or secret transfers, and no plaintext request or reply body is persisted.

## Connection end and shutdown

When the connection ends, the owner is dropped: the connection identity is retired, intake closes, a pending frame is dropped without ACK, and waiting requests and an unstarted build are withdrawn. A live connection is part of the paired caller's validity, so a write that has not yet renamed its file is cancelled by the existing write-origin check. `drain_connection` joins jobs, retires every queued reply (rows deleted) and requires a complete ledger before the next connect. A failed drain makes the relay report Failed and Host shutdown return an error; shutdown does not hang.

## Not implemented

No Apple client sends the scoped or session requests. The Swift Snapshot, Inspection, Bind and Update codecs predate this Host shape and do not interoperate with it; iOS integration is #117.

## Validation

Local compile/static checks and disposable real-relay walkthroughs with a scratch Replica helper ([scoped requests](../development/settings-validation.md#remote-scoped-settings-walkthrough), [remote session](../development/settings-validation.md#remote-settings-session-walkthrough)). Deferred automated coverage is tracked in [#32](https://github.com/izyuumi/everything/issues/32).

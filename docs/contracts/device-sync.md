# Device synchronization contract

`devices::DevicePacket` is encrypted as the existing crypto protocol's envelope plaintext. It has strict `schema_version = 1` and tagged `payload.kind`. Unknown packet fields/kinds fail closed. Native `CommandEnvelope` and `EventEnvelope` remain the [event contract](events.md); transport does not regenerate them while deduplicating.

| Payload kind | Direction and semantics |
| --- | --- |
| `command` | Replica to authoritative host. Sender must equal `command.issuing_device_id`. Conversation and command/permission-submission scopes are required. Full original command JSON is journaled before native host acceptance. |
| `command_result` | Host to replica. Command ID, terminal `completed` or `rejected` state, optional result. `pending` is invalid. A missing result may represent archived terminal output. Replica application acknowledges durable local consumption separately. |
| `sync_request` | Replica to host. One authorized conversation cursor; event count 1 through 256. Applications use the durable replica cursor. |
| `sync_batch` | Host to replica. Existing bounded contiguous `SyncBatch`, at most 512 KiB. Immutable envelopes only; no private provider checkpoint. |
| `sync_acknowledgement` | Replica to host. Binds the delivered batch message ID, next cursor and optional final event ID to a durable host offer. Advances host knowledge only after replica application committed. |
| `revoked` | Authoritative host to its replica, naming that replica's identity and timestamp. Receipt makes the local endpoint permanently inactive. It does not promise remote deletion of prior data. |

`DeviceAccess` is an owner-supplied value at pairing, never model input. Its validated local pin is immutable. `PairingApproval` requires a label, the exact fingerprint the owner confirmed, and explicit access. Initial pairing changes trust and must be driven by trusted application UI/CLI. Relay public-key enrollment remains a separate operator action.

## Durable boundaries

1. Authenticate/decrypt a relay Delivery using the currently pinned sender keys.
2. In one Store transaction, recheck exact pin/revocation/local recipient and insert replay marker plus pending plaintext inbox.
3. Only then send relay `Acknowledge`. Authenticated exact duplicates may repeat this ACK.
4. Validate/process the local durable packet. A native host reserves original commands before generating events; an event replica commits events, cursor and encrypted application ACK together.
5. Only an encrypted application ACK matching the host's persisted offer advances the host's device cursor.

The relay's `Accepted` means its ciphertext queue accepted the sender's message; the sender may remove its local ciphertext copy. It says nothing about the recipient's application. The relay's `Acknowledged` confirms removal of a delivery queue entry after durable inbox receipt. Neither relay response authorizes command execution or advances event cursors.

`Store::reserve_remote_command` returns `RemoteCommandReservation { created, command }`. A changed command ID reuse conflicts across sender, conversation or full original JSON. `DeviceEndpoint::pending_commands` recovers accepted pending commands after restart. Native dispatch must retain original command identity and reconcile existing host acceptance/outcomes. `complete_command` atomically records the immutable terminal outcome and encrypted reply; a quota error rolls back a new completion. A completed duplicate remains pending until its reply is durably queued. Pending work is never silently retried as a new command.

`Store::pending_transport_inbox_after` and `pending_remote_commands_after` provide bounded keyset reads. Their optional `TransportInboxCursor { received_at, message_id }` and `RemoteCommandCursor { received_at, command_id }` select rows strictly after that tuple, ordered by receipt time and ID. Construct the next cursor from the last returned row, including when that row remains pending. Existing reads without a cursor return the first page. Each page accepts a limit from 1 through 1000 and enforces the existing 16 MiB serialized aggregate bound; exhaustion returns an error rather than a truncated successful page. Cursor timestamps and IDs use the same validation as stored rows.

These reads preserve the original payloads, receipts, sender identities and pending-state filters. Inbox reads exclude revoked peers, and revocation atomically rejects their pending command rows. Pages observe current state individually, so callers must still recheck authority under the host revocation gate before dispatch. A drain must advance through all pages even while earlier rows remain blocked, allowing later cancellation and approval commands to reach the host. Restart each new drain from no cursor to revisit previously blocked work and newly inserted rows that sort before the previous cursor; cursors are scan positions, not acknowledgements or durable synchronization progress.

`ReceivedOperation::CommandResult` includes its durable inbox `message_id`. Save the notification in application state before calling `acknowledge_command_result(message_id)`. Until then, repeated processing/restart returns the same notification. This boundary prevents a transient callback from losing an accepted result.

## Public integration surface

- `DeviceEndpoint::new(Store, Arc<DeviceIdentity>, EndpointRole)` binds immutable local identity/role to one database.
- `load_or_create_identity_in(&cap_std::fs::Dir)` loads/publishes owner-only identity using an already trusted directory descriptor on Linux/Apple. Host bootstrap supplies the service StateDirectory handle and retains its host lock.
- `begin_pairing`, `create_pairing_request`, `approve_pairing`, and `finish_pairing` implement a bounded out-of-band QR/secret ceremony with explicit fingerprint confirmation.
- `submit_command`, `request_sync`, and `complete_command` queue real encrypted packets through durable outbox transactions.
- `receive_envelope` performs authenticated durable receipt; it does not ACK a socket itself.
- `process_inbox(now, limit)` performs scoped protocol operations and returns durable host commands, result notifications or explicit rejections. It does not execute tools/agents.
- `pending_commands(limit)` recovers native dispatch work; `acknowledge_command_result` completes application consumption.
- `run_relay(origin, cancellation, watch_sender)` runs the socket/outbox pump and emits `RelaySessionStatus`; dispatchers use `inbox_changed` notifications plus an initial startup drain. Callers must observe the returned result.
- `maintain(now)` explicitly reports expiry/archive work. `revoke_device` is an owner-only host lifecycle operation.

Endpoint synchronous APIs use the single Store connection under its mutex. Async callers use a blocking worker boundary; the built-in pump supplies its own boundary and awaits completion. Cloned endpoints share in-memory pairing/session admission. Independently constructed endpoints require the service's one-host lock rather than racing multiple dispatchers.

## Bounds and recovery

Encrypted plaintext is at most 1 MiB; serialized relay frames are at most 2 MiB. Incoming pending payload is bounded globally and per peer, and sender queues have separate count/byte bounds. See [device architecture](../architecture/devices.md#revocation-and-limits) for limits and retention. Exhaustion returns an explicit error. No relay ACK is sent when durable acceptance failed.

Replica events are keyed by authoritative host, conversation and sequence, with unique event IDs per host. Repeated events require exact identity/content equality. A received sequence gap cannot move the cursor; the endpoint requests its current durable cursor. A batch already applied before disconnect can be replayed and acknowledged safely. The host retains its previous cursor until it receives the matching application ACK; a client resume request uses the client's own committed cursor.

Expiry prevents late network acceptance. Already accepted pending inbox/command work survives network expiry and requires explicit application processing/rejection. Completed replay markers survive through envelope expiry; old command identity digests survive result archiving. Revocation is sticky and rechecked in receiving/applying transactions, and pending peer work is durably rejected. Relay administrative revocation is independent and must also be performed to remove blind relay traffic.

`RelayConnection` uses native Tokio WebSockets and Rustls/webPKI roots. Plaintext `ws` is restricted to literal loopback or `localhost` pinned to loopback addresses without DNS; production uses `wss`. Origin/device/session challenge bindings are mandatory. Bounds are 10 seconds connect, 20 seconds authentication, 4 KiB logical auth frames within the 2 MiB socket frame bound, 16 KiB read/write buffers, 2 MiB + 64 KiB maximum write buffer, 5 seconds per write, 90 seconds idle receive, and 2 seconds close. Socket errors expose redacted enum/static messages. The connection layer never decrypts, auto-ACKs, advances cursors, or reconnects.

Before each outbound write the manager freshly checks exact persisted ciphertext, active pinned recipient, active local endpoint and expiry. An already started write can race owner revocation; previously delivered data cannot be recalled. Malformed/authentication/durable-state failures terminate the pump for operator attention rather than silently bypassing security checks.

Automated validation is deferred in #32. Actual loopback relay, identity/pairing, replay/restart, command deduplication, reconnect convergence and revocation were manually inspected. Production TLS deployment, Apple interoperability, hostile-frame campaigns and security audit remain unverified.

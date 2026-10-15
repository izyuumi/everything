# Blind relay

The optional `everything-relay` binary routes signed encrypted envelopes between explicitly enrolled devices. The native host remains authoritative. The relay has no conversation decryption keys, model credentials, plaintext application commands, or command IDs. It persists public identities, routing metadata, encrypted envelope JSON, and deduplication digests in a dedicated SQLite database.

`src/relay_protocol.rs` is the shared client/server wire contract. `relay/src/server.rs` owns HTTP/WebSocket admission, challenge authentication, sessions, delivery, and cancellation. `relay/src/database.rs` owns enrollment, sticky revocation, durable queues, quotas, deduplication, and rate accounting. SQLite runs on the `tokio-rusqlite` worker thread with the same rusqlite 0.37 version used by the host.

## Operator setup

The database path is required before the subcommand. It must be a dedicated relay database, separate from host storage. Newly created directories use mode 0700 on Unix, and the database file uses mode 0600. The process rejects a database symlink/nonregular file and unsupported application/schema IDs.

```sh
cargo build --release -p everything-relay
./target/release/everything-relay --database /var/lib/everything-relay/relay.sqlite3 enroll --keys /secure-transfer/host-public.json
./target/release/everything-relay --database /var/lib/everything-relay/relay.sqlite3 enroll --keys /secure-transfer/phone-public.json
./target/release/everything-relay --database /var/lib/everything-relay/relay.sqlite3 serve --origin wss://relay.example.com
```

An identity file contains only the `DevicePublicKeys` JSON representation: device UUID and base64 Ed25519/X25519 public keys. Enrollment is a local operator decision after identity verification. No remote enrollment endpoint, trust-on-first-use registration, administrative bearer-token service, or automatic enrollment during host/client pairing exists. Exporting and safely transferring a paired device's public identity is a client/host integration responsibility. The file reader rejects files over 4096 bytes and unknown identity fields.

Enrollment of the same active UUID and identical keys is idempotent. Different keys for an existing UUID are rejected. Revocation is permanent for that UUID, including re-enrollment with the original keys; a replacement identity needs a new UUID and a new explicit enrollment. Device records, including revoked records, are bounded at 4096 and are not automatically removed.

```sh
./target/release/everything-relay --database /var/lib/everything-relay/relay.sqlite3 devices
./target/release/everything-relay --database /var/lib/everything-relay/relay.sqlite3 status
./target/release/everything-relay --database /var/lib/everything-relay/relay.sqlite3 revoke DEVICE_UUID
```

The default backend bind address is `127.0.0.1:8787`. `/ws` is the WebSocket endpoint; `/health` returns HTTP 204 without identity or queue data. The binary serves HTTP/1 without TLS. For remote use, terminate TLS at an operator-managed reverse proxy, forward WebSocket upgrades to the loopback backend, bound proxy request sizes and connection rates, and prevent direct public backend access. Set `--origin` to the exact configured public `wss://` origin the clients expect. TLS provisioning, certificates, reverse-proxy deployment, and service installation are not performed by this binary. Loopback development may use `--origin ws://127.0.0.1:8787`; plaintext `ws://` origins on nonloopback hosts are rejected.

## Origin and authentication contract

`RelayOrigin` parses and canonicalizes a `ws://` or `wss://` origin, rejects credentials, paths beyond `/`, queries, fragments, missing hosts, and values over 512 bytes, and permits `ws://` only on a literal loopback IP or `localhost`. `.websocket_url()` appends `/ws`. Origins are explicitly configured by each client and are not learned from incoming HTTP host headers or proxy headers.

Every JSON frame has a snake-case `type` tag and a required `schema_version: 1`. Unknown fields and unsupported versions are rejected during deserialization. The shared `parse`/`to_bytes` methods additionally enforce the 2 MiB total JSON-frame limit. Byte strings use padded RFC 4648 standard base64. UUIDs use the shared non-nil UUID types. Frames may be sent as WebSocket text or binary messages; the server emits binary JSON.

Client frames are `begin_authentication`, `authenticate`, `send`, `acknowledge`, and `ping`. Server frames are `challenge`, `authenticated`, `accepted`, `delivery`, `acknowledged`, `pong`, and `error`. `send` and `delivery` contain only an `EncryptedEnvelope`. Errors use a fixed `RelayErrorCode`, never reflected external strings or conversational text. Applications must parse frames through the shared bounded interface.

The client first sends `BeginAuthentication { schema_version, device_id }`. Unknown or revoked UUIDs are rejected. The server returns `Challenge { schema_version, challenge }`, where `RelayChallenge` contains a version, fresh session UUID, device UUID, configured origin, issue/expiry times, and 32 random nonce bytes. The challenge expires after 20 seconds and is held only by that socket's authentication flow; it cannot be submitted to a different socket or reused for a second authentication.

Before signing, the client calls `challenge.signing_bytes(&expected_origin, local_device_id, now_ms)`. This rejects a different origin/device, unsupported version, expired or excessive lifetime, and issue time more than 120 seconds in the future. The deterministic byte encoding is:

1. ASCII `everything.relay.challenge.v1` followed by a zero byte.
2. The 2-byte version, 16-byte session UUID, and 16-byte device UUID.
3. The 2-byte UTF-8 origin byte length and exactly those origin bytes.
4. The 8-byte issue time and 8-byte expiry time in Unix milliseconds.
5. The 32-byte nonce.

All integers use unsigned big endian encoding. The client signs these bytes with `DeviceIdentity::sign_challenge`; that method also applies the cryptographic authentication domain specified in [the crypto wire contract](../security/relay-protocol.md). The client responds with `Authenticate { schema_version, session_id, signature }`. The server verifies its own original challenge against the pinned active Ed25519 key and responds with `Authenticated`. At most one authenticated socket per device is allowed. Authentication never changes pinned keys.

Unauthenticated attempts use a separate in-memory global admission limit of 1024 attempts per 60-second window. They do not debit a claimed device's authenticated rate budget. Only after a valid signature and active identity check is the successful authentication charged to that device.

## Durable routing and acknowledgments

For every `Send`, the server checks that the envelope sender equals the authenticated socket identity, resolves the active pinned sender keys, verifies signature/version/lifetime/ciphertext limits, and requires an active enrolled recipient. A single immediate SQLite transaction prunes expired records, rechecks sender and recipient activity, checks quotas, reserves a deduplication digest, and inserts ciphertext into the recipient queue. `Accepted { message_id, duplicate }` is sent only after commit. A network failure before that response can leave a committed envelope; the sender may safely retry the same complete envelope.

The deduplication key is the transport message UUID. Its SHA-256 digest covers the normalized serialized encrypted envelope, including authenticated routing metadata and signature. Reusing a UUID for different envelope bytes returns `message_conflict`. Resubmitting an identical queued or already acknowledged envelope returns `duplicate: true` and does not enqueue another delivery. Deduplication tombstones remain until the original signed envelope expiry, even after acknowledgment or device revocation. This does not replace application command-ID idempotency in the host.

Connected recipients receive up to 16 pending envelopes at a time. A broadcast wakeup triggers delivery after enqueue; a 30-second heartbeat also checks the queue, covering missed notifications. Delivered-but-unacknowledged envelopes remain in SQLite. Reconnect and process restart reset the socket's delivery window and redeliver them. Ordering follows relay acceptance time and transport message UUID; application event sequences, cursor gaps, and command ordering are resolved inside ciphertext by the endpoints.

A receiver sends `Acknowledge { message_id }` only after committing authenticated plaintext to its durable replay-protected inbox. The relay deletes a queued message only when the acknowledging socket is its recipient, then emits `Acknowledged`. Repeated or unknown recipient acknowledgments are idempotent. The relay cannot validate the receiver's inbox transaction and cannot prove a client actually processed a message. Clients must not acknowledge before durable acceptance.

Expiry removes both queued ciphertext and deduplication metadata. It runs on enqueue, at the `status` command, and every 30 seconds while serving. Pending queries exclude expired messages. Immediately before each individual write, the server rechecks active recipient, active sender, queued-row existence, and current expiry, so a slow batch does not keep earlier authorization alive. One write already in flight when a device is revoked or an envelope expires can complete; its write timeout is five seconds. Endpoint revocation and expiry checks remain necessary.

Revocation atomically marks a device inactive and removes queued traffic both to and from it. New authentication, sends, and acknowledgments then fail. Idle active sockets discover revocation at the next heartbeat, at most 30 seconds later, while every operation and delivery checks current state. Revocation cannot retract frames already delivered or erase plaintext/key material at a peer.

## Resource and persistence bounds

| Resource | Bound |
| --- | --- |
| Accepted TCP connections, including incomplete HTTP and authenticated WebSockets | 64 by default; `--max-connections` permits 1–256 |
| HTTP header read | 10 seconds, 16 KiB buffer |
| WebSocket authentication flow | 20 seconds; each auth frame at most 4096 bytes |
| Incoming WebSocket frame/message and shared JSON frame | 2 MiB |
| Sender plaintext/ciphertext | Crypto contract limits of 1 MiB plaintext plus 16-byte AEAD tag |
| WebSocket write | 5 seconds, bounded write buffer |
| Socket heartbeat/idle limit | 30/90 seconds |
| Per authenticated device | 240 inbound frames and 32 MiB per UTC minute |
| Per recipient queue | 64 messages and 16 MiB of serialized encrypted envelopes |
| Global queue | 1024 messages and 128 MiB of serialized encrypted envelopes |
| Retained deduplication IDs | 100,000; new messages are refused when full |
| Enrolled identities, including revoked identities | 4096 |
| Delivery window | 16 unacknowledged envelopes per connection |

The TCP semaphore permit travels with the stream through HTTP and WebSocket upgrades, so an incomplete handshake does not bypass the global connection bound. The operating system listen backlog and reverse proxy have their own independent limits. There is no claim of general distributed denial-of-service resistance; a saturated global admission budget can delay legitimate clients.

Device rate counters persist across reconnects and relay restarts. Fixed minute windows can admit a burst across a boundary; the count and byte bounds are intentional admission controls, not a constant-rate scheduler. Each received application frame and WebSocket ping/pong is charged after authentication. Violations return an error and end the session. Unauthorized clients cannot consume another identity's persistent budget by claiming its UUID.

Queue and metadata counters update through SQLite triggers in the same transaction as insertion/deletion. Per-device and global count/byte quotas are checked before accepting new ciphertext. The SQLite database uses WAL, `synchronous=FULL`, foreign keys, a five-second busy timeout, periodic WAL checkpoints, a 16 MiB retained-journal target, and a maximum of 131,072 database pages (512 MiB at the initial default 4096-byte page size). WAL size targets do not replace queue quotas and are not a strict bound during an active transaction. Empty database pages may remain allocated after expiry; no backup or restore subsystem exists.

SIGINT/SIGTERM cancels listener admission and active sockets, tracks both HTTP and upgrade tasks through shutdown, and closes the SQLite worker after tasks finish. Socket writes and close attempts are bounded. The task drain reports a timeout after ten seconds rather than claiming cleanup succeeded. Abrupt process termination leaves committed ciphertext available on restart. Shutdown under blocked external SQLite writers or failed storage has not been manually verified.

## Validation and remaining integration

The Linux worktree passed formatting, all-target checking, strict Clippy, and release compilation with Rust 1.99.0. An actual Rust 1.85 compiler was not available for minimum-version validation. The CLI help, enrollment, idempotent re-enrollment, device listing/status, and revocation paths were exercised with fresh disposable public identities and SQLite storage. A disposable native WebSocket client verified origin-bound signed authentication, unknown-device rejection, offline delivery, duplicate enqueue handling, recipient decryption, reconnect redelivery, acknowledgment, no redelivery after acknowledgment/resubmission, durable queue recovery after server restart, expiry cleanup, the 64-message per-device cap, persisted rate limiting across reconnects, revocation of an active session, and refusal to send to a revoked recipient. Both SIGTERM shutdowns exited successfully. Relay storage contained ciphertext and routing/public identity metadata; the relay was never given device private keys or plaintext application payloads.

Independent implementation review found and corrected preauthentication device-budget consumption and stale batch revocation/expiry checks. This is not a security audit. Shared version checks, counters, global byte/count capacities, deduplication-cap exhaustion, hostile frame coverage, slow HTTP/write timeout behavior, and crash consistency beyond an orderly restart still require broader automated validation tracked in #32. No automated tests, fixtures, or vectors were created.

TLS/reverse-proxy deployment, Apple clients, protected host identity storage, host durable replay/inbox integration, application command dispatch, endpoint revocation propagation, and encrypted event synchronization are outside this relay implementation. #22 integrates the endpoint transport against this shared contract. A relay acknowledgment is transport acceptance, not evidence that the host executed a command.

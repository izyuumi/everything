# Paired devices and encrypted synchronization

`devices::DeviceEndpoint` connects an authoritative host or a replica to the native blind relay. A replica names exactly one authoritative host in its immutable local endpoint configuration. Transport is optional: constructing the endpoint does not start networking, execute commands, or grant tools. The host and its SQLite database remain authoritative.

## Responsibilities

`src/devices.rs` owns private identity loading, local pairing ceremonies, pinned peer metadata, scope checks, authenticated envelope receipt, command reservation, and synchronization packet handling. `src/devices/session.rs` runs the asynchronous connection and durable outbox pump. `transport::remote::RelayConnection` owns native WebSocket authentication and bounded socket I/O. `src/storage/transport.rs` owns transactions through the existing Store connection; it never opens an additional database connection.

All endpoint methods other than relay pumping and notifications are synchronous. Runtime callers must use `spawn_blocking` or another blocking worker boundary. `run_relay` moves every Store/cryptography operation onto a blocking worker and awaits it. Cancellation closes networking and retains queued work. Only one relay pump can run on an endpoint and its clones.

## Identity and host bootstrap

`load_or_create_identity_in` accepts an already verified capability directory handle. Host bootstrap must construct that handle from the service's descriptor-based `StateDirectory`; it must retain the service host lock. The path convenience loader checks its leaf directory but does not replace the service's ancestor trust checks.

Linux and Apple platforms require an owner-owned directory with mode 0700. `device-identity-v1.bin` is a fixed, versioned binary file containing the device UUID and existing crypto module's Ed25519/X25519 secret seeds. It must be a regular owner-owned, single-link file with mode 0600. File opens refuse symlinks. Initialization writes and fsyncs a new owner-only temporary sibling, publishes with atomic no-replace rename, and fsyncs a readable directory descriptor. Concurrent initializers read the winning complete identity. A crash before publication can leave an unpublished private temporary file; it never becomes an identity automatically. Incomplete or unsupported final files produce an error and are never overwritten.

The endpoint database is private host/client state containing decrypted inbox messages, results, event replicas, and permission metadata. E2EE protects communication with the relay; it does not encrypt this local database or defend against another process already running as the same user. No private key is uploaded to the relay. Secret invitation and identity serialization buffers use `Zeroizing`; allocator, OS, and QR rendering internals do not promise complete memory erasure.

## Pairing and scope

A host creates a short-lived invitation and a real SVG QR containing its signed invitation and random pairing secret. The owner transfers this secret out of band. Only a replica may create the response, and only a host may approve it. Both sides must explicitly confirm the other identity's complete displayed fingerprint. The existing crypto protocol supplies signed/HMAC-bound request and acceptance proofs; the endpoint adds no cryptographic primitive.

Each side approves a local label and `DeviceAccess`: all conversations or at most 64 selected conversation IDs, command submission, and optional permission-resolution submission. Permission resolution requires command-submission access. A replica can pin only its configured authoritative host. A pin includes the full validated public key set and access policy. A pairing ID cannot bind a second identity; an existing identity cannot silently change keys, widen scope, or become unrevoked. Changes require an explicit future owner lifecycle operation rather than a model command.

Pending ceremony secrets remain in bounded memory, at most eight invitations and eight responses. Expired entries are discarded on the next ceremony operation. Restart abandons unfinished ceremonies; completed pins and used pairing IDs persist. Pairing does not enroll keys into a relay: the relay operator separately enrolls public keys using the existing local administrative command.

## Receipt and execution

A received envelope must authenticate against an active pinned sender. Replicas accept envelopes only from their authoritative host. The existing crypto module validates signature, recipient, lifetime, and authenticated encryption before invoking `DurableInbox`.

One immediate SQLite transaction checks the exact pinned sender metadata again, confirms the local endpoint is active and the recipient matches, and inserts both replay identity and plaintext inbox payload in the same row. The relay delivery ACK is sent only after this transaction succeeds or after an authenticated exact duplicate is recognized. Conflicting reuse of a message ID fails closed. The relay's ACK removes its ciphertext delivery copy; it never means a command ran or an event was applied.

`process_inbox` validates versioned application packets, rejects unauthorized or malformed operations durably, and returns typed `ReceivedOperation` values. It never invokes the agent or Tool Broker. Host command dispatch must reserve first, then pass the original command ID through the native runtime's own idempotent acceptance path. A `ReceivedCommand` with completed/rejected state must never execute. A pending row with `newly_reserved = false` can be a retry or restart recovery; the host reconciles its native command outcome before dispatch.

The command journal stores the full original command JSON and a domain-separated SHA256 digest, including additive command fields, before any host-generated event IDs or timestamps. It binds command ID, sender, conversation, issuing device, and original payload. Changed reuse is rejected. Completion and the encrypted result reply commit together; completed duplicate requests remain pending until their replacement reply and inbox completion commit together. Completed journal payloads may be archived after seven days, retaining identity/state/digests; an archived duplicate receives the durable terminal state with an omitted result. Pending commands never expire implicitly.

Replica command-result notifications retain their plaintext inbox row until `acknowledge_command_result` is called after the application has durably saved its own result. Repeated processing can return the same result notification. An application must drain these acknowledgments to release inbox capacity. This transport does not claim exactly-once external tool effects; the native runtime and Tool Broker own their journals and recovery decisions.

## Event convergence

The native event codec stores a complete `EventEnvelope` in each public event row. A host checks its IDs, conversation, sequence, timestamp, and event kind against SQLite metadata before sending it. Provider continuation/checkpoint state is excluded from public event synchronization.

Callers request a known, authorized conversation using its durable replica cursor. Each request carries one conversation cursor and at most 256 events. Batches fit the protocol's 512 KiB encoded bound, with contiguous sequence numbers and immutable event identities. A host captures a head and stops the batch at that head. An individual event too large for a batch is an explicit error; events are never silently skipped.

Replica application inserts or verifies immutable event rows, advances its durable replica cursor, queues an encrypted application cursor ACK, queues a continuation request when `has_more`, and finishes the inbound message in one transaction. A gap is rejected and requests the current durable cursor again. Conflicting event identity/content never overwrites the replica. The host advances its device cursor only when a pinned, authorized peer sends an application ACK matching a persisted offer's message ID, conversation, through-sequence, and last event ID. Merely sending a batch or receiving relay Accepted does not advance that cursor.

`run_relay` supplies durable delivery and wakeups; the host/client dispatcher calls `process_inbox` and drains typed operations. Applications call `request_sync` on connection/resume for the conversations they display. There is no unrestricted conversation-enumeration endpoint. A request queued while disconnected persists across restart. Lost delivery responses are safely replayed; replicas verify duplicate events before acknowledging them.

## Revocation and limits

Owner-only host `revoke_device` persists sticky revocation, removes queued ciphertext and pending offers to that peer, rejects its pending inbox and commands, and prevents subsequent receive transactions. Every outbound frame gets a fresh pinned-peer/local-endpoint/expiry check against its exact still-persisted ciphertext immediately before the asynchronous write. A write already started at revocation cannot be recalled. Revocation cannot erase plaintext already held by another device.

Host-local revocation and relay enrollment/revocation are separate trust boundaries. The operator must also revoke the public identity in the relay to remove blind queued traffic and disable relay authentication. The host can process a signed encrypted self-revocation notice on a replica, but local revocation does not claim that an offline replica received such a notice.

Inbox pending bounds are 1,024 messages / 64 MiB globally and 128 / 8 MiB per peer. Outbound bounds are 1,024 / 128 MiB globally and 64 / 16 MiB per peer. All read pages retain the Store's 1,000-row and 16 MiB aggregate ceilings; callers reduce page sizes when necessary. There are at most 100,000 replay markers, 100,000 retained command identities, 100,000 used pairing IDs, 4,096 device registrations, and 4,096 unacknowledged sync offers. Exhaustion is an explicit error, not silent deletion of accepted work or deduplication identity. Command outcomes are bounded to 64 KiB, with storage reserved at acceptance. Authoritative/replica event histories remain persistent application data.

Maintenance reports expired outgoing message IDs, removes expired completed replay markers and expired sync offers, and archives old terminal command payloads. Accepted pending inbox entries survive envelope expiry: lifetime controls acceptance from the network, not an implicit cancellation of already accepted work.

The pump uses durable attempts/not-before times, at most 16 unaccepted sends, one outbound frame per loop, a 30-second heartbeat, and exponential reconnect delays capped at 32 seconds. Network/TLS/timeout and transient relay-capacity errors retry; authentication, conflicting identity, malformed protocol, and durable-state failures stop with an error. Watch status and the returned result expose failures. WebSocket bounds and authentication behavior are documented in [transport security](security.md) and [device-sync contract](../contracts/device-sync.md).

## Validation and remaining integration

Disposable manual execution against the real relay verified private identity reopen, QR/proof pairing, fingerprint rejection, selected-conversation enforcement, replay and pending inbox recovery, changed-command rejection, completed-command deduplication/result delivery, cursor ACK behavior, and client convergence after disconnect/reopen with a queued sync request. Formatting, Rust compilation, Clippy, and release builds are recorded with the implementation PR.

Service command dispatch and native client UI consume these APIs in their respective integration issues. Production TLS deployment, Apple interoperability/builds, hostile peer/frame campaigns, filesystem fault injection, and security review remain unverified. Automated verification is deferred to issue #32 under the current no-new-tests instruction; this work is not a production security audit.

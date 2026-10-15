# Authenticated local IPC

`src/transport/local.rs` defines the Unix endpoint contract. It uses the current [domain and sync contracts](events.md), not an HTTP gateway or another harness protocol. A supported background host exposes it through an owner-only Unix socket.

## Transport and authentication

The endpoint is `<state directory>/host.sock`. The physical absolute socket path is at most 100 bytes for portability across supported Unix platforms. The state directory is owned by the host user with mode 0700 and the socket with mode 0600. Descriptor-relative path validation rejects symlinks, unsafe ancestors, non-socket endpoints, and unsafe lock files. The host and client both verify that the kernel-reported peer UID equals the current effective UID; setuid execution is unsupported.

Each connection carries one request and one response, then closes. A frame is a four-byte unsigned big-endian byte count followed by that many bytes of UTF-8 JSON. Length zero and lengths above `MAX_LOCAL_FRAME_BYTES` (1 MiB) are rejected before allocation. The JSON parser rejects unsupported schema versions through `SchemaVersion`. No newline escaping or line-based framing is involved.

The endpoint supports at most 32 concurrent connection tasks and has a 32-call bounded application inbox. Excess connections are closed. Default server frame read/write timeout is 10 seconds; default command/reply timeout is 120 seconds. `LocalOptions` permits frame limits up to 120 seconds and command limits up to one hour, with positive values required. The client bounds connect/write with the frame timeout and the entire exchange with its command timeout. Cancellation releases connection and parser resources. Connection failures emit structured tracing events; malformed requests cannot terminate the accept loop.

## Requests

`LocalRequest` contains required `schema_version`, `request_id: CommandId`, and a `payload` tagged by snake-case `kind`. `LocalRequest::new(payload)` selects the current schema and a unique request ID, except submitted commands reuse their command ID. Call `validate()` after editing public fields; framed server handling validates every decoded request.

| Payload kind | Data and semantics |
| --- | --- |
| `status` | Return actual endpoint PID, process start timestamp, and application version |
| `submit_command` | A typed `CommandEnvelope`; command ID must equal request ID and remote-device attribution must be absent |
| `synchronize` | A validated `SyncRequest` with exactly one conversation cursor |

Submitted commands serialize to at most 256 KiB, leaving room for a bounded durable event and synchronized delivery. A host may enforce tighter semantic limits. Local authentication does not bypass command authorization, role checks, task-state checks, permission policy, command deduplication, or atomic persistence.

The single-conversation sync request avoids an unbounded aggregate response. `max_events` retains the shared 1–256 bound. One returned `SyncBatch` is at most 512 KiB before its IPC wrapper and therefore fits the 1-MiB frame. Clients paginate by accepting a batch atomically and then using `next_cursor()`; they never advance a durable cursor on receipt alone.

## Responses

`LocalResponse` contains `schema_version`, the exact `request_id`, and a tagged payload. `validate_for(&request)` checks matching identity/version, the response kind, frame bounds, and sync cursor semantics.

| Payload kind | Data and semantics |
| --- | --- |
| `status` | `HostStatus { process_id, started_at, version }`; endpoint availability only |
| `command_accepted` | `command_id`, host `accepted_at`, and `duplicate`; durable acceptance only |
| `synchronized` | One `SyncBatch` matching the requested cursor and event cap |
| `rejected` | `LocalFailure { code, message }`; bounded sanitized message |

Failure codes are `invalid_request`, `permission_denied`, `unavailable`, `busy`, `timeout`, and `internal`. Rejection messages are at most 1024 bytes and reject NUL. A missing inbox consumer or dropped oneshot reply yields `unavailable`. Processing timeout can leave a command durably accepted; the response says its outcome may already be durable. Retry the original command ID and exact contents to resolve ambiguity. A transport error or dropped client never proves that a command did not execute.

## Host integration

`LocalServer::bind(state_directory, options)` returns the socket-owning server and `mpsc::Receiver<LocalCall>`. Binding creates or checks the private state directory and acquires its singleton lock. `server.run(cancellation)` accepts authenticated requests and owns all connection tasks. Status is answered from real server process metadata; other requests are passed through the bounded channel.

`LocalCall` exposes the authenticated `peer_uid`, validated typed `request`, a request-lifetime `cancellation` token, and `respond(payload)`. There is no default command handler and no automatic acceptance response. The host consumes a call, applies authorization, performs the durable command/event transaction or bounded store read, then responds. SQLite calls must run through a blocking boundary. Only a successfully committed command can receive `command_accepted`; exact duplicate commands return their durable original outcome with `duplicate: true`.

The call token bounds reply work when a connection or server stops. It is not the task cancellation authority. Once a command is accepted, its task lifetime is owned by the persistent background runtime and continues independently of the request connection or UI process. Task cancellation requires its own authorized `cancel_task` command.

`LocalClient::new(options)` constructs a client. `request(state_directory, request, cancellation)` validates the private endpoint, authenticates its peer, sends one bounded frame, and validates the typed response. It does not open SQLite directly or silently start another host.

## Validation limits

Linux formatting, compilation, strict Clippy, and release build passed. A disposable real socket run observed status exchange, singleton rejection, absent-processor rejection, oversized-frame isolation, pre-cancelled client rejection, socket cleanup after cancellation, stale endpoint recovery, and rejection of a symlink socket and public state directory. Same-user peer credentials were exercised by both ends; rejection of a different UID was not runtime-verified. The complete accepted-command transaction, event-store synchronization, daemon CLI integration, macOS credential semantics, and launchd restart behavior require their owning integration and platform verification. Automated checks are deliberately deferred to [#32](https://github.com/izyuumi/everything/issues/32); no tests, mock services, or fixtures are introduced here.

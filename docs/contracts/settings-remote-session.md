# Remote restricted Settings session

A paired device with an explicit owner grant can run one restricted Settings chat on the Host over the existing E2EE [Settings transport](settings-access-transport.md). It reuses the [restricted runtime](restricted-settings.md) and its seven tools; there is no second runtime, provider loop, management service or OwnerQuery/OwnerOperation forwarding. Request and reply DATA are in [requests](settings-access-status.md) and [replies](settings-access-replies.md).

Owners: `src/host/settings/service/paired/session.rs` (plan, consent check, runtime calls), `src/host/settings/service/paired/remote.rs` (preparation), `src/host/settings/authority/paired.rs` (paired session parents), `src/host/settings/agent.rs` (`OpenPaired`), `src/host/settings/agent_tools.rs` and `service/projection.rs`/`service/writes.rs` (scope), `src/devices/settings_ingress/work/build.rs` (build).

## Authority and consent

- Pairing grants nothing. Only an Active owner [grant](settings-grants.md) whose `session_consent` is `ConfiguredProvider { provider, model, recipient }` allows a session; consent `None` answers `outside_scope`. An LLM or a remote request cannot create or widen a grant.
- `session_open` captures the Host's configured provider exactly as a native Open does (no model request is sent). If the captured provider, model or recipient differs from the grant's consent tuple, the chat is closed at once and the reply is `outside_scope`. A matching Open discloses the tuple, binding digest and tool names in `session_opened`.
- Every `session_send` must echo that binding digest, as the native Send does. The owner's consent is the grant tuple; the device's per-turn acceptance is the echoed digest. Owner consent cannot be confirmed from the device.
- Each request still passes the transport's per-request checks under the Host mutation guard: live pin, Active grant and the request's full `generation`.

## Paired session parent

The Host keeps at most one paired parent per device and four in total, inside the existing native-session table. A parent records the device's full public keys and the grant id/revision it was created under, and lives at most 300 s and never past the grant's expiry. It has no native proof: native callers cannot use it, and native-only routes (confirmations, action details, credential, pairing and device ceremonies) refuse it.

- `session_open` reuses the device's parent while it is live, has the same pin and grant revision and still owns a live chat; otherwise it retires that parent and creates a new one. An Open replay with the same `open_request_id` returns the original chat while it lives.
- One live chat per device: a new `open_request_id` while a chat is live answers `capacity`. Close the chat first.
- The parent is not tied to the relay connection. After a reconnect the device rereads `access_status` for the new generation and keeps polling the same `chat_id`.
- The parent is retired (closed and cancelled, which closes its chat through the runtime) when the owner changes the grant (`set_settings_grant`, new revision or revocation), when the device is revoked through the owner `RevokeDevice` route or the native revocation effect, when a remote request finds the grant no longer Active, and when a tool call finds the grant head changed. Retirement happens under the Host mutation guard in those owner routes.

## Execution scope

The chat is the restricted runtime's ordinary child with the paired parent as its parent; the journal origin of its writes is `SettingsAgent { session_id, native_session_id }` with the paired parent id.

| Tool | Paired chat behaviour |
| --- | --- |
| `settings.list` | Descriptors of readable keys only |
| `settings.get` | Snapshot entries of readable keys only (also the per-turn context capture) |
| `settings.update` | Every changed key must be writable; checked with a fresh grant-head read while the update holds the Host mutation guard, just before the file write |
| `settings.request_action`, `devices.list`, `devices.pair`, `devices.revoke` | Rejected before admission |

Each list/get/update call rereads the grant head; a revoked, reduced, expired or rotated grant or a revoked pin rejects the call and retires the parent. Grant storage only allows `memory.embeddings.enabled` and `memory.embeddings.dimensions` as writable keys, so no paired write needs native confirmation. Credential entry, pairing, revocation, grant changes, execution policy, recipient consent and lifecycle stay local-owner-only (`host_action` answers `host_action_required`).

## Result delivery

A turn runs for up to 120 s, beyond one 30 s request. The device polls with `session_poll` (a new `query_id` each time); each poll returns the current status, including after the turn finished. Committed text is paged by `text_offset` (at most 16 KiB escaped per reply). Operation ids in the status identify the turn's Settings writes; the authoritative effect is read back through the scoped `snapshot` and `changes` requests, which only show readable keys. A closed or expired chat's final status can be polled while the Host still retains its parent; a newer Open for the same device replaces the parent and that history.

`session_send` and `session_open` are replay-safe: the same `turn_id` with the same digest and text returns the original `session_accepted`, a changed text is `conflict`, and the same `open_request_id` returns the original Open. A device that lost a reply resends the same identity instead of inventing a new one.

## Build and guard

The reply build normally holds the Host mutation guard throughout. For session kinds it checks access and generation under the guard, releases the guard while it waits for the runtime (bounded by the request deadline minus 2 s), then reacquires it for publication, which rechecks current access and requires the same generation. A grant change in between drops the reply instead of publishing it. A runtime that does not answer in time yields `unavailable`; the device can resend the same identity.

## Data boundaries

Chat text, tool data and model output stay in the restricted runtime's fileless private store and RAM; there is no ordinary conversation, inbox, wiki, memory or embedding intake. Request text is held only in the RAM query cell. The sealed reply (status text) is stored only as the existing encrypted private outbox row until sent. No credential, invitation or secret transfer is carried in either direction.

## Limits and validation

Restricted-runtime limits apply unchanged: two live chats Host-wide (native and paired together), eight turns per chat, one running turn at a time. Validation evidence is in [Settings validation](../development/settings-validation.md#remote-settings-session-walkthrough). No Apple client sends these requests yet (#117). Deferred automated coverage is tracked in [#32](https://github.com/izyuumi/everything/issues/32).

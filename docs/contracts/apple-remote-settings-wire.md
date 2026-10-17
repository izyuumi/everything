# Apple remote Settings query wire data

`Shared/Settings/RemoteSettingsWire.swift` defines the request encoder and bounded response decoder for every Host query kind ([Host request DATA](settings-access-status.md), [reply DATA](settings-access-replies.md)), plus the legacy Bind codec, registered in both Apple targets. The codec authenticates nothing and grants no Host permission. The [query client](apple-settings-access-client.md) is its only caller. The legacy Snapshot/inspection [Read codec](settings-scoped-reads.md) and [Update codec](apple-settings-update-wire.md) and the Bind shape are not accepted by the Host and have no caller.

## Requests

- Wrapper: `schema_version=1`, `payload{kind=settings,version=1,host_id,message}`.
- Query: `message{kind=query,query_id,body}`. `RemoteSettingsAccessRequest(hostID:queryID:time:query:generation:)` encodes `RemoteSettingsQuery`: `access_status{time}`, `snapshot{generation,keys,time}`, `changes{generation,after_sequence,limit,time}`, `update{generation,request_id,expected_revision,changes,time}`, `host_action{action,time}`, `session_open{generation,open_request_id,time}`, `session_send{generation,chat_id,turn_id,provider_binding_sha256,text,time}`, `session_poll{generation,chat_id,text_offset,time}`, `session_close{generation,chat_id,time}`. A generation is required exactly for the generation-bearing kinds.
- Query validation before encoding: Snapshot keys non-empty, Host-scope and in contract order; Changes limit 1–64; Update non-nil `request_id`, positive revision, unique allowlisted Host keys encoded with the shared `SettingChange` DTO; canonical non-nil UUIDs; digest 64 lower-case hex; session text non-empty after trimming, no NUL, at most 8 KiB; text offset at most 16 KiB.
- Bind: `message{kind=bind,generation,binding_request_id,time,requested_parent_expires_at}`, with `issued_at < requested_parent_expires_at <= issued_at+300000`.
- `RemoteSettingsRequestTime`: expiry after issue and at most 30,000 ms later. IDs and times are supplied by the caller; encoding mints no ID, observes no clock and renews nothing.
- Complete encoded request at most 32,768 bytes; UUIDs canonical lowercase nonzero.

## Responses

`decodeAccessReply` and `decodeBindReply` parse with `WireValue.parse(maximum:32768)`: exact field sets, duplicate-key rejection, no trailing JSON, canonical UUIDs, signed-range integer times. `in_reply_to` (encrypted request ID), the inner query/Bind ID and the response envelope ID are distinct.

| Decoder | Correlation | Body and generation |
| --- | --- | --- |
| AccessStatus summary | `family=query,query_id` | `kind=access_status,summary`; message generation equals summary generation |
| Snapshot / Changes | `family=query,query_id` | shared `SettingsSnapshot` / `SettingsChangePage` DTOs with their own validation; generation required |
| Update | `family=query,query_id` | `request_id` and `operation{phase,updated_at,write,application}` decoded with the shared operation DTOs |
| Host action | `family=query,query_id` | `kind=host_action_required,action` (closed action set) |
| Session | `family=query,query_id` | `session_opened{chat}` (seven provider tool names required), `session_accepted{chat_id,turn_id}`, `session_status{status}` (page within `text_bytes` ≤ 16 KiB, at most 8 operation ids, at most 4 attempts) |
| Denial | `family=query,query_id` | `kind=denied,reason` from the closed reason set; generation null; packet at most 2048 bytes. `grant_unavailable` keeps its own body case |
| Bind | `family=bind,binding_request_id` | `kind=bound,binding_request_id,parent,expires_at`; IDs match; generation equals parent generation |

Summary: `generation`, `expires_at`, at most five ordered readable keys, at most two writable embeddings keys contained in the readable set, `session_consent` (`none` with null revision, or `configured_provider{provider,model,recipient}` with positive revision; model at most 256 UTF-8 bytes, recipient at most 4096 bytes with no userinfo/query/fragment) and `configured_settings_revision`. Invalid data throws `WireError.invalid` or `size`; nothing becomes Bound, denial or Saved. Descriptions are fixed redacted strings.

## Sealing and verification

- `DeviceIdentity.sealSettings` has typed overloads for Access, Bind, Snapshot and inspection requests. It reads wall/monotonic time inside the actor, requires the recipient to be the request Host, and sets the signed outer expiry equal to the inner expiry (duration at most 30,000 ms). Envelope at most 49,152 bytes; `Send` frame at most 364,544 bytes. Ordinary `seal` keeps its seven-day lifetime.
- `RelayConnection.receive()` returns `ReceivedRelayFrame` with the original relay message byte count; the private 364,544-byte ceiling is applied after authentication and Settings routing.
- `DeviceIdentity.receiveClassified` authenticates once and classifies a delivery as ordinary, settingsAccess, settingsBind, settingsSnapshot, settingsInspection or refused. Unsupported private forms get metadata-only refusal.
- Verified replies: response duration positive and at most 60,000 ms; envelope at most 49,152 (every non-denial query body and Bound) or 4,096 (denial). `commitMetadata` records ciphertext metadata (`payload=NULL`, `done=1`) in one transaction that rechecks Host pin, local identity and disabled state.
- Access display projection additionally requires the reply's sealed creation time to be no earlier than the request wall floor minus 5 s, and its expiry no later than the request expiry.

## Service ownership

`ReplicaService` owns one socket scope, one pending Access/Bind/Snapshot/Inspection original and one receive ticket on the existing runner/task group. The scope is created only after relay authentication and Store pin/local/disabled checks. Retired originals and receive data are held until the owning helper returns. Adoption requires a file-private, single-use qualification bound to the exact Fresh committed reply and current original. The Access RAM cache holds only the finite display, generation, public identities, fixed fences and a weak scope; revocation or identity mismatch clears it. Every query kind shares the Access pending cell; non-Access results go to one RAM result keyed by the request handle ([query client](apple-settings-access-client.md)).

## Validation

Local compile/static checks. The query encoder and decoder were exercised against the real Rust Host for every kind in a disposable Simulator walkthrough ([record](../development/settings-validation.md#ios-remote-settings-client-walkthrough)); the legacy Bind, Read and Update codecs were not. Automated coverage is deferred to [#32](https://github.com/izyuumi/everything/issues/32).

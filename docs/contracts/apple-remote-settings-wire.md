# Apple AccessStatus and Bind wire data

`Shared/Settings/RemoteSettingsWire.swift` defines request encoders and bounded response decoders for AccessStatus and Bind, registered in both Apple targets. The codec authenticates nothing and grants no Host permission. Snapshot and inspection use the [Read codec](settings-scoped-reads.md); Update uses the [Update codec](apple-settings-update-wire.md).

The iOS Settings screen exposes only the explicit [Access check](apple-settings-access-client.md). The Host in this tree answers only `AccessStatus`; Bind, Snapshot, inspection and Update packets are recorded as rejected ([Host Access transport](settings-access-transport.md)). Those other clients remain in source without a screen caller or responding Host.

## Requests

- Wrapper: `schema_version=1`, `payload{kind=settings,version=1,host_id,message}`.
- AccessStatus: `message{kind=query,query_id,body{kind=access_status,time}}`; no generation or parent.
- Bind: `message{kind=bind,generation,binding_request_id,time,requested_parent_expires_at}`, with `issued_at < requested_parent_expires_at <= issued_at+300000`.
- `RemoteSettingsRequestTime`: expiry after issue and at most 30,000 ms later. IDs and times are supplied by the caller; encoding mints no ID, observes no clock and renews nothing.
- Complete encoded request at most 32,768 bytes; UUIDs canonical lowercase nonzero.

## Responses

`decodeAccessReply` and `decodeBindReply` parse with `WireValue.parse(maximum:32768)`: exact field sets, duplicate-key rejection, no trailing JSON, canonical UUIDs, signed-range integer times. `in_reply_to` (encrypted request ID), the inner query/Bind ID and the response envelope ID are distinct.

| Decoder | Correlation | Body and generation |
| --- | --- | --- |
| AccessStatus summary | `family=query,query_id` | `kind=access_status,summary`; message generation equals summary generation |
| AccessStatus denial | `family=query,query_id` | `kind=denied,reason=grant_unavailable`; generation null; packet at most 2048 bytes |
| Bind | `family=bind,binding_request_id` | `kind=bound,binding_request_id,parent,expires_at`; IDs match; generation equals parent generation |

Summary: `generation`, `expires_at`, at most five ordered readable keys, at most two writable embeddings keys contained in the readable set, `session_consent` (`none` with null revision, or `configured_provider{provider,model,recipient}` with positive revision; model at most 256 UTF-8 bytes, recipient at most 4096 bytes with no userinfo/query/fragment) and `configured_settings_revision`. Invalid data throws `WireError.invalid` or `size`; nothing becomes Bound, denial or Saved. Descriptions are fixed redacted strings.

## Sealing and verification

- `DeviceIdentity.sealSettings` has typed overloads for Access, Bind, Snapshot and inspection requests. It reads wall/monotonic time inside the actor, requires the recipient to be the request Host, and sets the signed outer expiry equal to the inner expiry (duration at most 30,000 ms). Envelope at most 49,152 bytes; `Send` frame at most 364,544 bytes. Ordinary `seal` keeps its seven-day lifetime.
- `RelayConnection.receive()` returns `ReceivedRelayFrame` with the original relay message byte count; the private 364,544-byte ceiling is applied after authentication and Settings routing.
- `DeviceIdentity.receiveClassified` authenticates once and classifies a delivery as ordinary, settingsAccess, settingsBind, settingsSnapshot, settingsInspection or refused. Unsupported private forms get metadata-only refusal.
- Verified replies: response duration positive and at most 60,000 ms; envelope at most 49,152 (summary/Bound) or 4,096 (denial). `commitMetadata` records ciphertext metadata (`payload=NULL`, `done=1`) in one transaction that rechecks Host pin, local identity and disabled state.
- Access display projection additionally requires the reply's sealed creation time to be no earlier than the request wall floor minus 5 s, and its expiry no later than the request expiry.

## Service ownership

`ReplicaService` owns one socket scope, one pending Access/Bind/Snapshot/Inspection original and one receive ticket on the existing runner/task group. The scope is created only after relay authentication and Store pin/local/disabled checks. Retired originals and receive data are held until the owning helper returns. Adoption requires a file-private, single-use qualification bound to the exact Fresh committed reply and current original. The Access RAM cache holds only the finite display, public identities, fixed fences and a weak scope; revocation or identity mismatch clears it.

## Validation

Local compile/static checks only. No encoding/decoding execution, Rust/Swift interoperability, relay or UI behaviour has been observed. Automated coverage is deferred to [#32](https://github.com/izyuumi/everything/issues/32).

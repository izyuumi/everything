# Apple AccessStatus and Bind wire data

`Shared/Settings/RemoteSettingsWire.swift` defines pure request encoders and bounded response decoders for the generation-free AccessStatus bootstrap and subsequent Bind. The leaf has no project membership or transport consumer yet. It does not authenticate a response, grant access, create or adopt a parent, send an envelope or expose Host controls. Existing authenticated private Settings routing still refuses these packets before ordinary plaintext persistence.

## Finite request shapes

Both request encoders fix the complete `schema_version=1`, `payload{kind=settings,version=1,host_id,message}` wrapper. AccessStatus uses `message{kind=query,query_id,body{kind=access_status,time}}`. Bind uses `message{kind=bind,generation,binding_request_id,time,requested_parent_expires_at}`. No actor, principal, credential, owner operation, native proof or arbitrary request body is accepted.

`RemoteSettingsRequestTime` retains the supplied original positive signed-range interval: expiry is after issue and at most30000 milliseconds later. `RemoteSettingsGeneration` contains only nonzero `grant_id`, positive signed-range `grant_revision`, nonzero `host_epoch` and nonzero `connection_epoch`. Bind requires `issued_at < requested_parent_expires_at <= issued_at+300000`, with checked arithmetic and signed-range expiry. Request IDs and times are explicitly supplied and immutable; encoding does not mint an ID, observe now, renew a deadline or retry an intent. AccessStatus has no generation or parent.

Actual request encoding uses the existing `WireValue` serializer and checks the entire output against32768 bytes. UUIDs serialize as canonical lowercase nonzero strings. These are post-serialization byte ceilings, not allocator or secure-erasure bounds. No product serialization was executed for this source slice.

## Strict response decoding

`RemoteSettingsWire.decodeAccessReply` and `decodeBindReply` accept complete plaintext `Data`. They use the actual existing `WireValue.parse(maximum:32768)` before typed body decoding, including Foundation syntax validation, decoded duplicate-key rejection and existing depth/key bounds. Every nested object has its exact allowed and required fields. Missing fields, unknown fields, positional arrays, duplicate or escaped duplicate keys, unsupported tags/versions/directions, trailing JSON, noncanonical or zero UUIDs, and noninteger or out-of-range timestamps fail. No separate parser or generic retained JSON result is introduced. The existing `SettingsWire.decode` is not used as a duplicate-key boundary.

Both response families require `payload{kind=settings,version=1,host_id,message}` and `message{kind=response,in_reply_to,correlation,generation,body}`. The actual envelope request ID in `in_reply_to` remains distinct from the original inner query/Bind ID and the response envelope ID. Parsing their fields does not establish that they match a genuine pending original.

| Decoder | Correlation | Body and generation |
| --- | --- | --- |
| AccessStatus summary | `family=query,query_id` | `kind=access_status,summary`; message generation equals summary generation |
| AccessStatus fixed denial | `family=query,query_id` | `kind=denied,reason=grant_unavailable`; generation is explicitly null; complete original packet<=2048 bytes |
| Bind | `family=bind,binding_request_id` | `kind=bound,binding_request_id,parent,expires_at`; both binding IDs match and message generation equals parent generation |

The summary has exactly `generation`, positive signed-range `expires_at`, `readable_keys`, `writable_keys`, `session_consent` and required nullable `configured_settings_revision`. Readable keys are at most five strictly ordered existing Host keys. Writable keys are at most two ordered embeddings enabled/dimensions keys contained in the readable set. Existing `SettingKey` ordering and `SettingsProviderKind` are reused.

Consent is exactly `none` or `configured_provider{provider,model,recipient}`. None requires an explicit null configured revision; configured consent requires a positive signed-range revision. The model is nonblank, NUL-free and at most256 UTF8 bytes. The recipient is NUL-free and at most4096 UTF8 bytes, and Foundation URL parsing rejects credential userinfo, query or fragment. The exact tuple is retained without normalization, endpoint lookup, network contact or provider fallback. The actual Host grant validator owns canonical official/loopback recipient and model policy; this data decoder does not create consent or implement another endpoint authority.

Bound contains only a nonzero parent ID, the exact matching public generation and positive signed-range original parent expiry. It contains no Original/service outcome, receipt, grant summary or permission flag. Unsupported responses and invalid data throw; they never become Bound, a fixed denial or Saved. The decoders return typed immutable observations with no stored raw plaintext, parsed tree, diagnostic or additional field. Response records have file-private construction; all new aggregate descriptions and debug descriptions are fixed redacted strings. Existing finite `WireError.invalid` and `size` are the only outward errors; Foundation exception text is not retained or forwarded.

## Required authenticated consumers

A real future owner must register the bounded private adoption pipeline before decoding, authenticate the complete current pinned Host/local device and signature/AEAD, and match the current connection, actual retained query and encrypted request ID before adopting an AccessStatus generation. A decoded generation, correlation UUID or matching body is not authority. Revocation, reconnect and Host/connection epoch changes invalidate the corresponding observation. An authenticated denial disables the relevant current scope; it creates no parent.

Only that actual authenticated observation can supply the genuine Bind producer. Bound adoption must match its original Host, encrypted request ID, binding ID, complete generation and retained requested/grant deadlines. It must preserve the actual Host-issued parent/expiry rather than fabricate a parent or renew an original. This leaf implements neither producer nor adoption and names no type Authenticated or Granted.

`DeviceIdentity.seal` currently fixes ordinary envelope lifetime to seven days. It cannot be used for Settings fresh requests without a later scoped existing-identity API enforcing duration<=30000 milliseconds and outer expiry<=the original inner expiry. Accepted private receive still needs the genuine post-AEAD owner, current full-pin/local-ID/disabled rechecks, exact replay transaction and metadata-before-ACK ordering. Ordinary inbox caching is not a private Settings consumer, and a transport ACK is not Saved.

The actual later consumer must enforce full encrypted envelope bounds49152 for summary/Bound and4096 for denial, complete Settings relay frames<=364544, response duration<=60000 and the actual current grant/parent/fixed clocks. It must retain private owners and buffers through actual work joins. No queue, Task, storage, cryptography, UI or lifetime owner is added here. Root owns both Apple project registrations and shared integration. The [AccessStatus source](settings-access-status.md), [summary data](settings-access-replies.md), [Bind data](settings-bind-replies.md) and [Apple refusal boundary](apple-private-settings-ingress.md) retain their separate authority and activation requirements.

## Validation boundary

Standalone Swift grammar and source/tree/diff/link checks are syntax/static observations only. Actual supported macOS/iOS SDK build/analyze follows Root project registration. Product encoding/decoding, Rust/Swift encrypted interoperability, malformed/replay/expiry/correlation behavior, real device/relay/UI acceptance, private buffer lifetime and security review remain unverified. No new tests, fixtures, dependencies or source comments are introduced. Issues #116/#117 remain open; [#32](https://github.com/izyuumi/everything/issues/32) retains deferred automated authorization, interoperability, replay/expiry, revocation, cancellation and reconnect assertions.

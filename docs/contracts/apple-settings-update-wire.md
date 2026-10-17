# Apple scoped Update wire data

`RemoteSettingsUpdateWire.swift` defines the existing Host protocol's narrow Update command and matching Original response. Both Apple targets register this shared source. It is a data codec; no live Update requester, authenticated adopter or writable control is installed. The [Original inspection client](apple-settings-original-inspection-client.md) remains a separate read path.

## Immutable request

`RemoteSettingsUpdateRequest` retains Host ID, the supplied data parent/generation, Original request ID, fixed request time, expected Settings revision and optional embedding enabled/dimensions values. Initialization, encoding and matched decoding validate nonnil identifiers, positive signed-range revisions, an issued-before-expiry interval of at most30000 milliseconds and at least one supplied preference. Typed UUIDs encode canonically.

Both option fields are emitted. Null means no change; an explicit `backendDefault` dimension selection requests a change to the default. A selected dimension is validated in1...65536 to match the existing Rust command grammar. This grants no model/backend support; the Host still validates its supported configuration and current scope. No additional embedding model is introduced.

`encoded()` builds the exact schema1/settings1/command/update wrapper with the existing bounded `WireValue` encoder. The parent constructor remains private to its original file. Supplying data that resembles a parent never creates a genuine bound association or permission.

## Matched Original response

`decodeReply(_:matching:encryptedRequestID:)` uses the existing full-input, duplicate-rejecting `WireValue` parser and exact closed field sets. The response must have command correlation, an Original body, nonnull generation equal to the request parent, exact Host and both Original request IDs, and `in_reply_to` equal to the separately supplied encrypted message ID. It accepts no denial, null generation or other-family fallback. The protocol imposes no extra inequality between Original and encrypted message IDs.

The genuine new decoder calls the existing checked `RemoteSettingsReadWire.decodeOriginalView`. Only that method's visibility changes; its body and subordinate validators remain unchanged. Matched Update replies require kind Update, an empty mutation list and either unlinked/not-requested or the same Original operation with a recorded receipt. Receipt operation ID and expected revision must match the request. Write changed keys may be a subset of supplied preferences because a requested value can already match. Linked absence is rejected by this direct Update path, matching the actual Host projector; the broader inspection grammar stays unchanged.

Existing receipt, phase, write-resolution, revision and application relationships remain enforced. Original Completed, transport ACK, applied state, file resolution and application are distinct facts. Receipt update time is display data, not freshness or permission.

## Bounds and integration

| Representation | Maximum bytes |
| --- | ---: |
| Complete request | 32768 |
| Complete reply | 65536 |
| Canonical Original view | 4096 |
| Canonical service receipt | 2048 |

Encoding and decoding preserve fixed size errors and map other failures to fixed invalid errors. No private parser diagnostics, inferred denial, retry permission or absence result is produced. Request ciphertext49152, reply ciphertext90112 and received frame364544 remain obligations of later authenticated consumers.

The codec retains immutable Foundation data without a Task, socket, Store, queue, cache or runtime. Actual typed sealing/classification, one-use foreground capture, the same private Bound parent, fixed clocks, current full pins/Store checks, Fresh metadata, send/helper/ACK joins and uncertainty retention require a separate client increment. A fresh inspection of an old Original is not permission to resubmit it. Host private dispatch, disposition, ACK and retirement remain separate dependencies.

Full Root/independent source review, preservation and all62 official Swift grammar parses cover this data source. Current integrated SDK/semantic checks remain pending; earlier SDK results exclude this leaf. No codec/crypto/SQLite/interoperability, UI, transport or security acceptance follows from source review. #116/#117 remain open; automated verification remains deferred to [#32](https://github.com/izyuumi/everything/issues/32).

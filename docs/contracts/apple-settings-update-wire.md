# Apple Update wire data

`Shared/Settings/RemoteSettingsUpdateWire.swift` is a data codec for a narrow embedding-preference Update command and its matching Original response, registered in both Apple targets. Its parent/Original shape predates the Host's implemented generation-scoped `update` [request](settings-access-status.md) and does not interoperate with it; alignment is #117 work. It is used only by the [Update client](apple-settings-update-client.md), which has no UI caller. The iOS Settings screen exposes only the [Access check](apple-settings-access-client.md), and the Host in this tree answers only `AccessStatus`; Update commands are recorded as rejected ([Host Access transport](settings-access-transport.md)).

## Request

`RemoteSettingsUpdateRequest` holds Host ID, parent/generation, Original request ID, request time, expected Settings revision and optional embeddings enabled/dimensions values. Validation: nonnil identifiers, positive signed-range revision, issue-to-expiry at most 30,000 ms, at least one supplied preference, dimensions in 1...65536. Both option fields are always emitted; null means no change. `encoded()` builds the schema 1 / settings 1 / command / update wrapper with the bounded `WireValue` encoder. Data that resembles a parent creates no association or permission.

## Response

`decodeReply(_:matching:encryptedRequestID:)` uses the duplicate-rejecting `WireValue` parser and exact field sets. It requires command correlation, an Original body, generation equal to the request parent, matching Host and Original request IDs, and `in_reply_to` equal to the encrypted request ID. No denial, null generation or other-family fallback. It reuses `RemoteSettingsReadWire.decodeOriginalView`; Update replies require kind Update, an empty mutation list and matching receipt operation and expected revision.

## Bounds

| Representation | Maximum bytes |
| --- | ---: |
| Complete request | 32768 |
| Complete reply | 65536 |
| Canonical Original view | 4096 |
| Canonical service receipt | 2048 |

Size failures keep fixed size errors; others map to fixed invalid errors. No diagnostics, inferred denial or retry permission are produced.

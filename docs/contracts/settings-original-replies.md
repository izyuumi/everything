# Closed Original reply data

`devices::settings_reply` defines Unix-only, crate-private, Serialize-only data for the Original response lane. It has no producer, projection, incoming decoder, authority gate or client handler. This source foundation remains uncompiled and unactivated pending genuine consumers and integration validation.

## Complete response packet

`SettingsOriginalReplyPacket::new(host_id, in_reply_to, request_id, generation, view)` constructs these fixed wrappers. Identifiers use the existing typed UUID serializers; `in_reply_to` identifies the encrypted transport message, while both inner request fields identify the same Original.

| Object | Exact fields |
| --- | --- |
| Packet | `schema_version`, `payload` |
| Payload | `kind=settings`, `version=1`, `host_id`, `message` |
| Message | `kind=response`, `in_reply_to`, `correlation`, `generation`, `body` |
| Correlation | `family=command`, `request_id` |
| Generation | `grant_id`, `grant_revision`, `host_epoch`, `connection_epoch` |
| Body | `kind=original`, `request_id`, `view` |

The schema is the existing `SchemaVersion::CURRENT`. Generation reuses the existing `BindingGeneration` with Serialize added; its incoming Bind decoder is unchanged. The data constructor rejects nil identifiers and zero or out-of-range generation revisions. It fixes correlation and body request IDs together. It supplies no proof of currentness or permission.

## Original and service facts

`OriginalView` contains exactly `original_kind`, `terminal`, `link`, `mutation_operation_ids` and `service`. Its terminal is the existing `RemoteOriginalTerminal`, independent of service/write/application facts. Optional fields serialize explicit null. No original input, text, values, timestamps, close metadata, principal, full pin, hash, fingerprint, native action reference or diagnostic is included.

| Data | Closed shapes |
| --- | --- |
| Original kind | `chat_open`, `send`, `update` |
| Link, tagged `state` | `unlinked`; `chat{chat_id}`; `turn{chat_id,turn_id}`; `operation{operation_id}` |
| Service, tagged `state` | `not_requested`; `not_recorded`; `recorded{receipt}` |
| Receipt | `operation_id`, `expected_revision`, `updated_at`, `phase`, `write`, `application` |
| Phase, tagged `phase` | `applying{expected_revision,revision}`; `finished{state}` |
| Finished state, tagged `kind` | `applied{revision}`; `pending_application{revision}`; `restart_required{revision}`; `conflict{observed_revision}`; `host_action_required`; `unavailable{reason}`; `failed{reason}`; `unknown`; `cancelled` |

Reasons, write inspections, resolutions and application states reuse the existing finite Settings types. This module does not forward `SettingsOperationInspection`, a storage Original record or a native/Owner response. Native Awaiting, InFlight, Completed and Requires states cannot be represented. Defining ChatOpen/Send data tags does not implement those routes.

The packet constructor and checked encoder enforce these shape constraints:

- ChatOpen permits only Unlinked/Chat and no mutation IDs or service lookup. Send permits only Unlinked/Turn. Update permits only Unlinked/Operation and no mutation IDs.
- Unlinked means NotRequested and no mutation IDs. A linked Update requires NotRecorded or Recorded for its exact operation ID. A Send receipt must belong to its sorted unique mutation IDs, bounded to eight; NotRecorded requires an actual candidate member.
- Receipt revisions are positive and within the signed SQLite range when present. Applying requires its matching unresolved write. Write revisions are exactly the successor of the matching expected revision. Changed keys are nonempty, strictly sorted, bounded to two and limited to embeddings enabled/dimensions.
- Applied, PendingApplication and RestartRequired require the matching Applied write. Unknown may retain an unresolved write. A NotApplied write requires Failed, Unavailable, Cancelled or Conflict. Application facts require PendingApplication with an Applied write. No restart flag or reason is inferred.

A later trusted projection must prove the real installed link, owning Original, current disclosure scope and actual service lookup. A NotRecorded shape alone cannot prove absence, NoEffects or retry permission. Expected revision retains its canonical Option shape; genuine paired origin/binding validation remains outside this data module. Original Unknown remains independent when a later service fact reports Applied; Original Completed does not mean Applied.

## Concrete encoding and ownership

`encode_checked` revalidates the actual view, serializes a complete recorded receipt with a 2048-byte ceiling, then the full view with a 4096-byte ceiling, then the full wrapped packet with a 65536-byte ceiling. Temporary buffers are Zeroizing and drop before the next serialization. Partial output also remains in a Zeroizing buffer when serialization fails. The returned complete packet is `Zeroizing<Vec<u8>>`; no field is truncated and no example size is treated as a runtime observation.

`SettingsReplyContractError` distinguishes Invalid, TooLarge and Encoding. Encoding retains its real structured `serde_json::Error`; Display and Debug use fixed redacted messages. Callers must not expose the underlying encoding cause as remote text. Aggregate Debug implementations omit their data, and aggregates have neither Clone nor Deserialize.

A genuine reply-build permit and current same-Core publication qualification are prerequisites **before private projection or encoding**. This module implements neither. A SQL reservation, copied generation or this constructor cannot substitute for them. The future build owner must retain its permit through projection, encoding, sealing, the actual store worker join and destruction or storage consumption of all build buffers, including error paths. Later durable ciphertext send admission is a separate counted lifetime within the existing outbound window.

Actual Inspection-family ciphertext/envelope capacity, atomic reservation transfer, current grant/pin/parent/expiry gates, socket ownership, full Host drain, decoder/adoption and manual runtime validation remain separate integration work. The [command receive contract](settings-command-receive.md), [Bind receive contract](settings-bind-receive.md) and [private ingress boundary](private-settings-ingress.md) retain their current activation limits. This foundation performs no sealing, storage, queue admission, send or execution.

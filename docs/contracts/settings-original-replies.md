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

## Shared current persisted pins

The actual Original current reader now invokes the single Core-private `paired/pins.rs` validator on its already supplied Main connection, after actual Source/context/Host authority checks. It requires bounded strict current Host configuration with the same Host ID, version1, Host role, exact actual endpoint public keys and no revocation. It validates the current peer's complete keys and DeviceAccess, requires version1 and exact keys plus complete registration equality with the genuine retained Source pin, and rejects peer revocation. The expected keys and registration come directly from that actual Source; this private facts checker constructs no authority.

Initial qualification, counted preprojection recheck, pretransfer checks, consumed-current checks and comparison capture all inherit the same reader. Their final actual Authority observation remains after all pin/grant/Original/operation SQL. Fixed source/receipt clocks, original eight-field slot checks, sole transfer, one commit and exact point-in-time facts remain unchanged. No inactive receive precommit guard or nested Store acquisition is used.

SQL and validation failures retain their actual structured service causes. Strict pin JSON decoding retains its actual serde_json cause through the existing StorageError::Json and projection authority chain; no malformed or changed pin becomes a denial. The ordinary grant/pairing/transport readers and native pin policies are unchanged. Accepted private activation, later queued/send ownership and disposable runtime/security verification remain separate; direct formatting/source preservation alone does not verify them.

## Genuine queued-current Original check

The new internal queued-current gate accepts the actual settled `OwnedQueuedSettingsReply` and borrows its real Fresh/Historical source and immutable committed comparison. Its trusted caller must hold the actual checked Store Main transaction and a newly held same Host mutation guard. The gate matches original message ID/encrypted digest and header/expiry to that retained owner, then reuses the row-free current Original read with full pins, exact embedded receipt capability, parent, grant and original fixed clocks. It never reconstructs a consumed proof or skips a reservation check through a flag.

Two temporary current-readable booleans come from that same real Active grant read. Current scope must cover every sealed disclosed key as well as the reader's existing current target/changed-key checks. Captured invocation, link/close/terminal floors remain associated and cannot regress or be replaced. A new matching link must belong to this actual Original; the real opaque history anchor checks the complete existing operation chain, or the genuine formerly unlinked case checks its new root from initial Applying. Both use the bounded protected writer-chain reader, not enum rank, sequence alone or event-time ordering. The ciphertext remains its original point-in-time receipt.

After all SQL/history, actual queued owner/handoff health is rechecked immediately before the final actual Authority observation. Captured and sealed wall floors and original monotonic deadlines remain mandatory. No SQL follows that final observation. The existing projection, sealer, consumed/current checks and native writers remain unchanged. The new Result-only gate is not yet called by a sender and grants no send, cleanup, retry, release or drain permission. Compiler, SQL/runtime and transported/security acceptance remain separate; automation stays [#32](https://github.com/izyuumi/everything/issues/32).

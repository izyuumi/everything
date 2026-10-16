# Closed Bind response data

The `settings_bind_reply.rs` leaf declares crate-private, Serialize-only data for the successful Bind response. The leaf remains uncompiled and unactivated. The module and crate-private exports are registered. Current Bind qualification and projection are implemented in the same Core source. The genuine registered build/store producer, publication and client adoption remain integration work. Defining this data does not accept a Bind or create a parent.

## Complete packet

`SettingsBindReplyPacket::new(host_id, in_reply_to, binding_request_id, parent, expires_at)` fixes these wrappers:

| Object | Exact fields |
| --- | --- |
| Packet | `schema_version`, `payload` |
| Payload | `kind=settings`, `version=1`, `host_id`, `message` |
| Message | `kind=response`, `in_reply_to`, `correlation`, `generation`, `body` |
| Correlation | `family=bind`, `binding_request_id` |
| Generation | `grant_id`, `grant_revision`, `host_epoch`, `connection_epoch` |
| Body | `kind=bound`, `binding_request_id`, `parent`, `expires_at` |
| Parent | `parent_id`, `generation` |

The schema is the existing `SchemaVersion::CURRENT`. Parent reuses the existing `SettingsParentReference`, with the existing `BindingGeneration` labels. The constructor fixes both binding IDs together and copies the message generation from that same parent. All wrapper fields are private. Constructor and checked encoder validate nonnil identifiers, matching correlation/body IDs and generations, positive signed-range grant revision and positive typed expiry.

`in_reply_to` is the actual received encrypted Bind message ID. It is distinct from the original `binding_request_id` and the new response envelope message ID. A trusted producer must derive the parent and body expiry from the actual successfully installed parent matched to its genuine committed Bind owner. The body expiry reports that original parent lifetime; it does not extend the shorter reply reservation or encrypted response expiry.

There is no Granted flag, Original/service result, grant summary, principal, full peer pin, secret, native proof/reference, diagnostic, arbitrary string or JSON field. Aggregates have neither Clone nor Deserialize; packet Debug is fixed redacted. Copying finite public generation labels does not copy authority. There is no mapping from a history snapshot, stored row or failed parent result into a successful Bound.

## Checked encoding and errors

`encode_checked` revalidates the complete packet and uses the existing serde JSON serializer with an owned `Zeroizing<Vec<u8>>`. It checks the actual complete encoded length against 32768 bytes and returns only that complete buffer. Partial serialization output and oversized output remain under Zeroizing through error return and destruction. No field is truncated, and no example encoding is treated as a measured runtime result. This ceiling is an acceptance check after complete serialization, not an allocator/RSS or universal erasure guarantee.

`SettingsBindReplyContractError` distinguishes Invalid, TooLarge and Encoding. Encoding retains the actual structured `serde_json::Error` as its source. Display and Debug remain fixed redacted messages; a future caller must not expose the underlying cause as remote text. These are data/encoding errors, not grant, parent, effect or delivery verdicts.

## Required producer ownership

The real producer must already own the single shared reply-build slot and pass the current same-Core Bind qualification before constructing this private response. That qualification must check the exact original owner, installed parent record/creation association, complete current pin/grant/epochs, actual Host mutation guard/Main transaction, fixed clocks and the full original Default reservation. It must not reuse the inactive receive precommit guard or create a parent as a lookup shortcut. The [current Core qualification and projection](settings-reply-projection.md) now implement those source checks; this DATA leaf itself supplies no authority, registered build job or publication.

The actual Default reservation retains its original immutable provenance with `parent_id=null` and its 49152-byte ciphertext allowance. A later parent cannot rewrite that reservation or authorize another slot. Sealing and transfer must check the actual complete encrypted envelope against that allowance and the complete Settings relay frame against 364544 bytes, while preserving the original deadline and exact reservation. This module performs no encryption, SQL, transfer, queue admission or send.

The future registered build owner must retain the genuine permit, Bind owner, actual parent association and leased state through projection, encoding, sealing, actual store-worker join and destruction or consumption of every private build buffer, including failures. The later durable ciphertext send uses its own counted admission inside the existing total outbound window. Releasing the build slot is separate from retiring the Bind owner: its history creator remains owned with the actual parent until genuine terminal disposition, because dropping it changes committed history to Unknown.

No success/error packet producer, optional denial, known-Bind receiver exception or incoming client handler is added. A failed or uncertain install stays a real owned error and cannot become Bound, NoEffects, a refund or retry permission. The [Bind receive](settings-bind-receive.md), [paired Authority](paired-settings-authority.md), [supervisor](settings-paired-supervision.md) and [transport storage](settings-transport-storage.md) contracts retain those separate boundaries. Current strict compiler/Clippy, actual serialization and disposable transported/manual/security verification remain pending; #116 and deferred #32 remain open.

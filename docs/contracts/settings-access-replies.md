# Closed AccessStatus response data

The `settings_access_reply.rs` leaf declares crate-private, Serialize-only responses for the pre-parent AccessStatus query. It is registered in the Rust module graph and remains uncompiled and unactivated. The strict request codec and genuine owned verification source are separate prerequisites; current read permission, authenticated receipt/build/transfer/send and client adoption remain integration work. A valid data constructor is not current authorization.

## Complete packet

Both constructors fix these wrappers:

| Object | Exact fields |
| --- | --- |
| Packet | `schema_version`, `payload` |
| Payload | `kind=settings`, `version=1`, `host_id`, `message` |
| Message | `kind=response`, `in_reply_to`, `correlation`, `generation`, `body` |
| Correlation | `family=query`, `query_id` |
| Summary body | `kind=access_status`, `summary` |
| Denial body | `kind=denied`, `reason=grant_unavailable` |

The schema is the existing `SchemaVersion::CURRENT`. Host, actual encrypted `in_reply_to` and the one original query ID must be nonnil. The query correlation is present for both legitimate responses; the denial does not drop it or invent another query. `in_reply_to` is distinct from the query ID and the new response envelope ID.

## Actual grant summary

`SettingsAccessReplyPacket::from_grant_head(host_id, in_reply_to, query_id, generation, head)` borrows the actual `SettingsGrantHead` and calls its existing `validate()`. It rejects revoked Host/peer flags, non-Active state and a Host/grant-ID/revision mismatch before copying approved fields. It does not construct a dummy head, key or grant to access validation, replace the existing validator or read configuration/provider state.

The summary contains exactly `generation`, `expires_at`, `readable_keys`, `writable_keys`, `session_consent` and `configured_settings_revision`. Expiry and configured revision are the actual grant values. Key vectors and `SettingsGrantConsent` are copied only from that validated Active head. Existing grant validation owns key ordering/allowlists, readable-and-writable relationships, lifetime/revision bounds, consent endpoint/model validation and the configured-revision/consent relationship; this leaf does not duplicate those rules. The existing validator may internally serialize grant data while checking its bounds; that is validation, not a remote projection of the head.

Message generation is present and equals the summary generation. Grant ID/revision must match the actual borrowed head; epochs/grant IDs are nonnil and revision is within the positive signed range. The generation labels do not prove a current Host/connection incarnation. Optional configured revision serializes explicit null. `session_consent` reuses the existing `none` or `configured_provider{provider,model,recipient}` form, preserving only the exact explicitly consented tuple. No newly configured recipient/model or unapproved provider fallback is substituted.

The summary exposes no grant owner request, principal, complete peer keys, change/rotation history, receipt hash, credentials or native proof. Its fields and wrappers are private; aggregates have no Clone or Deserialize, and packet Debug is fixed redacted. Existing validated consent strings are the only model/recipient values copied; there is no arbitrary error or additional JSON field. The input head itself is never retained or serialized as the response.

## Fixed pre-parent denial

`SettingsAccessReplyPacket::grant_unavailable(host_id, in_reply_to, query_id)` constructs only the fixed denial with `generation=null`. It contains no grant summary, private value, peer key, parent, grant ID or consent tuple. Selecting this constructor requires a real Core policy decision; an I/O, authentication, validation, bookkeeping or worker failure must not be silently converted to grant_unavailable. The leaf does not inspect policy or classify those failures.

## Encoding and errors

`encode_checked` revalidates the private wrapper/generation shape, then serializes the actual complete packet into `Zeroizing<Vec<u8>>`. Summary packets must fit 32768 bytes; the fixed denial must fit 2048 bytes. The cap comes from the actual closed body variant. Partial error output and oversized output remain owned by Zeroizing through destruction; success returns the entire owned buffer without truncation or fallback. No product serialization or actual size measurement was executed for this source slice. These post-serialization ceilings do not establish allocator/RSS or universal erasure bounds.

`SettingsAccessReplyContractError` preserves Invalid, GrantValidation with the actual `SettingsContractError`, TooLarge and Encoding with the actual `serde_json::Error`. Display and Debug remain fixed redacted messages. A future caller must retain the real structured cause and must not forward its source as remote text. Data/encoding errors are not grant, receipt, effect or delivery outcomes.

## Required consumers and ownership

Before reading a private summary or constructing/encoding this packet, the actual producer must hold genuine counted query/build ownership and the current same-Core pre-parent qualification. It must authenticate the actual encrypted sender against the complete current own pin, Host role/connection/epochs, actual grant/head, original query/correlation and fixed request/receipt clocks. A copied generation or borrowed head is not that authority. No parent is created or required for AccessStatus itself.

The genuine receipt and reply reservation, common single build slot, registered projection/encoding/seal/store work, final Main transfer and separate counted sender must preserve actual owners and joins through every private buffer lifetime. Summary encrypted envelopes must fit the actual Default 49152-byte allowance; denial envelopes must fit Denial 4096. The actual producer also checks the complete Settings relay frame ceiling and current publication/send gates. There is no encryption, SQL, queue, socket, second build slot or fake permit here.

The strict [AccessStatus request codec and owned source](settings-access-status.md) are registered separately. An actual authenticated receipt, summary/denial policy producer and client adoption remain absent. Client adoption must match its real pending query and verified current Host before revealing values. The [grant](settings-grants.md), [Bind response](settings-bind-replies.md), [Original response](settings-original-replies.md) and [transport storage](settings-transport-storage.md) contracts retain their separate boundaries. Current strict compiler/Clippy, actual serialization and disposable transported/manual/security observations remain pending; #116/#117 and deferred #32 stay open.

## Held actual counted Core consumer

The same-Core `project_access_reply_on` now consumes the genuine joined Access qualification and actual associated BUILD permit before calling these unchanged packet constructors. It repeats full current query/pin/grant/original clock and exact reservation checks, then selects summary only from the actual Active head or denial only from genuine GrantUnavailable. Errors do not become denials. The complete existing checked encoder returns the actual Zeroizing32768/2048 packet to a private projection containing only successful observation floors and the unchanged original reply deadline.

`in_reply_to` comes from the genuine encrypted request MessageId in Source receipt metadata, while correlation uses the separate original query UUID. The real held Access sealer uses its own new reply-envelope ID and the unchanged Default49152/Denial4096 slot. No constructor, field, validation rule, encoding behavior or error in this DATA module changed. The new consumer is uninvoked until real Main/Work Access registration is integrated; current formatting/static checks are no serialization, crypto/SQL, transported-client or security observation. Queued/send/cleanup/client gates and #116/#117/#32 remain open.

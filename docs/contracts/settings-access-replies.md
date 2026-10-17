# AccessStatus response data

`src/devices/settings_access_reply.rs` defines the closed, Serialize-only AccessStatus response. Selecting which constructor to use is [Access qualification](settings-access-qualification.md); sealing and sending are in [the Access transport](settings-access-transport.md).

## Packet

| Object | Fields |
| --- | --- |
| Packet | `schema_version`, `payload` |
| Payload | `kind=settings`, `version=1`, `host_id`, `message` |
| Message | `kind=response`, `in_reply_to`, `correlation`, `generation`, `body` |
| Correlation | `family=query`, `query_id` |
| Summary body | `kind=access_status`, `summary` |
| Denial body | `kind=denied`, `reason=grant_unavailable` |

`in_reply_to` is the encrypted request MessageId; `query_id` is the request's query UUID. Both are nonnil and distinct from the reply envelope id.

## Summary

`SettingsAccessReplyPacket::from_grant_head` validates the borrowed `SettingsGrantHead` with its existing `validate()` and rejects revoked or non-Active heads and Host/grant/revision mismatches. The summary carries exactly `generation`, `expires_at`, `readable_keys`, `writable_keys`, `session_consent` and `configured_settings_revision`. Message `generation` equals the summary generation. Nothing else from the head (owner, principal, peer keys, history, credentials) is serialized.

## Denial

`SettingsAccessReplyPacket::grant_unavailable` produces the fixed denial with `generation=null`. It is used only for a genuine `GrantUnavailable` decision; I/O, validation or worker failures must not become a denial.

## Encoding and errors

`encode_checked` returns `Zeroizing<Vec<u8>>`. Limits: summary 32,768 bytes, denial 2,048 bytes; oversize is an error, never truncated. Sealed envelopes must fit the `default` 49,152 or `denial` 4,096 reservation. `SettingsAccessReplyContractError` keeps the structured cause behind fixed redacted Display/Debug.

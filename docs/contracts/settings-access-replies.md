# Remote Settings reply DATA

`src/devices/settings_access_reply.rs` defines the closed, Serialize-only reply packet. Selecting the body is [qualification](settings-access-qualification.md) and [payload preparation](settings-access-transport.md#reply-build); sealing and sending are in [the transport](settings-access-transport.md).

## Packet

| Object | Fields |
| --- | --- |
| Packet | `schema_version`, `payload` |
| Payload | `kind=settings`, `version=1`, `host_id`, `message` |
| Message | `kind=response`, `in_reply_to`, `correlation`, `generation`, `body` |
| Correlation | `family=query`, `query_id` |

`in_reply_to` is the encrypted request MessageId; `query_id` is the request's query UUID. Both are nonnil and distinct from the reply envelope id. `generation` is the current grant id/revision and Host/connection epochs for every non-denial body, and `null` for a denial.

## Bodies

| `kind` | Fields | Used for |
| --- | --- | --- |
| `access_status` | `summary` | AccessStatus with an Active grant |
| `snapshot` | `snapshot` (existing `SettingsSnapshot`, entries limited to the requested readable keys) | Snapshot |
| `changes` | `page` (existing `SettingsChangePage`, hint keys intersected with readable keys) | Changes |
| `update` | `request_id`, `operation` (`phase`, `updated_at`, `write`, `application` of the original operation) | Update, including exact replay |
| `host_action_required` | `action` | HostAction |
| `session_opened` | `chat` (existing `SettingsChatOpened`: `chat_id`, `provider`, `model`, `recipient`, `provider_binding_sha256`, the seven tool names, `expires_at`) | SessionOpen, including replay of the same `open_request_id` |
| `session_accepted` | `chat_id`, `turn_id` | SessionSend, including exact replay |
| `session_status` | `status` (see below) | SessionPoll, SessionClose |
| `denied` | `reason` | See below |

`session_status.status` projects the existing `SettingsChatStatus`: `chat_id`, `phase`, `turn_id`, `text_revision`, `text_offset`, `text_bytes`, `text`, `provisional`, `context`, `operation_ids`, `model_attempts`, `charged_tokens`, `reserved_output_tokens`, `provider_complete`, `usage_complete`. `text` is the page of committed assistant text starting at the requested `text_offset`, cut at a character boundary so that its JSON-escaped form is at most 16 KiB; `text_bytes` is the full committed length, so a client reads the rest with larger offsets at the same `text_revision`. Streaming provisional text is not sent; `provisional` carries only its `streaming`/`interrupted` state.

The summary carries exactly `generation`, `expires_at`, `readable_keys`, `writable_keys`, `session_consent` and `configured_settings_revision`. Nothing else from the grant head (owner, principal, peer keys, history, credentials) is serialized. The update `operation` omits the binding, origin and digests of the journal record.

Denial reasons: `grant_unavailable` (no, expired or revoked grant), `grant_changed` (stale generation), `outside_scope`, `conflict` (stale revision or changed reuse of `request_id`), `write_blocked`, `capacity`, `invalid`, `outcome_unknown`, `unavailable`. For session requests: `outside_scope` also covers a grant without session consent, a consent tuple that does not match the captured provider, an unknown, expired or retired chat, and a provider digest mismatch; `conflict` is a changed reuse of `turn_id`; `outcome_unknown` is an Open still preparing or uncertain; `invalid` is a `text_offset` that is not a character boundary within the text; `capacity` is a second live chat for the same device or no free paired parent; `unavailable` covers no configured model or credential, a busy or draining runtime and a request that did not finish before its deadline. I/O, validation or worker failures in the transport itself never become a denial; they drop or quarantine as described in the transport.

## Encoding and errors

`encode_checked` returns `Zeroizing<Vec<u8>>`. Limits: non-denial 32,768 bytes, denial 2,048 bytes; oversize is an error, never truncated. Sealed envelopes must fit the `default` 49,152 or `denial` 4,096 reservation. `SettingsAccessReplyContractError` keeps the structured cause behind fixed redacted Display/Debug.

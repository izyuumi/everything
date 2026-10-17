# Remote Settings request DATA

The Host decoder is `src/devices/settings_ingress/access.rs`. Reply DATA is in [replies](settings-access-replies.md); the transport path is in [the transport](settings-access-transport.md). The Apple encoder `apps/Apple/Shared/Settings/RemoteSettingsWire.swift` produces only `access_status` in this shape ([Apple wire](apple-remote-settings-wire.md)).

## Shape

| Object | Exact fields |
| --- | --- |
| Packet | `schema_version=1`, `payload` |
| Payload | `kind=settings`, `version=1`, `host_id`, `message` |
| Message | `kind=query`, `query_id`, `body` |
| Time | `issued_at`, `expires_at` (Unix ms) |
| Generation | `grant_id`, `grant_revision`, `host_epoch`, `connection_epoch` (copied from the latest AccessStatus summary) |

| Body `kind` | Other exact fields |
| --- | --- |
| `access_status` | `time` |
| `snapshot` | `generation`, `keys`, `time` |
| `changes` | `generation`, `after_sequence`, `limit`, `time` |
| `update` | `generation`, `request_id`, `expected_revision`, `changes`, `time` |
| `host_action` | `action`, `time` |

## Rules

- Every level must be a JSON object. Unknown, missing or duplicate fields, wrong `kind` strings, other versions and trailing JSON are invalid.
- Complete input is at most 32,768 bytes.
- `host_id`, `query_id`, `request_id` and generation UUIDs are canonical lower-case non-nil UUIDs; `grant_revision` is positive and within the signed range.
- `issued_at > 0`, `expires_at > issued_at`, and the lifetime is at most 30,000 ms.
- `snapshot.keys`: non-empty, unique, Host-scope setting keys.
- `changes`: `limit` 1–64, `after_sequence` 0 or a durable sequence cursor.
- `update.changes`: the existing typed `SettingChange` list; together with `request_id`, `expected_revision` and this Host it must form a valid `SettingsPatch` without confirmation. `request_id` is the immutable mutation identity; `query_id` only correlates one exchange.
- `host_action.action`: one of `credential_entry`, `pairing_invitation`, `pairing_approval`, `device_revocation`, `grant_change`, `execution_policy`, `recipient_consent`, `lifecycle`.
- A request carries no grant authority, parent, caller, credential, invitation or free text. A generation is a freshness check, not authority.

Decoding returns only the typed intent or Invalid, with no diagnostics. A decoded intent is neither authenticated nor authorized.

## Verification at receipt

The verified source binds the intent to the authenticated envelope, its connection and the receive clock floor. At admission it requires a Host endpoint, a live connection, `host_id` equal to this Host, an outer envelope lifetime of at most 30 s ending no later than `expires_at`, and the active receive guard. The Host wall clock plus 5000 ms must not be before `issued_at`; the request must not have expired on either the wall or monotonic clock. Failures are OutsideScope, Expired or Unavailable; the transport classifies them as described in [the transport](settings-access-transport.md#failure-classification). Reusing a `query_id` with a different body is OutsideScope.

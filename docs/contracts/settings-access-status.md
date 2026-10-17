# AccessStatus request DATA

`AccessStatus` is the one remote Settings request the Host answers. The Host decoder is `src/devices/settings_ingress/access.rs`; the Apple encoder is `apps/Apple/Shared/Settings/RemoteSettingsWire.swift` ([Apple wire](apple-remote-settings-wire.md)). Reply DATA is in [Access replies](settings-access-replies.md); the transport path is in [Access transport](settings-access-transport.md).

## Shape

| Object | Exact fields |
| --- | --- |
| Packet | `schema_version=1`, `payload` |
| Payload | `kind=settings`, `version=1`, `host_id`, `message` |
| Message | `kind=query`, `query_id`, `body` |
| Body | `kind=access_status`, `time` |
| Time | `issued_at`, `expires_at` (Unix ms) |

## Rules

- Every level must be a JSON object. Unknown, missing or duplicate fields, wrong `kind` strings, other versions and trailing JSON are invalid.
- Complete input is at most 32,768 bytes.
- `host_id` and `query_id` are canonical lower-case non-nil UUIDs.
- `issued_at > 0`, `expires_at > issued_at`, and the lifetime is at most 30,000 ms.
- The request carries no grant, parent, caller, credential, target or free text. The query id is correlation only.

Decoding returns only the typed intent or Invalid, with no diagnostics. A decoded intent is neither authenticated nor authorized.

## Verification at receipt

The verified source binds the intent to the authenticated envelope, its connection and the receive clock floor. At admission it requires a Host endpoint, a live connection, `host_id` equal to this Host, an outer envelope lifetime of at most 30 s ending no later than `expires_at`, and the active receive guard. The Host wall clock plus 5000 ms must not be before `issued_at`; the request must not have expired on either the wall or monotonic clock. Failures are OutsideScope, Expired or Unavailable; the transport classifies them as described in [Access transport](settings-access-transport.md#failure-classification).

# Scoped Settings read DATA (Swift only)

`apps/Apple/Shared/Settings/RemoteSettingsReadWire.swift`, registered in both EverythingMac and EverythingIOS, encodes Snapshot and Original-inspection requests and decodes their replies. It predates the Host's implemented [request shape](settings-access-status.md): the Host now answers a parent-free, generation-scoped `snapshot` and has no Original-inspection or Bind parent, so these Swift requests are rejected (`settings_unavailable`) and ACKed. Aligning the Swift codec is #117 work. The iOS Settings screen does not expose these controls.

## Requests

Both use the shared outer packet `schema_version:1, payload:{kind:"settings", version:1, host_id, message:{kind:"query", query_id, body}}`.

| Body | Fields |
| --- | --- |
| Snapshot | `kind:"snapshot"`, `parent`, `keys`, `time` |
| Original inspection | `kind:"inspect_original"`, `parent`, `request_id`, `operation_id` (nullable), `time` |

`parent` is `parent_id` plus `generation` (`grant_id`, `grant_revision`, `host_epoch`, `connection_epoch`). `time` is `issued_at`, `expires_at`. The encoded request is at most 32,768 bytes. Snapshot keys are a nonempty, unique, declaration-ordered subset of the five version-one Host keys.

## Replies

The decoder accepts only a reply matching the supplied original request: same Host, query id, parent generation and selected keys or inspection target, with `issued_at <= observed_at < expires_at`.

| Reply | Complete packet limit |
| --- | --- |
| Snapshot | 32,768 bytes |
| Original inspection | 65,536 bytes |
| Denial | 2,048 bytes |

Inspection lookups are `not_recorded` or `recorded`; nested Original view and receipt are bounded at 4,096 and 2,048 bytes. Input is parsed with the strict `WireValue.parse` (duplicate fields, depth and key limits) before typed decoding; UUIDs must be canonical lower-case and non-nil, times and revisions positive.

These are DATA checks only. A decoded reply proves no authenticated ownership, current grant, effect outcome or permission to retry.

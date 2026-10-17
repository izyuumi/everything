# Settings transport storage

Migration `0016_settings_transport.sql` (schema 16) adds private Settings reply capacity and keeps it separate from ordinary sending. The only producer is the Host Access path in [Access transport](settings-access-transport.md): admission inserts a reply reservation, publication consumes it into private outbox rows, and the session deletes those rows after the send attempt. Owners: `src/storage/settings_transport.rs`, `src/storage/settings_publication.rs`, `src/storage/transport.rs`.

## Schema

| Object | Responsibility |
| --- | --- |
| `settings_transport_reply_reservations` | Immutable reserved ciphertext capacity with closed public provenance |
| `settings_transport_private_outbox` | Immutable provenance for one `outbox` / `transport_outbox` ciphertext pair |
| `settings_transport_reply_reservations_immutable` | Reject reservation updates |
| `settings_transport_reply_capacity` | Reject duplicate reservation identities and Settings quota overflow |
| `settings_transport_private_outbox_capacity` | Reject a private row whose reservation still exists, and Settings quota overflow |
| `settings_transport_private_outbox_immutable` | Reject private provenance updates |
| `settings_transport_private_outbox_root` | Keep a tagged outbox root immutable apart from monotonic retry bookkeeping |
| `outbox_capacity_admission` | Check every new outbox row against ordinary quota; private rows must match their provenance |

Both private-outbox foreign keys to the ciphertext roots are `DEFERRABLE INITIALLY DEFERRED` and cascade on delete. Rows use canonical lowercase nonzero UUIDs, positive times with expiry after creation, provenance of at most 2048 bytes and at most 3072 bytes per row. Provenance values (pins, epochs, grant) are correlation data; they do not authenticate a connection or grant authority. No plaintext, credentials or diagnostics are stored.

Reserved bytes by family: denial 4,096, default 49,152, inspection 90,112, status 360,448. The Access path uses only default and denial. A private ciphertext must be positive and at most its family allowance.

## Capacity

| Scope | Ordinary pending outbox | Settings replies (reservations + private rows) |
| --- | --- | --- |
| Global | 1024 messages, 128 MiB | 128 replies, 32 MiB |
| Per peer | 64 messages, 16 MiB | 16 replies, 8 MiB |

Ordinary counts exclude private rows, so Settings never consumes ordinary capacity. Consuming a reservation deletes it before inserting the private row, so bytes are never counted twice. Exhausted headroom returns `StorageError::TransportCapacity`. Existing over-limit ordinary backlogs are grandfathered; only new admissions must fit.

## Ordinary paths exclude private rows

`pending_outbox` and `pending_transport_outbox` skip rows with private provenance. `acknowledge_outbox`, `confirm_transport_outbox` and `accept_transport_outbox` return `StorageError::TransportConflict("private settings outbox requires its owned transport")` for a private row. Ordinary expiry reports and deletion in `maintain_transport` skip private rows.

## Sweep and revocation

`sweep_settings_transport` deletes reservations and private outbox roots (cascading their transport and provenance rows) whose `expires_at` has passed. It runs inside the Store-open migration transaction and in `maintain_transport`. `revoke_device` deletes the peer's pending outbox rows, which cascades its private rows, and its reservations.

## Validation

Local compile and static checks only. No populated-migration, quota, sweep or runtime observation has been made at this schema. Automated coverage is deferred to [#32](https://github.com/izyuumi/everything/issues/32).

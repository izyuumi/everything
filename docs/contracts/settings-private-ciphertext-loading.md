# Private Settings ciphertext loading

`Store::load_private_settings_ciphertext` (`src/storage/settings_publication.rs`) loads one queued AccessStatus reply for sending. The relay session calls it in a blocking task while holding the Host mutation guard, then sends the returned frame on the same socket. See [the Access transport](settings-access-transport.md).

## Checks

One Deferred transaction on the checked Main connection, inside `catch_unwind`:

1. Host endpoint sharing this Store.
2. `recheck_queued_reply_on`: current Access authority for the queued reply.
3. Reservation stamp matches the sealed header (sender, recipient, expiry, creation not earlier than the reservation, lifetime at most 60 s).
4. Read the joined outbox, transport-outbox and private rows; compare provenance, family, ids, byte count and expiry; the ciphertext must fit its family (`default` 49,152, `denial` 4,096), match the RAM SHA-256 and decode to the exact sealed header and signature.
5. Verify the authenticated sender and encode the relay frame.
6. Repeat the current-authority recheck, then COMMIT.

The result is the relay `Send` frame only.

## Failure behaviour

| Result | Session action |
| --- | --- |
| `BeforeTransaction`, `RolledBack` (expired, revoked, swept, mismatch) | Retire the reply immediately; no send |
| `RollbackFailed` | Store quarantined; Settings receive quarantined |
| `CommitUncertain`, `Panicked` | Settings receive quarantined |

Errors keep typed causes behind fixed redacted text; ciphertext and provenance are not logged. After any send attempt the reply is retired per [publication](settings-reply-publication.md#retirement); there is no resend.

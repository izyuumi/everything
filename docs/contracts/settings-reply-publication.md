# Settings reply publication

`Store::publish_settings_reply` (`src/storage/settings_publication.rs`) turns one committed remote Settings receipt into a sealed, queued private ciphertext. It runs in the Work build worker under the Host mutation guard, after `prepare_remote_reply` has computed any Snapshot, Changes or Update payload under that same guard; see [the transport](settings-access-transport.md#reply-build) for the surrounding flow. The payload is passed in `SettingsReplySealContext::payload` and must carry the generation the publication qualification observes.

## Transaction

One IMMEDIATE transaction on the checked Main connection, inside `catch_unwind`:

1. Require the context authority to be the request's embedded receipt authority (pointer equality).
2. `qualify_access_reply_on`: full current [qualification](settings-access-qualification.md) with the reservation still present.
3. `SealedSettingsReply::access` (`src/devices/settings_publication.rs`): project the [response DATA](settings-access-replies.md), seal once with the endpoint identity, check the envelope against its reservation family and the relay frame cap, verify the authenticated sender, and compute a RAM-only SHA-256 of the encrypted bytes.
4. `consume_settings_reply_in` (`src/storage/settings_transport.rs`): delete the exact reservation, insert the outbox and transport-outbox rows and the private provenance row, and read them back against the sealed bytes.
5. Capture the finite comparison, run the consumed-current recheck, then `COMMIT`.

Nothing runs between the final recheck and COMMIT. Only an observed COMMIT returns `CommittedSettingsReply` (header, signature, encrypted SHA-256, length, comparison; no ciphertext buffer).

## Failure behaviour

| Result | Meaning | Work action |
| --- | --- | --- |
| `BeforeTransaction`, `RolledBack` | No effect | Drop the query; its reservation is swept on expiry |
| `RollbackFailed` | Store latched quarantined ([ADR-0013](../decisions/ADR-0013-permanent-store-quarantine.md)) | Quarantine Settings receive |
| `CommitUncertain`, `Panicked` | Outcome unknown | Quarantine Settings receive |

There is no retry, reseal or fallback to denial.

## Retirement

`Store::retire_private_settings_reply` runs after the send attempt, or when loading failed with a rolled-back cause. In one IMMEDIATE transaction it either confirms that the outbox, transport, private and reservation roots are all absent, or verifies the exact stored ciphertext and deletes its outbox root (cascading the rest). Any other root combination is a conflict. A rollback failure quarantines the Store; any retirement failure quarantines Settings receive.

# AccessStatus receipt storage

`Store::admit_settings_access` (`src/storage/transport.rs`) is the only writer of an AccessStatus receipt. It runs inside the Work receive callback described in [the Access transport](settings-access-transport.md) and returns `Duplicate` or `Fresh { reply }` only after an observed COMMIT.

## Transaction

One IMMEDIATE transaction on the Host Store connection:

1. Validate the receiver (complete pinned registration, revocation, local recipient) and the verified source.
2. Compare sender, recipient and encrypted-envelope digest for the message id. An exact match is `Duplicate`: no query registration, reservation or new expiry. A changed envelope under the same id is a transport conflict.
3. Build the receipt plan through [Access qualification](settings-access-qualification.md): current pins, grant head and the Access query cell.
4. Insert one reply reservation (`default` 49,152 bytes with a grant reference, or `denial` 4,096 bytes with `grant:null`; both with `parent_id:null`) under the shared Settings reply quota in [transport storage](settings-transport-storage.md).
5. Insert the processed inbox marker (payload, command id and detail NULL), within the existing replay-marker ceiling.
6. Read back reservation and marker, recheck the plan, then COMMIT.

No plaintext, query body, Original or command row is persisted.

## Failure behaviour

Every error after BEGIN goes through an explicit rollback (`DropBehavior::Ignore`, no hidden retry).

| Outcome | Classification |
| --- | --- |
| Peer-attributable rejection (expired, out of scope, query-id reuse, clock regression, revoked device or endpoint, transport conflict, unavailable clock) after successful rollback | `rejects_message`: caller records a terminal `settings_unavailable` marker and ACKs |
| Capacity (reply quota, replay markers, query cells) after successful rollback | `is_capacity`: nothing recorded, not ACKed |
| SQL/storage fault, failed COMMIT, failed rollback | `quarantines`: Settings receive is quarantined |

A failed rollback also latches the shared Store quarantine ([ADR-0013](../decisions/ADR-0013-permanent-store-quarantine.md)) while the connection mutex is still held. A COMMIT error stays uncertain even if rollback succeeds.

A query cell registered before a later capacity failure stays counted until it expires or the next connection clears the cells; capacity never refunds it.

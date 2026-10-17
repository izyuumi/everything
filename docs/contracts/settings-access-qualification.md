# AccessStatus qualification

`src/host/settings/service/paired/access.rs` decides, on a caller-supplied Main transaction, whether an AccessStatus query from a verified peer gets a summary or the fixed denial. It is used at receipt admission ([receipts](settings-access-receipts.md)), at reply publication ([publication](settings-reply-publication.md)) and when a queued reply is loaded for sending. The overall flow is in [the Access transport](settings-access-transport.md).

## Checks

Every qualification and recheck:

- matches the request's original Agent, state lease, Store connection, endpoint, Host mutation guard and live connection identity;
- reads the Host endpoint and the requesting peer's complete pinned registration, including revocation (shared pin reader `paired/pins.rs`, identities bounded before decoding);
- reads the peer's Settings grant head with the existing grant reader;
- observes the Access query cell in Authority before and after the reads; the cell's first floor and deadline must be unchanged and clocks must not regress.

Decision:

| Grant state | Decision | Reply family |
| --- | --- | --- |
| No head, revoked or expired grant | `GrantUnavailable` | `denial`, 4,096 bytes, `grant:null` |
| Active, issued, current peer keys | `Active` | `default`, 49,152 bytes, grant id and revision |
| Revoked Host/peer flag, changed peer keys, grant issued in the future | error (out of scope) | none |

Identity, storage, clock and corruption failures are errors, never a denial.

## Query cells

Authority (`src/host/settings/authority/paired.rs`) keeps RAM query cells for the current connection: at most 512 total and 64 per peer. A cell holds only immutable query intent, peer keys, first clock floor, fixed deadline and observation floors. An exact replay of a query keeps its first deadline; a conflicting reuse of a query id is rejected. Cells past their deadline are evicted before the capacity check, and all cells are cleared when a new connection registers its first query.

## Clocks

`issued_at` may be up to 5000 ms (`SETTINGS_CLOCK_SKEW_MS`) ahead of the Host wall clock. Request lifetime is at most 30 s. Reply expiry is the earliest of observation + 60 s, the query expiry and an Active grant's expiry; the monotonic reply deadline is derived from the original query deadline, never renewed.

A grant change between admission and publication does not change the charged family; the recheck fails and the query is dropped.

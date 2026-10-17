# Paired Settings Authority (Access only)

`src/host/settings/authority/paired.rs` holds the paired transport state inside the existing Settings `Authority` sessions mutex. It serves only the remote [Access transport](settings-access-transport.md). It creates no Caller, parent, session or grant; native callers and factories are unchanged.

## Host attachment

`attach_paired_transport` records the Host's live `SettingsHostIdentity` once. A different identity Arc is a conflict, even with equal public IDs. A closed Authority returns Stopped. `paired_receipt_owner` returns the grant owner (Host id, owner principal) only for that same registered, live identity.

## Access query cells

Each admitted `AccessStatus` registers one RAM cell keyed by (peer device id, query id). A cell holds the immutable intent, verified peer keys, receipt clock floor, original deadline and the last observed wall/monotonic times.

| Rule | Behaviour |
| --- | --- |
| Capacity | 512 cells, 64 per peer; over capacity returns Capacity |
| Expiry | Cells past their original deadline are evicted before the capacity check |
| Reuse | Same key with the same intent and peer is accepted; a different intent or peer is OutsideScope |
| Connection | Cells belong to one connection. A different connection may register only once the old one is no longer live and has a different epoch; all cells are then cleared. Otherwise Conflict |
| Close | Closing the paired transport clears all cells |

Registration and observation also require an open, uncancelled Authority, the registered live Host identity, a live connection on that Host, and a request addressed to this Host. Otherwise Stopped or OutsideScope.

## Observation

`observe_access_query` samples the wall and monotonic clocks under the mutex and requires:

- the cell exists for the same connection (absent: Expired if past the original deadline, else NotFound);
- both clocks are not below the cell floor, the receipt floor or the previous observation (clock regression is OutsideScope);
- the Host wall clock plus 5000 ms (`SETTINGS_CLOCK_SKEW_MS`) is not before `issued_at` (OutsideScope);
- the wall clock is before `expires_at` and the monotonic clock is before both deadlines (otherwise Expired).

A successful observation advances the cell's observed times and returns the grant owner, cell identity, floor, deadline and sampled clocks. Deadlines are never renewed.

## Not implemented

Bind parents, parent registration or withdrawal, paired command qualification, a paired-direct caller and paired update effects are not in this tree.

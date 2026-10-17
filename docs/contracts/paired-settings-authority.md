# Paired Settings Authority

`src/host/settings/authority/paired.rs` holds the paired transport state inside the existing Settings `Authority` sessions mutex. It serves only the remote [Settings transport](settings-access-transport.md). Native callers and factories are unchanged.

## Host attachment

`attach_paired_transport` records the Host's live `SettingsHostIdentity` once. A different identity Arc is a conflict, even with equal public IDs. A closed Authority returns Stopped. `paired_receipt_owner` returns the grant owner (Host id, owner principal) only for that same registered, live identity.

## Query cells

Each admitted request registers one RAM cell keyed by (peer device id, query id). A cell holds the immutable intent (including its body), verified peer keys, receipt clock floor, original deadline and the last observed wall/monotonic times.

| Rule | Behaviour |
| --- | --- |
| Capacity | 512 cells, 64 per peer; over capacity returns Capacity |
| Expiry | Cells past their original deadline are evicted before the capacity check |
| Reuse | Same key with the same intent and peer is accepted; a different intent or peer is OutsideScope |
| Connection | Cells belong to one connection. A different connection may register only once the old one is no longer live and has a different epoch; all cells are then cleared. Otherwise Conflict |
| Close | Closing the paired transport clears all cells and the paired caller |

Registration and observation also require an open, uncancelled Authority, the registered live Host identity, a live connection on that Host, and a request addressed to this Host. Otherwise Stopped or OutsideScope.

## Observation

`observe_access_query` samples the wall and monotonic clocks under the mutex and requires:

- the cell exists for the same connection (absent: Expired if past the original deadline, else NotFound);
- both clocks are not below the cell floor, the receipt floor or the previous observation (clock regression is OutsideScope);
- the Host wall clock plus 5000 ms (`SETTINGS_CLOCK_SKEW_MS`) is not before `issued_at` (OutsideScope);
- the wall clock is before `expires_at` and the monotonic clock is before both deadlines (otherwise Expired).

A successful observation advances the cell's observed times and returns the grant owner, cell identity, floor, deadline and sampled clocks. Deadlines are never renewed.

## Paired caller

`CallerSource::PairedSettings { device_id }` exists only inside one reply build, after current access was reread under the Host mutation guard.

- `paired_caller` requires the registered Host identity, the request's live connection (the one owning the query cells) and an unexpired request. It installs one lease (device, connection, request wall expiry, original monotonic deadline) as the single current paired caller and returns a `SettingsCaller` bound to it. `retire_paired_caller` clears it after the call.
- `Authority::validate`, `require_tool`, `session` and `lifetime` reject a paired caller. Generic owner, native, agent, credential, device, pairing and grant entry points therefore refuse it.
- `validate_paired_update` (via `require_update`) accepts it only while its lease is the current one, the Authority is open, the connection and Host identity are live and neither expiry has passed. `paired_lifetime` supplies the original binding expiry and deadline.
- The journal origin is `PairedDevice { device_id }` with the device id as `client_session_id`; only Patch targets are valid for it. `check_origin` for that origin requires the current lease for the same device, so the existing write-origin check before the settings file rename fails closed after disconnect or expiry.
- `check_principal` lets a paired caller replay only operations whose origin is the same device. File reconciliation rejects paired callers; the local owner keeps its cleanup authority over every origin.
- Fresh paired operations are capped at 32 retained identities per device and 128 in total within the shared 1024-identity journal; exact replay is checked before the cap.

Device revocation paths that do not take the Host mutation guard are not serialized with an in-progress paired write; the pin is rechecked when access is reread before the write.

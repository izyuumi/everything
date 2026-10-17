# Apple Bind client

Status: Swift source only. The iOS Settings screen exposes only the explicit [Access check](apple-settings-access-client.md); it has no Bind control. `AppModel` and `ReplicaService.requestSettingsBind` remain in the tree without a screen caller. The Host in this tree answers only `AccessStatus` and records Bind packets as rejected ([Host Access transport](settings-access-transport.md)), so no Bound reply can arrive.

## Intended behaviour of the source

- A Bind follows a genuine current Access observation. A one-use MainActor gesture captures `ContinuousClock` before wall time, one Bind UUID, the Service, scope token and originating Access handle. Labels, copied generations or IDs cannot supply this association.
- Consent `none` or `configuredProvider` may both request a management Bind. Bind performs no provider lookup, consent expansion, Snapshot, update or chat.
- It shares the single pending cell and the total-16 ordinary/private send window with Access. The first [association](apple-settings-bind-owner.md) is reserved before request construction and is never reset by errors, lost reply, expiry or reconnect; no retry, rebind or Host Close is inferred.

## Clocks

With Access floors `Wa/Ma`, observation fence `E/De`, Bind capture `Wb/Mb` and grant expiry `G`: `Qb=min(Wb+30000,E)`, `Db=min(Mb+(Qb-Wb),De)`, `P=min(Wb+300000,G)`, `Dp=min(Mb+(P-Wb),Ma+(P-Wa))`. A Bound expiry `B` must be current and no later than `P`/`G`; the adopted parent deadline is `Dp-(P-B)`. Nothing renews these bounds.

## Send and receive

Send, `Accepted`, reply return and failure are tracked per cell as for Access. Only an authenticated Fresh Bound reply matching the full Host/local identity, frame, encrypted `in_reply_to`, Bind UUID and generation, after metadata commit and final Store checks, can be adopted once. Verification failures are refused and ACKed per delivery as in the Access client; storage and capacity failures are not ACKed. Failures never create Bound or denial.

The UI state, if shown, would expose only stage, observation/expiry and freshness; no parent ID or generation. No runtime behaviour has been observed.

# Apple Bind requester and reply owner

Status: superseded Swift source without a Settings screen caller. The Host rejects Bind packets ([Host transport](settings-access-transport.md)); iOS uses the [query client](apple-settings-access-client.md).

This owner extends the [Access owner and cache](apple-remote-settings-wire.md) with one Bind original and private Bound qualification/adoption, used by the [Bind client](apple-settings-bind-client.md).

## Invariants

- `requestSettingsBind(_:)` accepts only the immutable input taken once from the MainActor gesture (Service, scope token, originating Access handle, first ID/clocks). Intake requires the current scope, a genuine Access cache with a summary, an empty Access/Bind/Snapshot/Inspection pending cell, window capacity and no existing association.
- At most one lean Bind association exists per connection scope. It holds request metadata, fixed clocks, the readable/writable key sequences copied from the Access summary, opaque handles, weak scope/capture references and the optional sealed parent. It holds no frame, proof, cache, Service or socket.
- The association is reserved before request construction; a construction failure still spends it. It is released once retired (cancel, failure, scope retirement, disconnect, stop), its scope is no longer current or its parent deadline has passed, and no pending Bind or send cell references it. Release is not Host Close or NoEffects.
- Clock bounds are as in the [Bind client](apple-settings-bind-client.md); no later observation produces now-plus-remaining.

## Preparation and adoption

- The pump seals once via `identity.sealSettings`, rechecking scope, cache, Store Host pin/disabled state and local identity around every await. The envelope keeps the original expiry, creation at or after the first floor and duration at most 30,000 ms.
- The receive owner registers the frame and Prepared original before crypto/Store work, calls `receiveClassified` once and commits metadata once. Qualification requires exact Host/local identity, `in_reply_to` equal to the encrypted request ID, matching Bind UUID and generation, response duration at most 60,000 ms and header expiry no later than the parent expiry.
- The qualification is file-private and single-use; the adopter performs the final Store/current-owner check and assigns without another await. The parent and generation have no getter.
- Limits: private plaintext 32,768, envelope 49,152, original frame 364,544 bytes.

## Failures

Retirement during Preparing/Receiving only invalidates usability; data is held until the owning helper returns. Store, commit, quarantine, capacity or conflict failure prevents adoption and ACK; a duplicate cannot re-adopt. Local errors are never treated as remote denial, Closed, NoEffects or retry permission. No runtime behaviour has been observed.

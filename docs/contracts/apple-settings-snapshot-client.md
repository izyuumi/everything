# Apple Snapshot client

Status: Swift source only. The iOS Settings screen exposes only the explicit [Access check](apple-settings-access-client.md); it has no Host values control. `ReplicaService.requestSettingsSnapshot` and its [Read codec](settings-scoped-reads.md) remain without a screen caller. The Host in this tree answers only `AccessStatus` and records Snapshot reads as rejected ([Host Access transport](settings-access-transport.md)).

## Intended behaviour of the source

- A Snapshot requires a genuine adopted [Bind parent](apple-settings-bind-owner.md). It requests exactly the readable-key sequence (nonempty, sorted, at most five keys) copied at Bind association creation. That is a historical ceiling, not current Host permission; the Host would have to check current scope.
- A one-use MainActor gesture captures `ContinuousClock` before wall, a query UUID, Service, scope token and binding handle. Intake reserves the single pending cell and one cell in the total-16 send window before construction or crypto. A per-capture `OSAllocatedUnfairLock<Bool>` once bit is spent before a new pending owner; failure or interleaving never resets it.
- Clocks: with gesture floors `Ws/Ms`, Bound expiry `B` and parent deadline `Dp`, `Qs=min(Ws+30000,B)` and `Ds=min(Ms+(Qs-Ws),Dp)`. Nothing renews them.

## Receive

Only the pending Snapshot selects this branch; there is no fallback to another family. Full Host/local identity, frame, encrypted ID versus query ID, generation and clocks are checked; metadata commits (`payload=NULL`, `done=1`) before ACK; only a genuine Fresh reply passing final Store checks is adopted. Limits: packet 32,768, envelope 49,152, denial 2,048/4,096, original frame 364,544 bytes.

## Display

One bounded RAM cache of typed endpoint-free values with the original fences; no raw reply, parent, generation, Service or socket. A failed or cancelled refresh clears copied values; revocation, pin mismatch or reduced Access clears them. No endpoint, credential, secret or writable control is exposed.

No runtime behaviour has been observed.

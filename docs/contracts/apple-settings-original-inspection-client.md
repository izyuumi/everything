# Apple Original inspection client

Status: superseded Swift source with no caller. The Host has no permanent Originals and rejects this read shape ([Host transport](settings-access-transport.md)). iOS recovers an uncertain change by resending the same Update `request_id` through the [query client](apple-settings-access-client.md); the notes below describe the legacy source only.

## Original target and genuine parent

- Targets are a canonical Original request UUID and an optional Send operation UUID, each at most 36 UTF-8 bytes, captured before the owned Task. They are lookup metadata, not authority; the source never generates replacement mutations.
- A one-use MainActor gesture captures `ContinuousClock` before wall, a query UUID, Service, scope and binding handle. A per-capture `OSAllocatedUnfairLock<Bool>` once bit is spent before a new pending owner or crypto and is never reset by failure, retirement or interleaving.
- It requires a genuine adopted [Bind parent](apple-settings-bind-owner.md). With gesture floors `Ws/Ms` and parent expiry/deadline `B/Dp`: `Q=min(Ws+30000,B)`, `D=min(Ms+(Q-Ws),Dp)`. Nothing renews them.

## Send and receive

Shares the single pending cell and the total-16 send window; one send, no resend. Only the pending Inspection selects its classifier; there is no fallback. Plaintext/parser gates allow 65,536 bytes only for Inspection replies (envelope 90,112, frame 364,544); requests keep 32,768/49,152; denial 2,048/4,096. Metadata commits before ACK; only a genuine Fresh reply passing final Store checks is adopted.

## Display

A RAM cache of endpoint-free observations would distinguish NotRecorded, Unknown, NotRequested, Applying and receipt/write/application states. A missing record permits no NoEffects inference, retry or replacement ID. Failed or cancelled refresh clears the copy.

No runtime behaviour has been observed.

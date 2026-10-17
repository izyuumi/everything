# Apple Update client

Status: superseded Swift source. `ReplicaSettingsUpdateGesture` and `ReplicaService.requestSettingsUpdate` have no caller, and the Host rejects their parent-bound command shape ([Host transport](settings-access-transport.md)). iOS writes through the generation-scoped `update` query of the [query client](apple-settings-access-client.md). The legacy codec is in [Update wire data](apple-settings-update-wire.md); the notes below describe the legacy source only.

## Intended behaviour of the source

- The gesture captures `ContinuousClock` before wall, an Original request UUID, Service, scope and Bind handle, a positive expected Settings revision and at least one embeddings enabled/dimensions change (`nil` = no change; `backendDefault` is an explicit change; dimensions 1...65536).
- Admission requires the same Service, scope, socket and a genuine adopted [Bind parent](apple-settings-bind-owner.md). Each change must fit the writable-key ceiling copied from the Access summary at Bind creation; that ceiling is not current Host permission.
- Expiry is the earlier of first wall + 30,000 ms and parent expiry; the monotonic deadline is clipped from the same capture. Nothing renews them.
- At most one Update Original exists at a time. It is released once it has a final status or its scope is no longer current, and no pending Update or send cell references it. A later Update is a new Original guarded by its expected revision.
- One send, no resend. The pending/window owner stays charged until the pre-send Store helper returns or unwinds. Metadata commits before ACK; only a genuine matching Fresh reply yields the finite outcome once.
- Limits: request 32,768 / envelope 49,152; reply 65,536 / envelope 90,112; frame 364,544 bytes.

No writable form, retry, reset, rebind, reconciliation or recovery exists. Local errors never become denial, Saved, NoEffects or retry permission. No runtime behaviour has been observed.

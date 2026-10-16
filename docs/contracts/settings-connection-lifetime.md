# Local Settings relay lifetime

This source adds an opaque lifetime for the original Host relay owner and each genuinely connected relay socket. It does not authenticate an E2EE peer or accept a private Settings packet. The current [private ingress rejection](private-settings-ingress.md) remains unchanged. [#116](https://github.com/izyuumi/everything/issues/116) stays open.

The Host factory source is present in `host/settings/transport.rs`; its module declaration and actual Host worker wiring are a separate coordinator integration. Until that wiring, the existing Host still calls the ordinary public `run_relay` path. These source leaves remain unpublished pending real Core/owned-request consumers, independent review and exact-head compilation. Formatting is not type checking or runtime verification.

## Owner and identity

`host_settings_transport(agent, endpoint, mutation)` is declared inside the Host descendant and retains the approved three-argument boundary. Its device-module constructor requires actual Host role, the same actual Main connection as `agent.store()` and a present Agent StateLease. The factory must receive the original Agent and mutation Arc from the existing Host worker; it does not construct another Agent, Store, mutex, Authority or runtime. Matching a serialized Host UUID is insufficient.

The noncloneable `HostSettingsTransportOwner` retains that actual Agent, endpoint and mutation Arc. Its Agent clone retains the original lease. The lightweight cloneable `HostSettingsTransport` handle retains only the exact `SettingsHostIdentity` Arc. The separate full owner is moved into the existing relay worker, without a queue or HostInner back-reference.

`SettingsHostIdentity` contains only the actual Host ID, a fresh Host epoch, a permanent closed atomic and cancellation token. `SettingsConnectionIdentity` contains only that lightweight Host identity Arc, a fresh connection epoch, a permanent retired atomic and child cancellation token. Neither identity can reach an Agent, StateLease, Store, endpoint, mutation Arc, Host context, queue, callback or service. Core may retain these lightweight identities without an Authority-to-Agent cycle.

All fields and constructors are private except the narrow trusted crate interfaces. These types have fixed redacted Debug and no serialization. Epochs are generated with the established random source and UUID builder; they label actual Arc incarnations and are not authorization secrets. Later association compares exact Arc identity, not matching IDs or epochs reconstructed from a packet or database row.

## Successful connection and retirement

Public `DeviceEndpoint::run_relay` keeps its signature and delegates to one shared driver without a Settings owner. The crate-private `run_relay_with_settings` delegates to the same driver with the original owner. Both use the same existing session mutex, network pump, ordinary receive/send logic, retry classification and backoff.

Only the actual `Ok(mut connection)` branch following `RelayConnection::connect` calls `connected_relay`. This validates the exact endpoint inner Arc and live Host owner, then creates a noncloneable guard borrowing that owner mutably. It allocates a new connection identity, never reusing an old Arc or epoch. The passed `RelayConnection` reference ties the private callsite to the real connection; it is not a peer proof or an independent socket-state validator.

The guard spans the actual relay pump await. It is explicitly dropped synchronously before `connection.close().await`, retry or return. Its Drop permanently marks the connection retired and cancels its token, including cancellation/unwind exits. Owner mismatch or epoch-generation failure permanently closes the Host identity before returning an error. Dropping the full owner also permanently closes the Host identity and cancels its token. Retry retains that same Host identity and creates a different connection identity after a later genuine successful connection.

Read-only identity accessors expose original Host ID/epoch, original connection epoch, borrowed lightweight Host identity and liveness. Connection cancellation access returns a child token; callers never receive the stored token or a reset method. Cancelling an observation child does not renew or directly retire the original incarnation. `is_live` requires the relevant permanent atomic and original cancellation facts to remain live. No closing/retirement transition can be reversed.

## Authority and lifetime limits

Relay authentication establishes that the local endpoint authenticated to the relay. It does not establish an envelope sender's current full pin, successful E2EE opening, a valid private request, committed receipt, grant, parent or SettingsCaller. This slice creates no `AuthenticatedSettingsSource`, `OwnedCommittedSettingsRequest`, paired parent/caller, private packet, body, reservation, ciphertext publisher or send owner. The [transport storage foundation](settings-transport-storage.md) remains a distinct prerequisite.

A later authenticated source must borrow the actual registered request owner retaining the genuine Agent/Main/lease/mutation context, complete verified peer pin, successful original commit and exact lightweight connection identity. Authority must compare its own expected original context and current grant/pin, then recheck liveness after waits and at actual guarded effect boundaries. An unlocked liveness observation, epoch or Arc label is necessary correlation only.

The lean guard owns no private worker or output. Synchronous Drop invalidates intake identity; it does not prove that any worker joined, a buffer was destroyed, an effect is known or a reservation can be refunded. Existing ordinary `blocking` can detach a spawn_blocking job when its waiter drops. Accepted private receiving must instead register and retain original jobs, raw/typed/returned buffers, actual lease and guard ownership through true joins and preserve quarantine on failure. This source does not strengthen the existing ordinary shutdown claim.

## Validation boundary

Direct Rust1.88 formatting/parsing, diff and surgical preservation checks are appropriate for this held slice. The unchanged ordinary pump, six packet lanes, crypto, Store and retry/error policies require no invented validation result. No local Cargo/compiler, database, network or runtime observation was performed; no automated tests, fixtures, source comments, dependencies or dead-code suppressions were added. Genuine Core and owned-request consumers, coordinator module/worker integration, independent exact-source review and strict current-head CI remain gates before publication. Security/privacy readiness and Apple private transport behavior are unverified; automated coverage remains deferred to [#32](https://github.com/izyuumi/everything/issues/32).

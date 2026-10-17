# Settings relay connection lifetime

`src/devices/settings_connection.rs` owns the Host Settings transport identity and one owner per connected relay socket. The flow it serves is in [the Access transport](settings-access-transport.md).

## Host owner

`host_settings_transport` (`src/host/settings/transport.rs`) calls `HostSettingsTransportOwner::from_host` with the Host Agent, endpoint and mutation Arc. It requires the Host role, the Agent's Store connection and a present state lease. The Host then installs the `SettingsRelayCapability`, registers the lightweight `HostSettingsTransport` handle and only then calls `SettingsService::bind_paired_transport` (`src/host/runtime.rs`). If any step fails, the relay worker runs plain `run_relay` without Settings.

`SettingsHostIdentity` holds the Host id, a fresh Host epoch, a permanent closed flag and a cancellation token. Dropping or failing the owner closes it permanently.

## Per-connection owner

After a successful `RelayConnection::connect`, `connected_relay` checks the endpoint and live Host identity, captures the authenticated [socket identity](relay-socket-identity.md), allocates a fresh connection epoch and connects receive Work. `SettingsConnectionIdentity` is live only while the Host identity, socket identity, its own retired flag and its cancellation token are all live. Epochs label incarnations; they are not secrets or authority.

Dropping `SettingsConnectionOwner` retires the identity and closes Work intake. `drain_connection` then joins Work jobs, retires queued replies and requires a complete ledger before the next connect. A drain failure latches the owner failed; the relay reports Failed and Host shutdown returns an error instead of hanging.

## Limits

Relay authentication does not authenticate an envelope sender, grant or query. Those checks belong to receipt admission and [qualification](settings-access-qualification.md). Liveness is correlation only.

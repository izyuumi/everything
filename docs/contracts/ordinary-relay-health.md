# Ordinary relay ownership health

All clones of one `DeviceEndpoint` share a monotonic `outbound_failed` flag (`src/devices.rs`), initially false. `quarantine_outbound` sets it; there is no reset. `check_outbound_health` returns `DeviceError::OutboundQuarantined` once it is set. These checks do no SQL or locking.

The [send window](ordinary-relay-send-window.md) sets the flag when it must retain unfinished ownership: a bookkeeping mismatch, a preparation it cannot join at finish, or being dropped with unfinished cells. Ordinary network errors do not set it; they stay eligible for durable retry. The window checks health on creation and before each send, and the session checks it before selecting a candidate.

Host transport drain requires the endpoint not to be quarantined and the Settings receive witness, if present, to be complete. During Host shutdown a quarantined endpoint, or a failed Settings receive (bookkeeping not ok or terminal quarantine), makes the transport wait return an error instead of blocking (`HostInner::settings_transport_failed`). A clear flag proves no join or completed drain. A new endpoint does not prove the old one drained.

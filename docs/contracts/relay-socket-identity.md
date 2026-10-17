# Relay socket identity

Each successful `RelayConnection::connect` (`src/transport/remote.rs`) creates one opaque `Arc<RelaySocketIdentity>` after relay authentication. It holds a monotonic closed flag and the connection's cancellation token. It is live while the flag is unset and the token is not cancelled. There is no serialized form, reset or public constructor.

- `capture_socket_identity` returns that Arc only for the connection's own authenticated device, with an uncancelled token, a present socket and a live identity. Otherwise Protocol, Cancelled or Closed.
- `send` and `receive` refuse to use the socket once the identity is no longer live: the socket is dropped and they return Cancelled or Closed. Early `send` validation errors (wrong sender, bad ciphertext size, authentication frames) leave the socket and identity unchanged.
- A send or receive failure closes the identity and clears the socket. `close` closes the identity before its bounded socket close; dropping the connection also closes it.

`HostSettingsTransportOwner::connected_relay` captures this identity for each [Settings connection](settings-access-transport.md); the Settings connection is live only while it is. Closing the identity proves no network close, task join or completed drain.

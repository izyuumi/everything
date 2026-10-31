# Transport

The shared [crypto envelope protocol](../security/relay-protocol.md) implements authenticated ciphertext, device key pinning, pairing and replay acceptance boundaries. Typed sync contracts carry encrypted per-conversation cursors and host events.

The [optional relay server](relay.md) now routes authenticated ciphertext over bounded WebSockets with durable offline queues, acknowledgement/deduplication, expiry, operator enrollment/revocation and rate limits. It has no conversation plaintext or decryption keys. Its `vorra relay` subcommands and disposable runtime checks are documented alongside known limits.

Host device trust, durable receiver inbox/replay storage, client synchronization and native clients remain separate downstream implementations. Relay enrollment and host pairing are separate explicit trust steps. Local IPC/background service is in focused integration; [implementation status](../status/implementation.md) is the current gate reference.

## Private Settings rejection

The Rust endpoint classifies authenticated packet routing inside the existing replay callback before ordinary inbox payload copying. Six ordinary lanes retain original bytes and their existing processor; Settings and invalid/ambiguous routing receive fixed terminal metadata-only refusals. The shared Store transaction validates the same full pin/active recipient and exact ciphertext replay, then commits a rejected row with no payload or command ID. Successful transport receipt may be acknowledged without admitting a Settings operation or sending a denial response.

This boundary admits no scoped Settings caller, session or mutation and changes no native cryptographic API. Existing marker capacity and relay-error behavior remain; accepted-ingress authority, ownership, backpressure and Apple pre-copy routing remain separate work. See [the rejection contract](../contracts/private-settings-ingress.md) for exact compatibility and historical-payload limits.

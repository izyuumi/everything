# Transport

The shared [crypto envelope protocol](../security/relay-protocol.md) implements authenticated ciphertext, device key pinning, pairing and replay acceptance boundaries. Typed sync contracts carry encrypted per-conversation cursors and host events.

The [optional relay server](relay.md) now routes authenticated ciphertext over bounded WebSockets with durable offline queues, acknowledgement/deduplication, expiry, operator enrollment/revocation and rate limits. It has no conversation plaintext or decryption keys. Its CLI and disposable runtime checks are documented alongside known limits.

Host device trust, durable receiver inbox/replay storage, client synchronization and native clients remain separate downstream implementations. Relay enrollment and host pairing are separate explicit trust steps. Local IPC/background service is in focused integration; [implementation status](../status/implementation.md) is the current gate reference.

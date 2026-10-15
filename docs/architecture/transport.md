# Transport

`src/crypto.rs` defines the encrypted envelope boundary shared by the local host, relay, and optional clients. Typed commands, conversation events, synchronization payloads, cursors, and command IDs serialize inside ciphertext. The relay sees versioned routing metadata, authenticated sender/recipient device IDs, envelope IDs, timestamps, size, ciphertext, and a sender signature. It can validate routing and expiry without decryption keys.

Each envelope uses fresh randomness, X25519/HKDF-SHA-256 key derivation, ChaCha20-Poly1305 authenticated encryption, and an Ed25519 signature. Receiver processing validates the trusted sender, recipient, lifetime, signature, and ciphertext before atomically persisting plaintext into a durable replay-protected inbox. Application command authorization and event sequencing follow that boundary.

The foundation provides mutual key-pinning pairing messages and challenge signatures. Relay routing, offline delivery, acknowledgment handling, reconnect behavior, trust registry storage, revocation propagation, and client synchronization remain the work of #13 and #22. The foundation alone is not a running transport service. See the [wire contract](../security/relay-protocol.md) and [security limits](../security/threat-model.md).

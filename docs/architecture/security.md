# Security

The cryptographic foundation lives in `src/crypto.rs`: separate Ed25519/X25519 device keys, signed ChaCha20-Poly1305 envelopes, short-lived possession-authenticated pairing, relay challenge signatures, strict expiry/size validation, and a mandatory durable replay/inbox boundary. Secret key exports and decrypted payloads use zeroizing memory.

Callers supply pinned active peer keys. The module does not create a trusted device registry, persist identity secrets, authorize commands, enforce tool permissions, or distribute revocations. A receiver must implement `ReplayGuard` by atomically recording an authenticated inbox payload and a unique replay marker before acknowledging delivery. Native shell execution requires an explicit trust boundary beyond application-level path validation.

See [versioned wire contract](../security/relay-protocol.md) and [threat model](../security/threat-model.md). Static identity key agreement does not provide forward secrecy. Metadata remains visible to the relay. Rust/Swift interoperability and automated security validation are deferred to #32; no security audit has been completed.

## Fresh private packet refusal

The Rust device receiver now refuses Settings routing before any fresh ordinary inbox plaintext copy. It stores only authenticated envelope metadata, its unchanged ciphertext digest and a fixed rejection code; parser/private Store errors are not stringified into that record. Duplicate routing keys, non-object/non-string routing and unknown lanes fail closed through standard serde. Valid ordinary bytes remain unchanged, so this cannot detect arbitrary private content mislabeled as an ordinary packet.

This is a rejection boundary, not remote management authorization. It does not erase historical rows, change the Apple receive path, strengthen the existing detached-worker/Agent-lease lifetime or establish runtime privacy/security acceptance. Actual accepted remote Settings still requires live grants, scoped authority, durable originals and reviewed worker/output ownership. [The contract](../contracts/private-settings-ingress.md) records those limits.

# Encrypted transport version 1

The implementation is `src/crypto.rs`. JSON is the transport representation, while signatures and authenticated encryption use the binary encodings below. JSON object ordering and whitespace have no cryptographic meaning. Unknown JSON fields are rejected. UUIDs are non-nil UUID strings in JSON and exactly 16 bytes in canonical encodings. All integers are unsigned, fixed width, big endian. All byte strings in JSON use padded RFC 4648 standard base64. Fixed-size fields must decode to their exact length.

The relay receives an `EncryptedEnvelope`, never the serialized command, conversation, event, sync request, pairing secret, or wiki payload. Command IDs and event synchronization cursors belong inside the encrypted payload. `MessageId` identifies an envelope; it is separate from the durable command ID and conversation event sequence in `protocol.rs`. Application parsing and authorization happen after authenticated decryption.

## Device identity

`DeviceIdentity` holds an Ed25519 signing seed and a separate X25519 secret, generated using operating system randomness. `DevicePublicKeys` contains a device UUID, 32-byte Ed25519 verification key, and 32-byte X25519 public key. Its canonical encoding is UUID bytes, signing key bytes, then encryption key bytes. Weak Ed25519 keys and zero X25519 public keys are rejected, as are noncontributory X25519 agreements. The fingerprint is SHA-256 of the canonical public-key encoding, displayed as eight hyphen-separated groups of eight lowercase hex digits.

A device UUID does not authenticate its keys. The receiver must look up the pinned active public keys in its local paired-device registry, reject revoked devices, then pass those keys to `open`. The relay must likewise pin the public keys associated with authenticated device registrations. Key substitution requires a fresh explicit pairing decision.

`export_secrets` exists for integration with protected OS credential storage. It returns zeroizing memory and does not serialize or log secret material. Durable secret storage and revocation propagation are integration responsibilities of #22; this module does not persist keys or manage an OS keychain.

## Envelope

`EnvelopeHeader` contains `version`, `message_id`, `sender`, `recipient`, `created_at_ms`, `expires_at_ms`, `nonce`, and `key_salt`. The version is 1. Timestamps are UTC Unix milliseconds. The nonce is 12 random bytes and the key salt is 32 random bytes, generated fresh for every envelope.

The canonical header is the concatenation of:

1. The ASCII bytes `everything.envelope.v1` followed by one zero byte.
2. The 2-byte version.
3. The 16-byte message UUID, 16-byte sender UUID, and 16-byte recipient UUID, in that order.
4. The 8-byte creation time and 8-byte expiration time.
5. The 12-byte nonce and 32-byte key salt.

The sender and recipient derive the same X25519 shared secret from their static identity encryption keys. HKDF-SHA-256 extracts with the header key salt and expands to 32 bytes using the info string `everything.envelope.key.v1` followed by one zero byte, the canonical sender public keys, then canonical recipient public keys. These 32 bytes are the ChaCha20-Poly1305 key. The nonce is the header nonce and the associated data is the entire canonical header.

`ciphertext` contains ChaCha20-Poly1305 ciphertext followed by the 16-byte authentication tag. `signature` is the 64-byte Ed25519 signature over the canonical header followed by a 4-byte ciphertext length and the ciphertext including its tag. The relay can check this signature and expiry without access to either encryption secret or plaintext.

Plaintext must contain between 1 and 1,048,576 bytes. Ciphertext is at most 1,048,592 bytes. `seal` creates a new envelope ID and rejects sending to the same device. Lifetime must be positive and at most seven days. A receiver rejects expiry at or before its current time and creation more than 120 seconds in its future. It validates version, identities, lifetime, sender signature, and AEAD authentication before releasing plaintext.

Transport handlers must impose frame and request-body limits before JSON parsing. Base64 deserializers validate encoded and decoded bounds, but a JSON parser can allocate an incoming string before visiting the field. Message size bounds do not replace connection, queue, per-device, or concurrency limits in the relay.

## Replay and durable delivery

`open` requires a `ReplayGuard`; there is no public decryption method that bypasses it. After signature and AEAD verification it calls `record_if_new(record, plaintext, now_ms)` before returning zeroizing plaintext. `ReplayRecord` includes the message ID, authenticated sender and recipient, and expiry. The receiver must atomically store a replay marker and a durable inbox copy of the plaintext. `false` means a duplicate; a storage error fails delivery. The adapter must provide atomic uniqueness under concurrent reception and retain the marker through expiry. Pending inbox records remain until processing has durably completed, even after transport expiry.

The plaintext parameter permits the adapter to commit inbox data and deduplication in one transaction. A replay marker written without its inbox payload can lose a command if the process crashes before dispatch. A receiver must acknowledge relay delivery only after durable acceptance. Repeated commands encrypted into distinct envelopes require a second application-level command-ID deduplication boundary; transport replay checks do not replace it. Event sequence cursors and gap recovery are application state inside ciphertext, implemented by #22 rather than by this cryptographic module.

## Pairing

The pairing workflow is mutual key pinning using a short-lived, single-use, out-of-band invitation. `begin_pairing` creates a `PendingPairing` containing a signed invitation with a random 32-byte secret. The maximum lifetime is ten minutes. A caller displays the invitation over a trusted local QR or equivalent channel, with the inviter key fingerprint available for human verification. Serialization intentionally includes the secret; never send the invitation through the relay, put it in logs, or expose it in an ordinary deep-link query or telemetry.

The invitation fields are version, pairing message UUID, inviter public keys, creation and expiration timestamps, secret, and inviter signature. Its signature covers `everything.pairing.invitation.v1` followed by a zero byte, the 2-byte version, 16-byte pairing ID, canonical inviter keys, 8-byte creation and expiration times, and 32-byte secret.

After the user accepts the inviter identity, `PairingInvitation::create_request` validates that invitation and constructs a `PairingRequest`. Its proof bytes are `everything.pairing.request.v1` followed by a zero byte, the 2-byte version, 16-byte pairing ID, 16-byte inviter device ID, canonical responder public keys, and 8-byte expiration. `proof` is HMAC-SHA-256 keyed by the invitation secret over these bytes. The responder Ed25519 signature covers those proof bytes followed by the 32-byte HMAC. The request contains no invitation secret. Intercepting a request does not permit choosing a different responder key.

`PendingPairing::accept` consumes the pending invitation, verifies the expiry, pinned inviter identity, invitation ID, responder signature, and HMAC, then returns the responder public keys and a `PairingAcceptance`. An invalid acceptance attempt consumes the pending invitation too; generate a new invitation after failure. A pending invitation has no reconstruction API and cannot be cloned. The pairing UI must retain exactly one pending instance and avoid accepting multiple independently reconstructed pending invitations.

The acceptance signature covers `everything.pairing.acceptance.v1` followed by a zero byte, the 2-byte version, 16-byte pairing ID, canonical inviter keys, canonical responder keys, and 8-byte expiration. `PendingPairingCompletion::finish` verifies this signature and the exact pair of keys before returning the inviter keys for durable pinning. A registry must reject an already paired or revoked UUID whose keys differ, and persist pinning before reporting pairing complete.

The module authenticates pairing messages; transporting a request/acceptance, displaying the QR, approval UI, and storing the trusted keys are #22 responsibilities. Requests can be sent over an untrusted channel after out-of-band invitation transfer because they carry authenticated public data and a possession proof. Sender metadata and pairing relationships remain visible on such a channel.

## Relay challenge authentication

`sign_challenge` and `verify_challenge` use Ed25519 over `everything.authentication.v1` followed by a zero byte and the exact challenge bytes. Challenge lengths are 1–4,096 bytes. The relay protocol must provide a fresh cryptographically random nonce, expected device ID, canonical relay origin, session binding, and expiry in the challenge, consume it once, and require TLS. The client must validate the origin/session context before signing. Envelope signatures authenticate messages independently of WebSocket authentication.

## Validation

Rust formatting, all-target checking, Clippy with warnings denied, release compilation, and disposable manual runtime exercises are required for this implementation. Deterministic interoperability vectors, negative automated tests, and Rust/Swift interoperability are deferred to consolidated issue #32 under the explicit instruction to create no tests during this stage. This document specifies the encoding; it is not evidence of Apple CryptoKit interoperability or a completed security audit.

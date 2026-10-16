import CryptoKit
import Foundation

struct EnvelopeHeader: Sendable {
    let messageID: UUID
    let sender: UUID
    let recipient: UUID
    let created: UInt64
    let expires: UInt64
    let nonce: Data
    let salt: Data

    init(_ value: WireValue) throws {
        let fields = try value.object(keys: ["version", "message_id", "sender", "recipient", "created_at_ms", "expires_at_ms", "nonce", "key_salt"], required: ["version", "message_id", "sender", "recipient", "created_at_ms", "expires_at_ms", "nonce", "key_salt"])
        guard try fields.required("version").integer == 1 else { throw WireError.invalid }
        messageID = try fields.required("message_id").identifier
        sender = try fields.required("sender").identifier
        recipient = try fields.required("recipient").identifier
        created = try fields.required("created_at_ms").integer
        expires = try fields.required("expires_at_ms").integer
        nonce = try fields.required("nonce").bytes(count: 12)
        salt = try fields.required("key_salt").bytes(count: 32)
    }

    init(sender: UUID, recipient: UUID, created: UInt64, lifetime: UInt64) throws {
        guard lifetime > 0, lifetime <= 604_800_000, created <= UInt64.max - lifetime else { throw WireError.expired }
        messageID = UUID()
        self.sender = sender
        self.recipient = recipient
        self.created = created
        expires = created + lifetime
        nonce = try secureRandom(12)
        salt = try secureRandom(32)
    }

    var wire: WireValue { .object(["version": .unsigned(1), "message_id": .id(messageID), "sender": .id(sender), "recipient": .id(recipient), "created_at_ms": .unsigned(created), "expires_at_ms": .unsigned(expires), "nonce": .bytes(nonce), "key_salt": .bytes(salt)]) }
    var canonical: Data {
        var data = Data.domain("everything.envelope.v1")
        data.appendBE(UInt16(1))
        data += WireID.bytes(messageID) + WireID.bytes(sender) + WireID.bytes(recipient)
        data.appendBE(created)
        data.appendBE(expires)
        data += nonce + salt
        return data
    }

    func validate(now: UInt64) throws {
        let latestCreation = now + min(120_000, UInt64.max - now)
        guard sender != recipient, expires > created, expires - created <= 604_800_000,
              created <= latestCreation else { throw WireError.invalid }
        guard expires > now else { throw WireError.expired }
    }
}

struct EncryptedEnvelope: Sendable {
    let header: EnvelopeHeader
    let ciphertext: Data
    let signature: Data

    init(_ value: WireValue) throws {
        let fields = try value.object(keys: ["header", "ciphertext", "signature"], required: ["header", "ciphertext", "signature"])
        header = try EnvelopeHeader(fields.required("header"))
        let encoded = try fields.required("ciphertext").text
        guard encoded.utf8.count <= 1_398_124, let bytes = Data(base64Encoded: encoded), bytes.count > 16,
              bytes.count <= 1_048_592, bytes.base64EncodedString() == encoded else { throw WireError.size }
        ciphertext = bytes
        signature = try fields.required("signature").bytes(count: 64)
    }

    init(header: EnvelopeHeader, ciphertext: Data, signature: Data) {
        self.header = header
        self.ciphertext = ciphertext
        self.signature = signature
    }

    var wire: WireValue { .object(["header": header.wire, "ciphertext": .bytes(ciphertext), "signature": .bytes(signature)]) }
    var signedBytes: Data {
        var data = header.canonical
        data.appendBE(UInt32(ciphertext.count))
        data += ciphertext
        return data
    }
}

protocol DurableEnvelopeInbox: Sendable {
    func acceptAuthenticated(_ envelope: EncryptedEnvelope, plaintext: Data, host: DevicePublicKeys, localID: UUID) async throws -> Bool
}

actor DeviceIdentity {
    let publicKeys: DevicePublicKeys
    private let signing: Curve25519.Signing.PrivateKey
    private let encryption: Curve25519.KeyAgreement.PrivateKey

    init(deviceID: UUID, signingSeed: Data, encryptionSecret: Data) throws {
        signing = try Curve25519.Signing.PrivateKey(rawRepresentation: signingSeed)
        encryption = try Curve25519.KeyAgreement.PrivateKey(rawRepresentation: encryptionSecret)
        publicKeys = try DevicePublicKeys(deviceID: deviceID, signingKey: signing.publicKey.rawRepresentation, encryptionKey: encryption.publicKey.rawRepresentation)
    }

    func sign(_ data: Data) throws -> Data { try signing.signature(for: data) }

    func signChallenge(_ data: Data) throws -> Data {
        guard !data.isEmpty, data.count <= 4_096 else { throw WireError.size }
        return try signing.signature(for: Data.domain("everything.authentication.v1") + data)
    }

    private func key(peer: DevicePublicKeys, header: EnvelopeHeader) throws -> SymmetricKey {
        let peerKey = try Curve25519.KeyAgreement.PublicKey(rawRepresentation: peer.encryptionKey)
        let shared = try encryption.sharedSecretFromKeyAgreement(with: peerKey)
        let own = publicKeys
        let sender = header.sender == own.deviceID ? own : peer
        let recipient = header.sender == own.deviceID ? peer : own
        return shared.hkdfDerivedSymmetricKey(using: SHA256.self, salt: header.salt, sharedInfo: Data.domain("everything.envelope.key.v1") + sender.canonical + recipient.canonical, outputByteCount: 32)
    }

    func seal(_ plaintext: Data, recipient: DevicePublicKeys, now: UInt64) throws -> EncryptedEnvelope {
        guard !plaintext.isEmpty, plaintext.count <= 1_048_576, recipient.deviceID != publicKeys.deviceID else { throw WireError.size }
        let header = try EnvelopeHeader(sender: publicKeys.deviceID, recipient: recipient.deviceID, created: now, lifetime: 604_800_000)
        let sealed = try ChaChaPoly.seal(plaintext, using: key(peer: recipient, header: header), nonce: ChaChaPoly.Nonce(data: header.nonce), authenticating: header.canonical)
        let bytes = sealed.ciphertext + sealed.tag
        let unsigned = EncryptedEnvelope(header: header, ciphertext: bytes, signature: Data())
        return EncryptedEnvelope(header: header, ciphertext: bytes, signature: try sign(unsigned.signedBytes))
    }

    func receive(_ envelope: EncryptedEnvelope, host: DevicePublicKeys, into inbox: any DurableEnvelopeInbox, now: UInt64) async throws -> Bool {
        guard envelope.header.sender == host.deviceID, envelope.header.recipient == publicKeys.deviceID,
              host.deviceID != publicKeys.deviceID else { throw WireError.identity }
        try envelope.header.validate(now: now)
        try host.verify(envelope.signature, message: envelope.signedBytes)
        let ciphertext = envelope.ciphertext.dropLast(16)
        let tag = envelope.ciphertext.suffix(16)
        let box = try ChaChaPoly.SealedBox(nonce: ChaChaPoly.Nonce(data: envelope.header.nonce), ciphertext: ciphertext, tag: tag)
        let plaintext = try ChaChaPoly.open(box, using: key(peer: host, header: envelope.header), authenticating: envelope.header.canonical)
        guard !plaintext.isEmpty, plaintext.count <= 1_048_576 else { throw WireError.size }
        return try await inbox.acceptAuthenticated(envelope, plaintext: plaintext, host: host, localID: publicKeys.deviceID)
    }
}

extension WirePermissionInspection {
    func verifyArguments() throws {
        let digest = SHA256.hash(data: Data(argumentsJSON.utf8)).map { String(format: "%02x", $0) }.joined()
        guard digest == argumentsDigest else { throw WireError.signature }
    }
}

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

enum ReplicaSettingsReplyCorrelation: Sendable, Equatable, CustomStringConvertible, CustomDebugStringConvertible {
    case query(queryID: UUID)
    case bind(bindingRequestID: UUID)

    var description: String { "ReplicaSettingsReplyCorrelation([redacted])" }
    var debugDescription: String { description }
}

struct ReplicaSettingsReplyMetadata: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    let header: EnvelopeHeader
    let inReplyTo: UUID
    let correlation: ReplicaSettingsReplyCorrelation
    let generation: RemoteSettingsGeneration?

    fileprivate init(header: EnvelopeHeader, inReplyTo: UUID,
                     correlation: ReplicaSettingsReplyCorrelation, generation: RemoteSettingsGeneration?) {
        self.header = header
        self.inReplyTo = inReplyTo
        self.correlation = correlation
        self.generation = generation
    }

    var description: String { "ReplicaSettingsReplyMetadata([redacted])" }
    var debugDescription: String { description }
}

enum ReplicaSettingsMetadataDisposition: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    case fresh
    case duplicate

    var description: String { "ReplicaSettingsMetadataDisposition([redacted])" }
    var debugDescription: String { description }
}

fileprivate enum VerifiedReplicaSettingsBody: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    case access(RemoteSettingsAccessReply)
    case bind(RemoteSettingsBindReply)

    var description: String { "VerifiedReplicaSettingsBody([redacted])" }
    var debugDescription: String { description }
}

final class VerifiedReplicaSettingsReply: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    private let envelope: EncryptedEnvelope
    private let host: DevicePublicKeys
    private let localID: UUID
    private let body: VerifiedReplicaSettingsBody

    fileprivate init(envelope: EncryptedEnvelope, host: DevicePublicKeys,
                     localID: UUID, body: VerifiedReplicaSettingsBody) {
        self.envelope = envelope
        self.host = host
        self.localID = localID
        self.body = body
    }

    var metadata: ReplicaSettingsReplyMetadata {
        switch body {
        case .access(let reply):
            let generation: RemoteSettingsGeneration?
            switch reply.body {
            case .summary(let summary): generation = summary.generation
            case .grantUnavailable: generation = nil
            }
            return ReplicaSettingsReplyMetadata(header: envelope.header, inReplyTo: reply.inReplyTo,
                                                correlation: .query(queryID: reply.queryID), generation: generation)
        case .bind(let reply):
            return ReplicaSettingsReplyMetadata(header: envelope.header, inReplyTo: reply.inReplyTo,
                                                correlation: .bind(bindingRequestID: reply.bindingRequestID),
                                                generation: reply.parent.generation)
        }
    }

    func matchesVerifiedPeer(_ host: DevicePublicKeys, localID: UUID) -> Bool {
        self.host == host && self.localID == localID
    }

    func commitMetadata(into store: ReplicaStore) async throws -> CommittedReplicaSettingsReply {
        let fresh = try await store.rejectAuthenticated(envelope, host: host, localID: localID)
        return CommittedReplicaSettingsReply(verified: self, disposition: fresh ? .fresh : .duplicate)
    }

    var description: String { "VerifiedReplicaSettingsReply([redacted])" }
    var debugDescription: String { description }
}

final class CommittedReplicaSettingsReply: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    private let verified: VerifiedReplicaSettingsReply
    let disposition: ReplicaSettingsMetadataDisposition

    fileprivate init(verified: VerifiedReplicaSettingsReply, disposition: ReplicaSettingsMetadataDisposition) {
        self.verified = verified
        self.disposition = disposition
    }

    var metadata: ReplicaSettingsReplyMetadata { verified.metadata }

    func matchesVerifiedPeer(_ host: DevicePublicKeys, localID: UUID) -> Bool {
        verified.matchesVerifiedPeer(host, localID: localID)
    }

    var description: String { "CommittedReplicaSettingsReply([redacted])" }
    var debugDescription: String { description }
}

protocol DurableEnvelopeInbox: Sendable {
    func acceptAuthenticated(_ envelope: EncryptedEnvelope, plaintext: Data, host: DevicePublicKeys, localID: UUID) async throws -> Bool
    func rejectAuthenticated(_ envelope: EncryptedEnvelope, host: DevicePublicKeys, localID: UUID) async throws -> Bool
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
        return try seal(plaintext, recipient: recipient, header: header)
    }

    private func seal(_ plaintext: Data, recipient: DevicePublicKeys, header: EnvelopeHeader) throws -> EncryptedEnvelope {
        let sealed = try ChaChaPoly.seal(plaintext, using: key(peer: recipient, header: header), nonce: ChaChaPoly.Nonce(data: header.nonce), authenticating: header.canonical)
        let bytes = sealed.ciphertext + sealed.tag
        let unsigned = EncryptedEnvelope(header: header, ciphertext: bytes, signature: Data())
        return EncryptedEnvelope(header: header, ciphertext: bytes, signature: try sign(unsigned.signedBytes))
    }

    func sealSettings(_ request: RemoteSettingsAccessRequest, recipient: DevicePublicKeys) throws -> EncryptedEnvelope {
        let context = try settingsSealContext(hostID: request.hostID, time: request.time, recipient: recipient)
        return try sealSettings(request.encoded(), context: context)
    }

    func sealSettings(_ request: RemoteSettingsBindRequest, recipient: DevicePublicKeys) throws -> EncryptedEnvelope {
        let context = try settingsSealContext(hostID: request.hostID, time: request.time, recipient: recipient)
        return try sealSettings(request.encoded(), context: context)
    }

    private struct SettingsSealContext: CustomStringConvertible, CustomDebugStringConvertible {
        let recipient: DevicePublicKeys
        let header: EnvelopeHeader
        let deadline: ContinuousClock.Instant

        var description: String { "SettingsSealContext([redacted])" }
        var debugDescription: String { description }
    }

    private func settingsSealContext(hostID: UUID, time: RemoteSettingsRequestTime,
                                     recipient: DevicePublicKeys) throws -> SettingsSealContext {
        let started = ContinuousClock.now
        let created = try WireID.now()
        let recipient = try DevicePublicKeys(deviceID: recipient.deviceID, signingKey: recipient.signingKey,
                                             encryptionKey: recipient.encryptionKey)
        guard hostID == recipient.deviceID, recipient.deviceID != publicKeys.deviceID else { throw WireError.identity }
        guard time.issuedAt <= created, created < time.expiresAt else { throw WireError.expired }
        let lifetime = time.expiresAt - created
        guard lifetime <= 30_000 else { throw WireError.expired }
        let deadline = started.advanced(by: .milliseconds(Int64(lifetime)))
        let header = try EnvelopeHeader(sender: publicKeys.deviceID, recipient: recipient.deviceID,
                                        created: created, lifetime: lifetime)
        return SettingsSealContext(recipient: recipient, header: header, deadline: deadline)
    }

    private func sealSettings(_ plaintext: Data, context: SettingsSealContext) throws -> EncryptedEnvelope {
        guard !plaintext.isEmpty, plaintext.count <= 32_768 else { throw WireError.size }
        let envelope = try seal(plaintext, recipient: context.recipient, header: context.header)
        _ = try envelope.wire.encoded(maximum: 49_152)
        _ = try WireValue.object([
            "type": .string("send"), "schema_version": .unsigned(1), "envelope": envelope.wire
        ]).encoded(maximum: 364_544)
        let observed = try WireID.now()
        guard observed >= context.header.created, observed < context.header.expires,
              ContinuousClock.now < context.deadline else { throw WireError.expired }
        return envelope
    }

    private func authenticatedPlaintext(_ envelope: EncryptedEnvelope, host: DevicePublicKeys, now: UInt64) throws -> Data {
        guard envelope.header.sender == host.deviceID, envelope.header.recipient == publicKeys.deviceID,
              host.deviceID != publicKeys.deviceID else { throw WireError.identity }
        try envelope.header.validate(now: now)
        try host.verify(envelope.signature, message: envelope.signedBytes)
        let ciphertext = envelope.ciphertext.dropLast(16)
        let tag = envelope.ciphertext.suffix(16)
        let box = try ChaChaPoly.SealedBox(nonce: ChaChaPoly.Nonce(data: envelope.header.nonce), ciphertext: ciphertext, tag: tag)
        let plaintext = try ChaChaPoly.open(box, using: key(peer: host, header: envelope.header), authenticating: envelope.header.canonical)
        guard !plaintext.isEmpty, plaintext.count <= 1_048_576 else { throw WireError.size }
        return plaintext
    }

    func verifySettingsAccessReply(_ envelope: EncryptedEnvelope, host: DevicePublicKeys) throws -> VerifiedReplicaSettingsReply {
        let plaintext = try authenticatedPlaintext(envelope, host: host, now: WireID.now())
        let reply = try RemoteSettingsWire.decodeAccessReply(plaintext)
        guard reply.hostID == host.deviceID else { throw WireError.identity }
        let maximum: Int
        switch reply.body {
        case .summary: maximum = 49_152
        case .grantUnavailable: maximum = 4096
        }
        try validateSettingsReplyEnvelope(envelope, maximum: maximum)
        return VerifiedReplicaSettingsReply(envelope: envelope, host: host,
                                            localID: publicKeys.deviceID, body: .access(reply))
    }

    func verifySettingsBindReply(_ envelope: EncryptedEnvelope, host: DevicePublicKeys) throws -> VerifiedReplicaSettingsReply {
        let plaintext = try authenticatedPlaintext(envelope, host: host, now: WireID.now())
        let reply = try RemoteSettingsWire.decodeBindReply(plaintext)
        guard reply.hostID == host.deviceID else { throw WireError.identity }
        try validateSettingsReplyEnvelope(envelope, maximum: 49_152)
        return VerifiedReplicaSettingsReply(envelope: envelope, host: host,
                                            localID: publicKeys.deviceID, body: .bind(reply))
    }

    private func validateSettingsReplyEnvelope(_ envelope: EncryptedEnvelope, maximum: Int) throws {
        guard envelope.header.expires > envelope.header.created,
              envelope.header.expires - envelope.header.created <= 60_000 else { throw WireError.invalid }
        _ = try envelope.wire.encoded(maximum: maximum)
    }

    func receive(_ envelope: EncryptedEnvelope, host: DevicePublicKeys, into inbox: any DurableEnvelopeInbox, now: UInt64) async throws -> Bool {
        let plaintext = try authenticatedPlaintext(envelope, host: host, now: now)
        switch DevicePackets.routing(plaintext) {
        case .ordinary:
            return try await inbox.acceptAuthenticated(envelope, plaintext: plaintext, host: host, localID: publicKeys.deviceID)
        case .rejected:
            return try await inbox.rejectAuthenticated(envelope, host: host, localID: publicKeys.deviceID)
        }
    }
}

extension WirePermissionInspection {
    func verifyArguments() throws {
        let digest = SHA256.hash(data: Data(argumentsJSON.utf8)).map { String(format: "%02x", $0) }.joined()
        guard digest == argumentsDigest else { throw WireError.signature }
    }
}

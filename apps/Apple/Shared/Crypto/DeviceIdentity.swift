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
    case command(requestID: UUID)

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
    case snapshot(RemoteSettingsReadReply)
    case inspection(RemoteSettingsReadReply)
    case update(RemoteSettingsUpdateReply)

    var description: String { "VerifiedReplicaSettingsBody([redacted])" }
    var debugDescription: String { description }
}

struct ReplicaSettingsAccessDisplaySummary: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    let readableKeys: [SettingKey]
    let writableKeys: [SettingKey]
    let sessionConsent: RemoteSettingsConsent
    let configuredSettingsRevision: UInt64?

    fileprivate init(readableKeys: [SettingKey], writableKeys: [SettingKey],
                     sessionConsent: RemoteSettingsConsent, configuredSettingsRevision: UInt64?) {
        self.readableKeys = readableKeys
        self.writableKeys = writableKeys
        self.sessionConsent = sessionConsent
        self.configuredSettingsRevision = configuredSettingsRevision
    }

    var description: String { "ReplicaSettingsAccessDisplaySummary([redacted])" }
    var debugDescription: String { description }
}

enum ReplicaSettingsAccessDisplayStatus: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    case available(ReplicaSettingsAccessDisplaySummary)
    case grantUnavailable

    var description: String { "ReplicaSettingsAccessDisplayStatus([redacted])" }
    var debugDescription: String { description }
}

struct ReplicaSettingsAccessDisplay: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    let status: ReplicaSettingsAccessDisplayStatus

    fileprivate init(status: ReplicaSettingsAccessDisplayStatus) { self.status = status }

    var description: String { "ReplicaSettingsAccessDisplay([redacted])" }
    var debugDescription: String { description }
}

final class ReplicaSettingsAccessProjection: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    let display: ReplicaSettingsAccessDisplay
    let grantExpiresAt: UInt64?
    private let generation: RemoteSettingsGeneration?

    fileprivate init(display: ReplicaSettingsAccessDisplay, grantExpiresAt: UInt64?,
                     generation: RemoteSettingsGeneration?) {
        self.display = display
        self.grantExpiresAt = grantExpiresAt
        self.generation = generation
    }

    func prepareBindRequest(owner: isolated ReplicaService) throws -> RemoteSettingsBindRequest {
        let input = try owner.captureSettingsBindRequestInput(self)
        guard let generation, let grantExpiresAt, grantExpiresAt == input.grantExpiresAt,
              input.requestedParentExpiresAt <= grantExpiresAt else { throw WireError.unavailable }
        guard case .available = display.status else { throw WireError.unavailable }
        return try RemoteSettingsBindRequest(hostID: input.hostID, generation: generation,
                                             bindingRequestID: input.bindingRequestID, time: input.time,
                                             requestedParentExpiresAt: input.requestedParentExpiresAt)
    }

    var description: String { "ReplicaSettingsAccessProjection([redacted])" }
    var debugDescription: String { description }
}

final class ReplicaSettingsBoundParentProjection: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    private let parent: RemoteSettingsParent
    let expiresAt: UInt64

    fileprivate init(parent: RemoteSettingsParent, expiresAt: UInt64) {
        self.parent = parent
        self.expiresAt = expiresAt
    }

    func prepareSnapshotRequest(owner: isolated ReplicaService) throws -> RemoteSettingsSnapshotReadRequest {
        let input = try owner.captureSettingsSnapshotRequestInput(self)
        return try RemoteSettingsSnapshotReadRequest(hostID: input.hostID, queryID: input.queryID,
                                                     parent: parent, keys: input.keys, time: input.time)
    }

    func prepareInspectionRequest(owner: isolated ReplicaService) throws -> RemoteSettingsOriginalInspectionRequest {
        let input = try owner.captureSettingsInspectionRequestInput(self)
        return try RemoteSettingsOriginalInspectionRequest(hostID: input.hostID, queryID: input.queryID,
                            parent: parent, requestID: input.requestID, operationID: input.operationID, time: input.time)
    }

    func prepareUpdateRequest(owner: isolated ReplicaService) throws -> RemoteSettingsUpdateRequest {
        let input = try owner.captureSettingsUpdateRequestInput(self)
        return try RemoteSettingsUpdateRequest(hostID: input.hostID, parent: parent, requestID: input.requestID,
                                               time: input.time, expectedSettingsRevision: input.expectedSettingsRevision,
                                               enabled: input.enabled, dimensions: input.dimensions)
    }

    var description: String { "ReplicaSettingsBoundParentProjection([redacted])" }
    var debugDescription: String { description }
}

enum ReplicaSettingsSnapshotDisplay: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    case values(RemoteSettingsHostSnapshot)
    case denied(RemoteSettingsReadDenial)

    var description: String { "ReplicaSettingsSnapshotDisplay([redacted])" }
    var debugDescription: String { description }
}

final class ReplicaSettingsSnapshotProjection: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    let display: ReplicaSettingsSnapshotDisplay

    fileprivate init(display: ReplicaSettingsSnapshotDisplay) { self.display = display }

    var description: String { "ReplicaSettingsSnapshotProjection([redacted])" }
    var debugDescription: String { description }
}

enum ReplicaSettingsInspectionLookup: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    case notRecorded
    case recorded(RemoteSettingsOriginalView)

    var description: String { "ReplicaSettingsInspectionLookup([redacted])" }
    var debugDescription: String { description }
}

enum ReplicaSettingsInspectionDisplay: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    case observation(requestID: UUID, operationID: UUID?, observedAt: UInt64, lookup: ReplicaSettingsInspectionLookup)
    case denied(RemoteSettingsReadDenial)

    var description: String { "ReplicaSettingsInspectionDisplay([redacted])" }
    var debugDescription: String { description }
}

final class ReplicaSettingsInspectionProjection: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    let display: ReplicaSettingsInspectionDisplay

    fileprivate init(display: ReplicaSettingsInspectionDisplay) { self.display = display }

    var description: String { "ReplicaSettingsInspectionProjection([redacted])" }
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
        case .snapshot(let reply):
            return ReplicaSettingsReplyMetadata(header: envelope.header, inReplyTo: reply.inReplyTo,
                                                correlation: .query(queryID: reply.queryID), generation: reply.generation)
        case .inspection(let reply):
            return ReplicaSettingsReplyMetadata(header: envelope.header, inReplyTo: reply.inReplyTo,
                                                correlation: .query(queryID: reply.queryID), generation: reply.generation)
        case .update(let reply):
            return ReplicaSettingsReplyMetadata(header: envelope.header, inReplyTo: reply.inReplyTo,
                                                correlation: .command(requestID: reply.requestID), generation: reply.generation)
        }
    }

    func matchesVerifiedPeer(_ host: DevicePublicKeys, localID: UUID) -> Bool {
        self.host == host && self.localID == localID
    }

    func commitMetadata(into store: ReplicaStore) async throws -> CommittedReplicaSettingsReply {
        let fresh = try await store.rejectAuthenticated(envelope, host: host, localID: localID)
        return CommittedReplicaSettingsReply(verified: self, disposition: fresh ? .fresh : .duplicate)
    }

    fileprivate func projectAccessDisplay(committed: CommittedReplicaSettingsReply,
                                          qualification: ReplicaSettingsAccessQualification,
                                          owner: isolated ReplicaService) throws -> ReplicaSettingsAccessProjection {
        guard committed.matchesVerifiedObject(self) else { throw WireError.identity }
        try owner.checkSettingsAccessDisplayProjection(qualification, committed: committed)
        let now = try WireID.now()
        try envelope.header.validate(now: now)
        guard envelope.header.expires > envelope.header.created,
              envelope.header.expires - envelope.header.created <= 60_000 else { throw WireError.invalid }
        guard case .access(let reply) = body else { throw WireError.invalid }
        switch reply.body {
        case .summary(let summary):
            guard summary.expiresAt > now, envelope.header.expires <= summary.expiresAt else {
                throw WireError.expired
            }
        case .grantUnavailable: break
        }
        try qualification.consumeForAccessProjection(for: committed, owner: owner)
        switch reply.body {
        case .summary(let summary):
            let displayed = ReplicaSettingsAccessDisplaySummary(
                readableKeys: summary.readableKeys, writableKeys: summary.writableKeys,
                sessionConsent: summary.sessionConsent, configuredSettingsRevision: summary.configuredSettingsRevision
            )
            return ReplicaSettingsAccessProjection(display: ReplicaSettingsAccessDisplay(status: .available(displayed)),
                                                   grantExpiresAt: summary.expiresAt, generation: summary.generation)
        case .grantUnavailable:
            return ReplicaSettingsAccessProjection(display: ReplicaSettingsAccessDisplay(status: .grantUnavailable),
                                                   grantExpiresAt: nil, generation: nil)
        }
    }

    fileprivate func projectBoundParent(committed: CommittedReplicaSettingsReply,
                                         qualification: ReplicaSettingsBindQualification,
                                         owner: isolated ReplicaService) throws -> ReplicaSettingsBoundParentProjection {
        guard committed.matchesVerifiedObject(self) else { throw WireError.identity }
        let input = try owner.checkSettingsBindProjection(qualification, committed: committed)
        let now = try WireID.now()
        try envelope.header.validate(now: now)
        guard envelope.header.expires > envelope.header.created,
              envelope.header.expires - envelope.header.created <= 60_000,
              envelope.header.created >= input.wallFloor else { throw WireError.invalid }
        guard case .bind(let reply) = body,
              reply.hostID == input.request.hostID,
              reply.inReplyTo == input.requestMessageID,
              reply.bindingRequestID == input.request.bindingRequestID,
              reply.parent.generation == input.request.generation else { throw WireError.identity }
        guard reply.expiresAt > now, reply.expiresAt > input.wallFloor,
              reply.expiresAt <= input.request.requestedParentExpiresAt,
              reply.expiresAt <= input.grantExpiresAt,
              envelope.header.expires <= reply.expiresAt else { throw WireError.expired }
        try qualification.consumeForBoundParent(for: committed, owner: owner)
        return ReplicaSettingsBoundParentProjection(parent: reply.parent, expiresAt: reply.expiresAt)
    }

    fileprivate func projectSnapshotDisplay(committed: CommittedReplicaSettingsReply,
                                             qualification: ReplicaSettingsSnapshotQualification,
                                             owner: isolated ReplicaService) throws -> ReplicaSettingsSnapshotProjection {
        guard committed.matchesVerifiedObject(self) else { throw WireError.identity }
        try owner.checkSettingsSnapshotDisplayProjection(qualification, committed: committed)
        try envelope.header.validate(now: WireID.now())
        guard envelope.header.expires > envelope.header.created,
              envelope.header.expires - envelope.header.created <= 60_000,
              case .snapshot(let reply) = body else { throw WireError.invalid }
        let display: ReplicaSettingsSnapshotDisplay
        switch reply.body {
        case .snapshot(let snapshot): display = .values(snapshot)
        case .denied(let reason): display = .denied(reason)
        case .originalInspection: throw WireError.invalid
        }
        try qualification.consumeForSnapshotProjection(for: committed, owner: owner)
        return ReplicaSettingsSnapshotProjection(display: display)
    }

    fileprivate func projectInspectionDisplay(committed: CommittedReplicaSettingsReply,
                                             qualification: ReplicaSettingsInspectionQualification,
                                             owner: isolated ReplicaService) throws -> ReplicaSettingsInspectionProjection {
        guard committed.matchesVerifiedObject(self) else { throw WireError.identity }
        try owner.checkSettingsInspectionDisplayProjection(qualification, committed: committed)
        try envelope.header.validate(now: WireID.now())
        guard envelope.header.expires > envelope.header.created,
              envelope.header.expires - envelope.header.created <= 60_000,
              case .inspection(let reply) = body else { throw WireError.invalid }
        let display: ReplicaSettingsInspectionDisplay
        switch reply.body {
        case .originalInspection(let requestID, let operationID, let observedAt, let lookup):
            let displayed: ReplicaSettingsInspectionLookup
            switch lookup {
            case .notRecorded: displayed = .notRecorded
            case .recorded(_, let view): displayed = .recorded(view)
            }
            display = .observation(requestID: requestID, operationID: operationID, observedAt: observedAt, lookup: displayed)
        case .denied(let reason): display = .denied(reason)
        case .snapshot: throw WireError.invalid
        }
        try qualification.consumeForInspectionProjection(for: committed, owner: owner)
        return ReplicaSettingsInspectionProjection(display: display)
    }

    fileprivate func projectUpdateOutcome(committed: CommittedReplicaSettingsReply,
                                          qualification: ReplicaSettingsUpdateQualification,
                                          owner: isolated ReplicaService) throws -> RemoteSettingsOriginalView {
        guard committed.matchesVerifiedObject(self) else { throw WireError.identity }
        try owner.checkSettingsUpdateProjection(qualification, committed: committed)
        try envelope.header.validate(now: WireID.now())
        guard envelope.header.expires > envelope.header.created,
              envelope.header.expires - envelope.header.created <= 60_000,
              case .update(let reply) = body else { throw WireError.invalid }
        try qualification.consumeForUpdateProjection(for: committed, owner: owner)
        return reply.view
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

    fileprivate func matchesVerifiedObject(_ candidate: VerifiedReplicaSettingsReply) -> Bool {
        verified === candidate
    }

    func projectAccessDisplay(qualifiedBy qualification: ReplicaSettingsAccessQualification,
                              owner: isolated ReplicaService) throws -> ReplicaSettingsAccessProjection {
        try qualification.checkCurrent(for: self, owner: owner)
        return try verified.projectAccessDisplay(committed: self, qualification: qualification, owner: owner)
    }

    func projectBoundParent(qualifiedBy qualification: ReplicaSettingsBindQualification,
                            owner: isolated ReplicaService) throws -> ReplicaSettingsBoundParentProjection {
        try qualification.checkCurrent(for: self, owner: owner)
        return try verified.projectBoundParent(committed: self, qualification: qualification, owner: owner)
    }

    func projectSnapshotDisplay(qualifiedBy qualification: ReplicaSettingsSnapshotQualification,
                                owner: isolated ReplicaService) throws -> ReplicaSettingsSnapshotProjection {
        try qualification.checkCurrent(for: self, owner: owner)
        return try verified.projectSnapshotDisplay(committed: self, qualification: qualification, owner: owner)
    }

    func projectInspectionDisplay(qualifiedBy qualification: ReplicaSettingsInspectionQualification,
                                owner: isolated ReplicaService) throws -> ReplicaSettingsInspectionProjection {
        try qualification.checkCurrent(for: self, owner: owner)
        return try verified.projectInspectionDisplay(committed: self, qualification: qualification, owner: owner)
    }

    func projectUpdateOutcome(qualifiedBy qualification: ReplicaSettingsUpdateQualification,
                              owner: isolated ReplicaService) throws -> RemoteSettingsOriginalView {
        try qualification.checkCurrent(for: self, owner: owner)
        return try verified.projectUpdateOutcome(committed: self, qualification: qualification, owner: owner)
    }

    var description: String { "CommittedReplicaSettingsReply([redacted])" }
    var debugDescription: String { description }
}

enum ReplicaDeliveryDisposition: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    case fresh
    case duplicate

    var description: String { "ReplicaDeliveryDisposition([redacted])" }
    var debugDescription: String { description }
}

enum ReplicaClassifiedDeliveryResult: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    case ordinary(ReplicaDeliveryDisposition)
    case settingsAccess(VerifiedReplicaSettingsReply)
    case settingsBind(VerifiedReplicaSettingsReply)
    case settingsSnapshot(VerifiedReplicaSettingsReply)
    case settingsInspection(VerifiedReplicaSettingsReply)
    case settingsUpdate(VerifiedReplicaSettingsReply)
    case refused(ReplicaDeliveryDisposition)

    var description: String { "ReplicaClassifiedDeliveryResult([redacted])" }
    var debugDescription: String { description }
}

final class ReplicaClassifiedDelivery: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    private let received: ReceivedRelayFrame
    private let value: ReplicaClassifiedDeliveryResult

    fileprivate init(received: ReceivedRelayFrame, result: ReplicaClassifiedDeliveryResult) {
        self.received = received
        value = result
    }

    var result: ReplicaClassifiedDeliveryResult { value }

    func matchesReceivedFrame(_ received: ReceivedRelayFrame) -> Bool {
        guard self.received.originalByteCount == received.originalByteCount,
              case .delivery(let original) = self.received.frame,
              case .delivery(let candidate) = received.frame else { return false }
        return original.header.canonical == candidate.header.canonical
            && original.ciphertext == candidate.ciphertext && original.signature == candidate.signature
    }

    var description: String { "ReplicaClassifiedDelivery([redacted])" }
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

    func sealSettings(_ request: RemoteSettingsSnapshotReadRequest, recipient: DevicePublicKeys) throws -> EncryptedEnvelope {
        let context = try settingsSealContext(hostID: request.hostID, time: request.time, recipient: recipient)
        return try sealSettings(request.encoded(), context: context)
    }

    func sealSettings(_ request: RemoteSettingsOriginalInspectionRequest, recipient: DevicePublicKeys) throws -> EncryptedEnvelope {
        let context = try settingsSealContext(hostID: request.hostID, time: request.time, recipient: recipient)
        return try sealSettings(request.encoded(), context: context)
    }

    func sealSettings(_ request: RemoteSettingsUpdateRequest, recipient: DevicePublicKeys) throws -> EncryptedEnvelope {
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
        return try verifiedSettingsAccessReply(plaintext, envelope: envelope, host: host)
    }

    private func verifiedSettingsAccessReply(_ plaintext: Data, envelope: EncryptedEnvelope,
                                             host: DevicePublicKeys) throws -> VerifiedReplicaSettingsReply {
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
        return try verifiedSettingsBindReply(plaintext, envelope: envelope, host: host)
    }

    private func verifiedSettingsBindReply(_ plaintext: Data, envelope: EncryptedEnvelope,
                                           host: DevicePublicKeys) throws -> VerifiedReplicaSettingsReply {
        let reply = try RemoteSettingsWire.decodeBindReply(plaintext)
        guard reply.hostID == host.deviceID else { throw WireError.identity }
        try validateSettingsReplyEnvelope(envelope, maximum: 49_152)
        return VerifiedReplicaSettingsReply(envelope: envelope, host: host,
                                            localID: publicKeys.deviceID, body: .bind(reply))
    }

    private func verifiedSettingsSnapshotReply(_ plaintext: Data, envelope: EncryptedEnvelope,
                                               host: DevicePublicKeys,
                                               request: RemoteSettingsSnapshotReadRequest) throws -> VerifiedReplicaSettingsReply {
        let reply = try RemoteSettingsReadWire.decodeReply(plaintext, matching: request)
        guard reply.hostID == host.deviceID else { throw WireError.identity }
        let maximum: Int
        switch reply.body {
        case .snapshot: maximum = 49_152
        case .denied: maximum = 4096
        case .originalInspection: throw WireError.invalid
        }
        try validateSettingsReplyEnvelope(envelope, maximum: maximum)
        return VerifiedReplicaSettingsReply(envelope: envelope, host: host,
                                            localID: publicKeys.deviceID, body: .snapshot(reply))
    }

    private func verifiedSettingsInspectionReply(_ plaintext: Data, envelope: EncryptedEnvelope,
                                               host: DevicePublicKeys,
                                               request: RemoteSettingsOriginalInspectionRequest) throws -> VerifiedReplicaSettingsReply {
        let reply = try RemoteSettingsReadWire.decodeReply(plaintext, matching: request)
        guard reply.hostID == host.deviceID else { throw WireError.identity }
        let maximum: Int
        switch reply.body {
        case .originalInspection: maximum = 90_112
        case .denied: maximum = 4096
        case .snapshot: throw WireError.invalid
        }
        try validateSettingsReplyEnvelope(envelope, maximum: maximum)
        return VerifiedReplicaSettingsReply(envelope: envelope, host: host,
                                            localID: publicKeys.deviceID, body: .inspection(reply))
    }

    private func verifiedSettingsUpdateReply(_ plaintext: Data, envelope: EncryptedEnvelope,
                                             host: DevicePublicKeys,
                                             original: ReplicaSettingsUpdateReplyExpectation) throws -> VerifiedReplicaSettingsReply {
        let reply = try RemoteSettingsUpdateWire.decodeReply(plaintext, matching: original.request,
                                                             encryptedRequestID: original.encryptedRequestID)
        guard reply.hostID == host.deviceID else { throw WireError.identity }
        try validateSettingsReplyEnvelope(envelope, maximum: 90_112)
        return VerifiedReplicaSettingsReply(envelope: envelope, host: host,
                                            localID: publicKeys.deviceID, body: .update(reply))
    }

    private func validateSettingsReplyEnvelope(_ envelope: EncryptedEnvelope, maximum: Int) throws {
        guard envelope.header.expires > envelope.header.created,
              envelope.header.expires - envelope.header.created <= 60_000 else { throw WireError.invalid }
        _ = try envelope.wire.encoded(maximum: maximum)
    }

    func receive(_ envelope: EncryptedEnvelope, host: DevicePublicKeys, into inbox: any DurableEnvelopeInbox, now: UInt64) async throws -> Bool {
        let packet = try authenticatedPacket(envelope, host: host, now: now)
        switch packet.routing {
        case .ordinary:
            return try await inbox.acceptAuthenticated(envelope, plaintext: packet.plaintext, host: host, localID: publicKeys.deviceID)
        case .settings, .rejected:
            return try await inbox.rejectAuthenticated(envelope, host: host, localID: publicKeys.deviceID)
        }
    }

    func receiveClassified(_ received: ReceivedRelayFrame, host: DevicePublicKeys,
                           into store: ReplicaStore) async throws -> ReplicaClassifiedDelivery {
        try await receiveClassified(received, host: host, into: store, expectation: .accessOrBind)
    }

    func receiveClassified(_ received: ReceivedRelayFrame, host: DevicePublicKeys,
                           into store: ReplicaStore,
                           matchingSnapshot request: RemoteSettingsSnapshotReadRequest?) async throws -> ReplicaClassifiedDelivery {
        try await receiveClassified(received, host: host, into: store, expectation: .snapshot(request))
    }

    func receiveClassified(_ received: ReceivedRelayFrame, host: DevicePublicKeys,
                           into store: ReplicaStore,
                           matchingInspection request: RemoteSettingsOriginalInspectionRequest?) async throws -> ReplicaClassifiedDelivery {
        try await receiveClassified(received, host: host, into: store, expectation: .inspection(request))
    }

    func receiveClassified(_ received: ReceivedRelayFrame, host: DevicePublicKeys,
                           into store: ReplicaStore,
                           matchingUpdate original: ReplicaSettingsUpdateReplyExpectation?) async throws -> ReplicaClassifiedDelivery {
        try await receiveClassified(received, host: host, into: store, expectation: .update(original))
    }

    private enum SettingsReplyExpectation {
        case accessOrBind
        case snapshot(RemoteSettingsSnapshotReadRequest?)
        case inspection(RemoteSettingsOriginalInspectionRequest?)
        case update(ReplicaSettingsUpdateReplyExpectation?)

        var packetLimit: Int {
            switch self {
            case .accessOrBind, .snapshot: 32_768
            case .inspection, .update: 65_536
            }
        }
    }

    private func receiveClassified(_ received: ReceivedRelayFrame, host: DevicePublicKeys,
                                   into store: ReplicaStore,
                                   expectation: SettingsReplyExpectation) async throws -> ReplicaClassifiedDelivery {
        guard case .delivery(let envelope) = received.frame else { throw WireError.invalid }
        let packet = try authenticatedPacket(envelope, host: host, now: WireID.now())
        let result: ReplicaClassifiedDeliveryResult
        switch packet.routing {
        case .ordinary:
            let fresh = try await store.acceptAuthenticated(envelope, plaintext: packet.plaintext, host: host, localID: publicKeys.deviceID)
            result = .ordinary(fresh ? .fresh : .duplicate)
        case .settings:
            guard received.originalByteCount <= 364_544, packet.plaintext.count <= expectation.packetLimit else { throw WireError.size }
            if let reply = try classifiedSettingsReply(packet.plaintext, envelope: envelope, host: host, expectation: expectation) {
                result = reply
            } else {
                let fresh = try await store.rejectAuthenticated(envelope, host: host, localID: publicKeys.deviceID)
                result = .refused(fresh ? .fresh : .duplicate)
            }
        case .rejected:
            let fresh = try await store.rejectAuthenticated(envelope, host: host, localID: publicKeys.deviceID)
            result = .refused(fresh ? .fresh : .duplicate)
        }
        return ReplicaClassifiedDelivery(received: received, result: result)
    }

    private struct AuthenticatedReplicaPacket: CustomStringConvertible, CustomDebugStringConvertible {
        let plaintext: Data
        let routing: DevicePackets.Routing

        var description: String { "AuthenticatedReplicaPacket([redacted])" }
        var debugDescription: String { description }
    }

    private func authenticatedPacket(_ envelope: EncryptedEnvelope, host: DevicePublicKeys,
                                     now: UInt64) throws -> AuthenticatedReplicaPacket {
        let plaintext = try authenticatedPlaintext(envelope, host: host, now: now)
        return AuthenticatedReplicaPacket(plaintext: plaintext, routing: DevicePackets.routing(plaintext))
    }

    private func classifiedSettingsReply(_ plaintext: Data, envelope: EncryptedEnvelope,
                                         host: DevicePublicKeys,
                                         expectation: SettingsReplyExpectation) throws -> ReplicaClassifiedDeliveryResult? {
        do {
            let packet = try WireValue.parse(plaintext, maximum: expectation.packetLimit)
            let family = try packet.field("payload").field("message").field("correlation").field("family").text
            if case .snapshot(let request) = expectation {
                guard family == "query", let request else { return nil }
                return .settingsSnapshot(try verifiedSettingsSnapshotReply(plaintext, envelope: envelope,
                                                                           host: host, request: request))
            }
            if case .inspection(let request) = expectation {
                guard family == "query", let request else { return nil }
                return .settingsInspection(try verifiedSettingsInspectionReply(plaintext, envelope: envelope,
                                                                           host: host, request: request))
            }
            if case .update(let original) = expectation {
                guard family == "command", let original else { return nil }
                return .settingsUpdate(try verifiedSettingsUpdateReply(plaintext, envelope: envelope,
                                                                       host: host, original: original))
            }
            switch family {
            case "query":
                return .settingsAccess(try verifiedSettingsAccessReply(plaintext, envelope: envelope, host: host))
            case "bind":
                return .settingsBind(try verifiedSettingsBindReply(plaintext, envelope: envelope, host: host))
            default: return nil
            }
        } catch let error as WireError where error == .invalid {
            return nil
        }
    }
}

extension WirePermissionInspection {
    func verifyArguments() throws {
        let digest = SHA256.hash(data: Data(argumentsJSON.utf8)).map { String(format: "%02x", $0) }.joined()
        guard digest == argumentsDigest else { throw WireError.signature }
    }
}

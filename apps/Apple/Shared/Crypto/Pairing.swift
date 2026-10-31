import CryptoKit
import Foundation

struct PairingInvitation: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    var description: String { "PairingInvitation([redacted])" }
    var debugDescription: String { description }

    let id: UUID
    let inviter: DevicePublicKeys
    let created: UInt64
    let expires: UInt64
    private let secret: Data
    private let signature: Data

    init(_ data: Data, now: UInt64) throws {
        let value = try WireValue.parse(data, maximum: 4_096)
        let fields = try value.object(keys: ["version", "pairing_id", "inviter", "created_at_ms", "expires_at_ms", "secret", "signature"], required: ["version", "pairing_id", "inviter", "created_at_ms", "expires_at_ms", "secret", "signature"])
        guard try fields.required("version").integer == 1 else { throw WireError.invalid }
        try self.init(id: fields.required("pairing_id").identifier, inviter: DevicePublicKeys(fields.required("inviter")),
                      created: fields.required("created_at_ms").integer, expires: fields.required("expires_at_ms").integer,
                      secret: fields.required("secret").bytes(count: 32), signature: fields.required("signature").bytes(count: 64), now: now)
    }

    init(compact data: Data, now: UInt64) throws {
        var reader = ByteReader(data)
        guard try reader.integer(UInt16.self) == 1 else { throw WireError.invalid }
        let id = try reader.identifier()
        let inviter = try DevicePublicKeys(deviceID: reader.identifier(), signingKey: reader.bytes(32), encryptionKey: reader.bytes(32))
        let created = try reader.integer(UInt64.self)
        let expires = try reader.integer(UInt64.self)
        let secret = try reader.bytes(32)
        let signature = try reader.bytes(64)
        guard reader.isAtEnd else { throw WireError.invalid }
        try self.init(id: id, inviter: inviter, created: created, expires: expires, secret: secret, signature: signature, now: now)
    }

    private init(id: UUID, inviter: DevicePublicKeys, created: UInt64, expires: UInt64, secret: Data, signature: Data, now: UInt64) throws {
        guard expires > now, expires > created, expires - created <= 600_000,
              created <= now + min(120_000, UInt64.max - now) else { throw WireError.expired }
        var canonical = Data.domain("everything.pairing.invitation.v1")
        canonical.appendBE(UInt16(1))
        canonical += WireID.bytes(id) + inviter.canonical
        canonical.appendBE(created)
        canonical.appendBE(expires)
        canonical += secret
        try inviter.verify(signature, message: canonical)
        self.id = id
        self.inviter = inviter
        self.created = created
        self.expires = expires
        self.secret = secret
        self.signature = signature
    }

    #if os(iOS)
    func request(identity: DeviceIdentity, confirmedFingerprint: String, now: UInt64) async throws -> (request: WireValue, proof: Data) {
        let responder = identity.publicKeys
        guard expires > now, confirmedFingerprint == inviter.fingerprint,
              inviter.deviceID != responder.deviceID else { throw WireError.pairing }
        var proofBytes = Data.domain("everything.pairing.request.v1")
        proofBytes.appendBE(UInt16(1))
        proofBytes += WireID.bytes(id) + WireID.bytes(inviter.deviceID) + responder.canonical
        proofBytes.appendBE(expires)
        let proof = Data(HMAC<SHA256>.authenticationCode(for: proofBytes, using: SymmetricKey(data: secret)))
        let signature = try await identity.sign(proofBytes + proof)
        return (.object(["version": .unsigned(1), "pairing_id": .id(id), "inviter": .id(inviter.deviceID), "responder": responder.wire, "expires_at_ms": .unsigned(expires), "proof": .bytes(proof), "signature": .bytes(signature)]), proof)
    }

    func code(responder: DevicePublicKeys, proof: Data) -> String {
        let info = Data.domain("vorra.pairing.code.v1") + inviter.canonical + responder.canonical + proof
        let key = HKDF<SHA256>.deriveKey(inputKeyMaterial: SymmetricKey(data: secret), salt: WireID.bytes(id), info: info, outputByteCount: 4)
        let value = key.withUnsafeBytes { bytes in bytes.reduce(UInt32(0)) { ($0 << 8) | UInt32($1) } }
        let code = (value & 0x07ff_ffff) % 100_000_000
        return String(format: "%04u %04u", code / 10_000, code % 10_000)
    }

    func nameBytes(responder: DevicePublicKeys, name: String) throws -> Data {
        let bytes = Data(name.utf8)
        guard !bytes.isEmpty, bytes.count <= 64, !name.unicodeScalars.contains(where: { CharacterSet.controlCharacters.contains($0) }) else { throw WireError.invalid }
        return Data.domain("vorra.pairing.name.v1") + WireID.bytes(id) + responder.canonical + Data([UInt8(bytes.count)]) + bytes
    }
    #endif

    func finish(_ data: Data, responder: DevicePublicKeys, now: UInt64) throws -> DevicePublicKeys {
        let fields = try WireValue.parse(data, maximum: 4_096).object(keys: ["version", "pairing_id", "inviter", "responder", "expires_at_ms", "signature"], required: ["version", "pairing_id", "inviter", "responder", "expires_at_ms", "signature"])
        guard try fields.required("version").integer == 1,
              try fields.required("pairing_id").identifier == id,
              try DevicePublicKeys(fields.required("inviter")) == inviter,
              try DevicePublicKeys(fields.required("responder")) == responder,
              try fields.required("expires_at_ms").integer == expires, expires > now else { throw WireError.pairing }
        var canonical = Data.domain("everything.pairing.acceptance.v1")
        canonical.appendBE(UInt16(1))
        canonical += WireID.bytes(id) + inviter.canonical + responder.canonical
        canonical.appendBE(expires)
        try inviter.verify(fields.required("signature").bytes(count: 64), message: canonical)
        return inviter
    }
}

#if os(iOS)
struct PairingLink: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    let invitation: PairingInvitation
    let origin: RelayOrigin
    let hostName: String

    var description: String { "PairingLink([redacted])" }
    var debugDescription: String { description }

    init(_ text: String, now: UInt64) throws {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard trimmed.utf8.count <= 2_048, let url = URLComponents(string: trimmed), let fragment = url.fragment,
              (url.scheme == "vorra" && url.host == "pair" && url.path.isEmpty)
                || (url.scheme == "https" && url.host == "vorra.yumi.to" && url.port == nil && url.path == "/pair"),
              url.query == nil, url.user == nil, url.password == nil else { throw PairingFailure.invalidLink }
        var base64 = fragment.replacingOccurrences(of: "-", with: "+").replacingOccurrences(of: "_", with: "/")
        base64 += String(repeating: "=", count: (4 - base64.count % 4) % 4)
        guard !fragment.contains("="), !fragment.contains("+"), !fragment.contains("/"),
              let payload = Data(base64Encoded: base64) else { throw PairingFailure.invalidLink }
        var reader = ByteReader(payload)
        do {
            guard try reader.integer(UInt8.self) == 1 else { throw PairingFailure.invalidLink }
            let compact = try reader.bytes(210)
            let originBytes = try reader.bytes(Int(reader.integer(UInt16.self)))
            let nameBytes = try reader.bytes(Int(reader.integer(UInt8.self)))
            guard reader.isAtEnd, let origin = String(data: originBytes, encoding: .utf8),
                  let name = String(data: nameBytes, encoding: .utf8), !name.isEmpty,
                  !name.unicodeScalars.contains(where: { CharacterSet.controlCharacters.contains($0) }) else { throw PairingFailure.invalidLink }
            self.origin = try RelayOrigin(origin)
            self.hostName = name
            do { invitation = try PairingInvitation(compact: compact, now: now) }
            catch WireError.expired { throw PairingFailure.expired }
        } catch let failure as PairingFailure {
            throw failure
        } catch {
            throw PairingFailure.invalidLink
        }
    }
}

enum PairingFailure: Error, LocalizedError, Sendable, Equatable {
    case invalidLink, expired, rejected, inUse, unreachable, unsupportedRelay, alreadyPaired(String), otherHost, notAllowed

    var errorDescription: String? {
        switch self {
        case .invalidLink: "This is not a Vorra pairing code. Run vorra pair on your Mac and scan the code it shows."
        case .expired: "This pairing code has expired or was already used. Run vorra pair on your Mac for a new one."
        case .rejected: "Pairing was rejected on the Mac."
        case .inUse: "Another device is already pairing with this code. Run vorra pair on your Mac for a new one."
        case .unreachable: "Your Mac could not be reached through its relay. Check that Vorra is running and connected, then try again."
        case .unsupportedRelay: "The relay does not support pairing yet. Update the relay and try again."
        case .alreadyPaired(let host): "This iPhone is already paired with \(host)."
        case .otherHost: "This iPhone is already paired with a different host. Disable this device in Settings before pairing it with another host."
        case .notAllowed: "The relay does not allow this host to pair new devices. Its operator must enroll the host with vorra relay enroll --pairing-host."
        }
    }
}
#endif

struct ByteReader {
    private let data: Data
    private var offset = 0

    init(_ data: Data) { self.data = Data(data) }

    var isAtEnd: Bool { offset == data.count }

    mutating func bytes(_ count: Int) throws -> Data {
        guard count >= 0, data.count - offset >= count else { throw WireError.invalid }
        defer { offset += count }
        return data.subdata(in: offset..<(offset + count))
    }

    mutating func integer<T: FixedWidthInteger & UnsignedInteger>(_: T.Type) throws -> T {
        try bytes(MemoryLayout<T>.size).reduce(T(0)) { ($0 << 8) | T($1) }
    }

    mutating func identifier() throws -> UUID {
        let raw = try bytes(16)
        let value = raw.withUnsafeBytes { UUID(uuid: $0.load(as: uuid_t.self)) }
        guard value != WireID.zero else { throw WireError.invalid }
        return value
    }
}

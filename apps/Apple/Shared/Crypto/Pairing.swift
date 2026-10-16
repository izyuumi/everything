import CryptoKit
import Foundation

struct PairingInvitation: Sendable {
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
        id = try fields.required("pairing_id").identifier
        inviter = try DevicePublicKeys(fields.required("inviter"))
        created = try fields.required("created_at_ms").integer
        expires = try fields.required("expires_at_ms").integer
        secret = try fields.required("secret").bytes(count: 32)
        signature = try fields.required("signature").bytes(count: 64)
        guard expires > now, expires > created, expires - created <= 600_000,
              created <= now + min(120_000, UInt64.max - now) else { throw WireError.expired }
        var canonical = Data.domain("everything.pairing.invitation.v1")
        canonical.appendBE(UInt16(1))
        canonical += WireID.bytes(id) + inviter.canonical
        canonical.appendBE(created)
        canonical.appendBE(expires)
        canonical += secret
        try inviter.verify(signature, message: canonical)
    }

    #if os(iOS)
    func request(identity: DeviceIdentity, confirmedFingerprint: String, now: UInt64) async throws -> WireValue {
        let responder = identity.publicKeys
        guard expires > now, confirmedFingerprint == inviter.fingerprint,
              inviter.deviceID != responder.deviceID else { throw WireError.pairing }
        var proofBytes = Data.domain("everything.pairing.request.v1")
        proofBytes.appendBE(UInt16(1))
        proofBytes += WireID.bytes(id) + WireID.bytes(inviter.deviceID) + responder.canonical
        proofBytes.appendBE(expires)
        let proof = Data(HMAC<SHA256>.authenticationCode(for: proofBytes, using: SymmetricKey(data: secret)))
        let signature = try await identity.sign(proofBytes + proof)
        return .object(["version": .unsigned(1), "pairing_id": .id(id), "inviter": .id(inviter.deviceID), "responder": responder.wire, "expires_at_ms": .unsigned(expires), "proof": .bytes(proof), "signature": .bytes(signature)])
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

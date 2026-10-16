import CryptoKit
import Foundation

struct DevicePublicKeys: Sendable, Equatable {
    let deviceID: UUID
    let signingKey: Data
    let encryptionKey: Data

    init(deviceID: UUID, signingKey: Data, encryptionKey: Data) throws {
        guard deviceID != WireID.zero, signingKey.count == 32, encryptionKey.count == 32,
              encryptionKey.contains(where: { $0 != 0 }) else { throw WireError.identity }
        _ = try Curve25519.Signing.PublicKey(rawRepresentation: signingKey)
        _ = try Curve25519.KeyAgreement.PublicKey(rawRepresentation: encryptionKey)
        self.deviceID = deviceID
        self.signingKey = signingKey
        self.encryptionKey = encryptionKey
    }

    init(_ value: WireValue) throws {
        let fields = try value.object(keys: ["device_id", "signing_key", "encryption_key"], required: ["device_id", "signing_key", "encryption_key"])
        try self.init(deviceID: fields.required("device_id").identifier, signingKey: fields.required("signing_key").bytes(count: 32), encryptionKey: fields.required("encryption_key").bytes(count: 32))
    }

    var wire: WireValue { .object(["device_id": .id(deviceID), "signing_key": .bytes(signingKey), "encryption_key": .bytes(encryptionKey)]) }
    var canonical: Data { WireID.bytes(deviceID) + signingKey + encryptionKey }
    var fingerprint: String {
        let bytes = Array(SHA256.hash(data: canonical))
        return stride(from: 0, to: bytes.count, by: 4).map { offset in
            bytes[offset..<(offset + 4)].map { String(format: "%02x", $0) }.joined()
        }.joined(separator: "-")
    }

    func verify(_ signature: Data, message: Data) throws {
        guard signature.count == 64,
              try Curve25519.Signing.PublicKey(rawRepresentation: signingKey).isValidSignature(signature, for: message) else { throw WireError.signature }
    }
}

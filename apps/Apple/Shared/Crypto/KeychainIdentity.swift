import CryptoKit
import Foundation
import Security

func secureRandom(_ count: Int) throws -> Data {
    var bytes = Data(count: count)
    let result = bytes.withUnsafeMutableBytes { buffer in
        guard let address = buffer.baseAddress else { return errSecParam }
        return SecRandomCopyBytes(kSecRandomDefault, count, address)
    }
    guard result == errSecSuccess else { throw WireError.unavailable }
    return bytes
}

enum KeychainIdentity {
    static func loadOrCreate(expectedID: UUID?) throws -> DeviceIdentity {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: "ai.everything.ios.identity",
            kSecAttrAccount as String: "identity-v1",
            kSecAttrSynchronizable as String: false,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne
        ]
        var result: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        if status == errSecSuccess {
            guard let bytes = result as? Data, bytes.count == 81, bytes[0] == 1 else { throw WireError.identity }
            let idBytes = Array(bytes[1..<17])
            let id = UUID(uuid: (idBytes[0], idBytes[1], idBytes[2], idBytes[3], idBytes[4], idBytes[5], idBytes[6], idBytes[7], idBytes[8], idBytes[9], idBytes[10], idBytes[11], idBytes[12], idBytes[13], idBytes[14], idBytes[15]))
            guard id != WireID.zero, expectedID == nil || expectedID == id else { throw WireError.identity }
            return try DeviceIdentity(deviceID: id, signingSeed: bytes.subdata(in: 17..<49), encryptionSecret: bytes.subdata(in: 49..<81))
        }
        guard status == errSecItemNotFound, expectedID == nil else { throw WireError.unavailable }
        let id = UUID()
        let signing = Curve25519.Signing.PrivateKey()
        let encryption = Curve25519.KeyAgreement.PrivateKey()
        var bytes = Data([1]) + WireID.bytes(id) + signing.rawRepresentation + encryption.rawRepresentation
        defer { bytes.resetBytes(in: 0..<bytes.count) }
        let insertion: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: "ai.everything.ios.identity",
            kSecAttrAccount as String: "identity-v1",
            kSecAttrSynchronizable as String: false,
            kSecAttrAccessible as String: kSecAttrAccessibleWhenUnlockedThisDeviceOnly,
            kSecValueData as String: bytes
        ]
        guard SecItemAdd(insertion as CFDictionary, nil) == errSecSuccess else { throw WireError.unavailable }
        return try DeviceIdentity(deviceID: id, signingSeed: signing.rawRepresentation, encryptionSecret: encryption.rawRepresentation)
    }
}

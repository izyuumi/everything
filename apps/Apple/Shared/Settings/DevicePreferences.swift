import Foundation

enum DevicePreferenceKey: String, Codable, CaseIterable, Sendable {
    case completion = "notifications.completion"
    case attention = "notifications.attention"
}

struct DevicePreferenceChange: Codable, Equatable, Sendable {
    let key: DevicePreferenceKey
    let value: Bool
}

struct DevicePreferencePatch: Equatable, Sendable {
    let requestID: UUID
    let deviceID: UUID
    let expectedRevision: UInt64
    let changes: [DevicePreferenceChange]

    func validate() throws {
        guard requestID != .zeroDevicePreferenceID, deviceID != .zeroDevicePreferenceID,
              expectedRevision > 0, expectedRevision <= UInt64(Int64.max),
              (1...DevicePreferenceKey.allCases.count).contains(changes.count),
              Set(changes.map(\.key)).count == changes.count else {
            throw DevicePreferenceError.invalidRequest
        }
    }

    func encodedIdentity() throws -> Data {
        try validate()
        let value = Identity(contractVersion: 1, requestID: requestID, scope: Scope(deviceID: deviceID), expectedRevision: expectedRevision, changes: changes)
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys]
        let bytes = try encoder.encode(value)
        guard bytes.count <= 16 * 1024 else { throw DevicePreferenceError.invalidRequest }
        return bytes
    }

    private struct Scope: Encodable {
        let kind = "device_preferences"
        let deviceID: UUID

        enum CodingKeys: String, CodingKey {
            case kind
            case deviceID = "device_id"
        }
    }

    private struct Identity: Encodable {
        let contractVersion: UInt16
        let requestID: UUID
        let scope: Scope
        let expectedRevision: UInt64
        let changes: [DevicePreferenceChange]

        enum CodingKeys: String, CodingKey {
            case contractVersion = "contract_version"
            case requestID = "request_id"
            case scope
            case expectedRevision = "expected_revision"
            case changes
        }
    }
}

struct DevicePreferenceSnapshot: Equatable, Sendable {
    let deviceID: UUID
    let revision: UInt64
    let completion: Bool
    let attention: Bool

    func value(for key: DevicePreferenceKey) -> Bool {
        switch key {
        case .completion: completion
        case .attention: attention
        }
    }
}

struct DevicePreferenceResult: Equatable, Sendable {
    let requestID: UUID
    let deviceID: UUID
    let outcome: Outcome

    enum Outcome: Equatable, Sendable {
        case applied(revision: UInt64)
        case conflict(observedRevision: UInt64)
    }
}

enum DevicePreferenceError: Error, LocalizedError, Sendable {
    case invalidRequest
    case wrongDevice
    case requestReused
    case capacity
    case storage
    case unsupportedVersion
    case uncertain

    var errorDescription: String? {
        switch self {
        case .invalidRequest: "The device preference request is invalid."
        case .wrongDevice: "These preferences belong to a different device."
        case .requestReused: "This request ID is already bound to a different change."
        case .capacity: "The local preference store has reached its capacity."
        case .storage: "The local preference store is unavailable."
        case .unsupportedVersion: "The local preference store uses an unsupported version."
        case .uncertain: "The original preference request must be inspected before retrying."
        }
    }
}

extension UUID {
    fileprivate static let zeroDevicePreferenceID = UUID(uuid: (0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0))
}

enum DeviceAppearance: String, Codable, CaseIterable, Sendable {
    case system
    case light
    case dark
}

struct DeviceAppearancePatch: Equatable, Sendable {
    let requestID: UUID
    let deviceID: UUID
    let expectedRevision: UInt64
    let appearance: DeviceAppearance

    func validate() throws {
        guard requestID != .zeroDevicePreferenceID, deviceID != .zeroDevicePreferenceID,
              expectedRevision > 0, expectedRevision <= UInt64(Int64.max) else {
            throw DevicePreferenceError.invalidRequest
        }
    }

    func encodedIdentity() throws -> Data {
        try validate()
        let value = Identity(contractVersion: 1, requestID: requestID, scope: Scope(deviceID: deviceID),
                             expectedRevision: expectedRevision, appearance: appearance)
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys]
        let bytes = try encoder.encode(value)
        guard bytes.count <= 16 * 1024 else { throw DevicePreferenceError.invalidRequest }
        return bytes
    }

    private struct Scope: Encodable {
        let kind = "device_preferences"
        let deviceID: UUID

        enum CodingKeys: String, CodingKey {
            case kind
            case deviceID = "device_id"
        }
    }

    private struct Identity: Encodable {
        let kind = "appearance"
        let contractVersion: UInt16
        let requestID: UUID
        let scope: Scope
        let expectedRevision: UInt64
        let appearance: DeviceAppearance

        enum CodingKeys: String, CodingKey {
            case kind
            case contractVersion = "contract_version"
            case requestID = "request_id"
            case scope
            case expectedRevision = "expected_revision"
            case appearance
        }
    }
}

struct DeviceAppearanceSnapshot: Equatable, Sendable {
    let deviceID: UUID
    let revision: UInt64
    let appearance: DeviceAppearance
}

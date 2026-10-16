import CoreFoundation
import Foundation

indirect enum WireValue: Sendable, Equatable {
    case object([String: WireValue])
    case array([WireValue])
    case string(String)
    case unsigned(UInt64)
    case signed(Int64)
    case number(Double)
    case bool(Bool)
    case null

    static func parse(_ data: Data, maximum: Int = 1_048_576) throws -> WireValue {
        guard !data.isEmpty, data.count <= maximum else { throw WireError.size }
        let parsed = try JSONSerialization.jsonObject(with: data, options: [.fragmentsAllowed])
        try uniqueObjectKeys(data)
        return try convert(parsed, depth: 0)
    }

    private final class ObjectKeys {
        var values = Set<String>()
        var expectsKey = true
    }

    private enum Container {
        case object(ObjectKeys)
        case array
    }

    private static func uniqueObjectKeys(_ data: Data) throws {
        let bytes = Array(data)
        var containers: [Container] = []
        var index = 0
        var keyCount = 0
        while index < bytes.count {
            switch bytes[index] {
            case 0x7b:
                containers.append(.object(ObjectKeys()))
                guard containers.count <= 65 else { throw WireError.size }
            case 0x5b:
                containers.append(.array)
                guard containers.count <= 65 else { throw WireError.size }
            case 0x7d, 0x5d:
                guard !containers.isEmpty else { throw WireError.invalid }
                containers.removeLast()
            case 0x2c:
                if case .object(let keys) = containers.last { keys.expectsKey = true }
            case 0x22:
                let start = index
                index += 1
                while index < bytes.count && bytes[index] != 0x22 {
                    if bytes[index] == 0x5c { index += 1 }
                    index += 1
                }
                guard index < bytes.count else { throw WireError.invalid }
                if case .object(let keys) = containers.last, keys.expectsKey {
                    keyCount += 1
                    guard keyCount <= 32_768,
                          let key = try JSONSerialization.jsonObject(with: Data(bytes[start...index]), options: [.fragmentsAllowed]) as? String,
                          keys.values.insert(key).inserted else { throw WireError.invalid }
                    keys.expectsKey = false
                }
            default: break
            }
            index += 1
        }
        guard containers.isEmpty else { throw WireError.invalid }
    }

    private static func convert(_ value: Any, depth: Int) throws -> WireValue {
        guard depth <= 64 else { throw WireError.size }
        if value is NSNull { return .null }
        if let number = value as? NSNumber {
            if CFGetTypeID(number) == CFBooleanGetTypeID() { return .bool(number.boolValue) }
            let type = String(cString: number.objCType)
            if ["q", "i", "s", "l", "c"].contains(type) {
                return number.int64Value >= 0 ? .unsigned(number.uint64Value) : .signed(number.int64Value)
            }
            if ["Q", "I", "S", "L", "C"].contains(type) { return .unsigned(number.uint64Value) }
            guard number.doubleValue.isFinite else { throw WireError.invalid }
            return .number(number.doubleValue)
        }
        if let string = value as? String { return .string(string) }
        if let array = value as? [Any] { return .array(try array.map { try convert($0, depth: depth + 1) }) }
        if let object = value as? [String: Any] {
            return .object(try object.mapValues { try convert($0, depth: depth + 1) })
        }
        throw WireError.invalid
    }

    func encoded(maximum: Int = 1_048_576) throws -> Data {
        let data = try JSONSerialization.data(withJSONObject: foundation, options: [.sortedKeys, .fragmentsAllowed, .withoutEscapingSlashes])
        guard !data.isEmpty, data.count <= maximum else { throw WireError.size }
        return data
    }

    private var foundation: Any {
        switch self {
        case .object(let value): value.mapValues(\.foundation)
        case .array(let value): value.map(\.foundation)
        case .string(let value): value
        case .unsigned(let value): NSNumber(value: value)
        case .signed(let value): NSNumber(value: value)
        case .number(let value): NSNumber(value: value)
        case .bool(let value): NSNumber(value: value)
        case .null: NSNull()
        }
    }

    var fields: [String: WireValue] {
        get throws {
            guard case .object(let value) = self else { throw WireError.invalid }
            return value
        }
    }

    func object(keys allowed: Set<String>, required: Set<String>) throws -> [String: WireValue] {
        let fields = try fields
        guard Set(fields.keys).isSubset(of: allowed), required.isSubset(of: Set(fields.keys)) else { throw WireError.invalid }
        return fields
    }

    func field(_ name: String) throws -> WireValue {
        guard let value = try fields[name] else { throw WireError.invalid }
        return value
    }

    var text: String {
        get throws {
            guard case .string(let value) = self else { throw WireError.invalid }
            return value
        }
    }

    var integer: UInt64 {
        get throws {
            guard case .unsigned(let value) = self else { throw WireError.invalid }
            return value
        }
    }

    var flag: Bool {
        get throws {
            guard case .bool(let value) = self else { throw WireError.invalid }
            return value
        }
    }

    var elements: [WireValue] {
        get throws {
            guard case .array(let value) = self else { throw WireError.invalid }
            return value
        }
    }

    var identifier: UUID {
        get throws {
            guard let value = UUID(uuidString: try text), value != WireID.zero else { throw WireError.invalid }
            return value
        }
    }

    func bytes(count: Int) throws -> Data {
        let string = try text
        guard let data = Data(base64Encoded: string), data.count == count, data.base64EncodedString() == string else { throw WireError.invalid }
        return data
    }

    static func id(_ value: UUID) -> WireValue { .string(value.uuidString.lowercased()) }
    static func bytes(_ value: Data) -> WireValue { .string(value.base64EncodedString()) }
}

enum WireError: Error, LocalizedError, Sendable, Equatable {
    case invalid, size, signature, expired, identity, pairing, revoked, storage, conflict, gap, queueFull, network, unavailable, timeout

    var errorDescription: String? {
        switch self {
        case .invalid: "The received data does not match the Everything protocol."
        case .size: "The operation exceeds its bounded size."
        case .signature: "The device signature or encrypted message could not be authenticated."
        case .expired: "The message, invitation or permission request has expired."
        case .identity: "The device identity does not match the pinned keys."
        case .pairing: "Pairing requires a fresh invitation and explicit fingerprint confirmation."
        case .revoked: "This device connection has been permanently disabled."
        case .storage: "Protected local storage is unavailable."
        case .conflict: "An existing identifier was reused for different content."
        case .gap: "Conversation history has a gap; synchronization is required."
        case .queueFull: "The local queue is full. Existing work has been preserved."
        case .network: "The relay connection is unavailable."
        case .unavailable: "Unlock the device to access its protected identity."
        case .timeout: "The relay operation timed out."
        }
    }
}

enum WireID {
    static let zero = UUID(uuid: (0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0))

    static func bytes(_ value: UUID) -> Data {
        let tuple = value.uuid
        return Data([tuple.0, tuple.1, tuple.2, tuple.3, tuple.4, tuple.5, tuple.6, tuple.7, tuple.8, tuple.9, tuple.10, tuple.11, tuple.12, tuple.13, tuple.14, tuple.15])
    }

    static func now() throws -> UInt64 {
        let milliseconds = Date().timeIntervalSince1970 * 1_000
        guard milliseconds.isFinite, milliseconds >= 0, milliseconds < Double(Int64.max) else { throw WireError.invalid }
        return UInt64(milliseconds.rounded(.down))
    }

    static func timestamp(_ value: WireValue) throws -> UInt64 {
        let value = try value.integer
        guard value <= UInt64(Int64.max) else { throw WireError.invalid }
        return value
    }
}

extension Data {
    mutating func appendBE<T: FixedWidthInteger>(_ value: T) {
        var value = value.bigEndian
        Swift.withUnsafeBytes(of: &value) { append(contentsOf: $0) }
    }

    static func domain(_ value: String) -> Data { Data((value + "\0").utf8) }
}

extension Dictionary where Key == String, Value == WireValue {
    func required(_ name: String) throws -> WireValue {
        guard let value = self[name] else { throw WireError.invalid }
        return value
    }
}

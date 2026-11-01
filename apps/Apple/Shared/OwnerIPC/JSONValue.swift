import Foundation

enum OwnerClientError: Error, LocalizedError, Sendable {
    case invalidConfiguration
    case invalidResponse
    case busy
    case cancelled
    case timedOut
    case outputLimit
    case unavailable
    case privateInputCleanup
    case commandFailed(Int32)
    case hostRejected(String)
    case systemOneProviderEndpoint
    case processOutputSetupFailed(Int32)
    case processSignalFailed(signal: Int32, code: Int32)

    var errorDescription: String? {
        switch self {
        case .invalidConfiguration: "Choose an absolute, trusted Vorra executable and state directory."
        case .invalidResponse: "The host returned an unsupported or invalid response."
        case .busy: "Another owner operation is still finishing."
        case .cancelled: "The request was cancelled. Inspect current state before repeating a mutation."
        case .timedOut: "The request timed out. A submitted action may have committed; inspect current state."
        case .outputLimit: "The owner command exceeded its output limit."
        case .unavailable: "The host or configured executable is unavailable."
        case .privateInputCleanup: "The private owner request file could not be removed."
        case .commandFailed(let status): "The owner command failed with exit status \(status). Refresh current state before retrying."
        case .hostRejected(let reason): "The host rejected the request: " + reason + ". Refresh current state before retrying."
        case .systemOneProviderEndpoint: "The Host rejected the System One provider endpoint."
        case .processOutputSetupFailed(let code): "The owner command output could not be configured (OS error \(code)). Its action may have committed; inspect current state."
        case .processSignalFailed(let signal, let code): "The owner CLI child could not be signalled (signal \(signal), OS error \(code))."
        }
    }
}

struct OwnerCleanupFailure: Error, LocalizedError, Sendable {
    let original: (any Error)?
    let failures: [any Error]

    var errorDescription: String? {
        let reason = original?.localizedDescription ?? "The owner command may have completed; inspect current state."
        return reason + " Local cleanup failed: " + failures.map(\.localizedDescription).joined(separator: " ")
    }
}

indirect enum JSONValue: Codable, Equatable, Sendable {
    case object([String: JSONValue])
    case array([JSONValue])
    case string(String)
    case integer(Int64)
    case unsigned(UInt64)
    case decimal(Double)
    case bool(Bool)
    case null

    init(from decoder: Decoder) throws {
        let value = try decoder.singleValueContainer()
        if value.decodeNil() { self = .null }
        else if let decoded = try? value.decode(Bool.self) { self = .bool(decoded) }
        else if let decoded = try? value.decode(Int64.self) { self = .integer(decoded) }
        else if let decoded = try? value.decode(UInt64.self) { self = .unsigned(decoded) }
        else if let decoded = try? value.decode(Double.self), decoded.isFinite { self = .decimal(decoded) }
        else if let decoded = try? value.decode(String.self) { self = .string(decoded) }
        else if let decoded = try? value.decode([JSONValue].self) { self = .array(decoded) }
        else { self = .object(try value.decode([String: JSONValue].self)) }
    }

    func encode(to encoder: Encoder) throws {
        var value = encoder.singleValueContainer()
        switch self {
        case .object(let item): try value.encode(item)
        case .array(let item): try value.encode(item)
        case .string(let item): try value.encode(item)
        case .integer(let item): try value.encode(item)
        case .unsigned(let item): try value.encode(item)
        case .decimal(let item): try value.encode(item)
        case .bool(let item): try value.encode(item)
        case .null: try value.encodeNil()
        }
    }

    subscript(_ key: String) -> JSONValue {
        guard case .object(let object) = self else { return .null }
        return object[key] ?? .null
    }

    var string: String? {
        guard case .string(let value) = self else { return nil }
        return value
    }

    var bool: Bool? {
        guard case .bool(let value) = self else { return nil }
        return value
    }

    var unsigned: UInt64? {
        switch self {
        case .unsigned(let value): return value
        case .integer(let value) where value >= 0: return UInt64(value)
        default: return nil
        }
    }

    var array: [JSONValue]? {
        guard case .array(let value) = self else { return nil }
        return value
    }

    func requiringString(_ key: String, maximum: Int = 4096) throws -> String {
        guard let value = self[key].string, value.utf8.count <= maximum,
              !value.utf8.contains(0) else { throw OwnerClientError.invalidResponse }
        return value
    }

    func replacing(_ key: String, with value: JSONValue) throws -> JSONValue {
        guard case .object(var object) = self else { throw OwnerClientError.invalidResponse }
        object[key] = value
        return .object(object)
    }

    func encoded(maximum: Int = 2 * 1024 * 1024) throws -> Data {
        let data = try JSONEncoder().encode(self)
        guard !data.isEmpty, data.count <= maximum else { throw OwnerClientError.outputLimit }
        return data
    }

    static func decode(_ data: Data) throws -> JSONValue {
        guard !data.isEmpty, data.count <= 2 * 1024 * 1024 else { throw OwnerClientError.outputLimit }
        var depth = 0
        var quoted = false
        var escaped = false
        for byte in data {
            if quoted {
                if escaped { escaped = false }
                else if byte == 92 { escaped = true }
                else if byte == 34 { quoted = false }
            } else if byte == 34 { quoted = true }
            else if byte == 123 || byte == 91 {
                depth += 1
                guard depth <= 64 else { throw OwnerClientError.invalidResponse }
            } else if byte == 125 || byte == 93 {
                depth -= 1
                guard depth >= 0 else { throw OwnerClientError.invalidResponse }
            }
        }
        guard depth == 0, !quoted else { throw OwnerClientError.invalidResponse }
        return try JSONDecoder().decode(JSONValue.self, from: data)
    }

    var formatted: String {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes]
        guard let data = try? encoder.encode(self), let text = String(data: data, encoding: .utf8) else {
            return "Unable to display this value."
        }
        return text
    }
}

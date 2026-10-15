import Foundation

struct OwnerConnection: Equatable, Sendable {
    let executable: String
    let stateDirectory: String

    func validate() throws {
        guard executable.hasPrefix("/"), executable.utf8.count <= 4096,
              !executable.utf8.contains(0), stateDirectory.hasPrefix("/"),
              stateDirectory != "/", stateDirectory.utf8.count + 10 <= 100,
              !stateDirectory.utf8.contains(0),
              !executable.split(separator: "/").contains(where: { $0 == "." || $0 == ".." }),
              !stateDirectory.split(separator: "/").contains(where: { $0 == "." || $0 == ".." }) else {
            throw OwnerClientError.invalidConfiguration
        }
    }
}

struct OwnerID: Hashable, Sendable, Identifiable {
    let value: UUID
    var id: Self { self }
    var string: String { value.uuidString.lowercased() }

    init(_ string: String) throws {
        guard let value = UUID(uuidString: string), value.uuidString != "00000000-0000-0000-0000-000000000000" else {
            throw OwnerClientError.invalidResponse
        }
        self.value = value
    }

    init() { value = UUID() }
}

struct OwnerRequest: Sendable {
    let id: OwnerID
    let body: JSONValue
    let mutating: Bool

    static func query(_ kind: String, fields: [String: JSONValue] = [:]) -> OwnerRequest {
        var query = fields
        query["kind"] = .string(kind)
        return OwnerRequest(id: OwnerID(), body: .object(["kind": .string("query"), "query": .object(query)]), mutating: false)
    }

    static func command(conversation: OwnerID, payload: [String: JSONValue]) throws -> OwnerRequest {
        let id = OwnerID()
        let milliseconds = Date().timeIntervalSince1970 * 1000
        guard milliseconds >= 0, milliseconds < Double(Int64.max) else { throw OwnerClientError.invalidConfiguration }
        let command: JSONValue = .object([
            "schema_version": .integer(1), "command_id": .string(id.string),
            "conversation_id": .string(conversation.string), "issuing_device_id": .null,
            "issued_at": .integer(Int64(milliseconds)), "payload": .object(payload)
        ])
        return OwnerRequest(id: id, body: .object(["kind": .string("command"), "command": command]), mutating: true)
    }

    static func operation(_ kind: String, fields: [String: JSONValue] = [:]) -> OwnerRequest {
        let id = OwnerID()
        var operation = fields
        operation["kind"] = .string(kind)
        return OwnerRequest(id: id, body: .object(["kind": .string("operation"), "request_id": .string(id.string), "operation": .object(operation)]), mutating: true)
    }
}

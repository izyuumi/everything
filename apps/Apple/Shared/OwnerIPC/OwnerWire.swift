import Foundation

enum OwnerPrivateTransportError: Error, LocalizedError, Sendable {
    case requiresPrivatePipe
    case invalidInput
    case inputSetupFailed(Int32)
    case inputWriteFailed(Int32)
    case inputCloseFailed(Int)
    case inputNotSubmitted

    var errorDescription: String? {
        switch self {
        case .requiresPrivatePipe: "This owner request requires the private pipe transport."
        case .invalidInput: "The private owner request must contain between one byte and 32 KiB."
        case .inputSetupFailed(let code): "The private owner input pipe could not be configured (OS error \(code))."
        case .inputWriteFailed(let code): "The private owner input pipe failed (OS error \(code)). Inspect the original action before retrying."
        case .inputCloseFailed(let code): "The private owner input pipe could not be closed (OS error \(code)). Inspect the original action before retrying."
        case .inputNotSubmitted: "The owner CLI exited before the complete private request was submitted. Inspect the original action before retrying."
        }
    }
}

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

    func validateForFileTransfer() throws {
        switch body["kind"].string {
        case "query", "command": return
        case "operation":
            switch body["operation"]["kind"].string {
            case "worker_coordinator", "revoke_worker_peer", "schedule", "acknowledge_scheduled_unknown",
                 "prepare_selection", "approve_selection", "discard_selection", "memory_enqueue", "memory_cancel",
                 "memory_retry", "apply_memory_plan", "rebuild_embeddings", "discard_unapplied", "replace_settings", "reconcile_settings",
                 "revoke_device", "wake_task", "retry_provider", "resume_interrupted",
                 "acknowledge_cleanup", "reconcile_drained": return
            default: throw OwnerPrivateTransportError.requiresPrivatePipe
            }
        default: throw OwnerPrivateTransportError.requiresPrivatePipe
        }
    }

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

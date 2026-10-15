import Foundation

struct HostTask: Identifiable, Sendable {
    let id: OwnerID
    let source: OwnerConnection
    let conversationID: OwnerID
    let instruction: String
    let status: String
    let failure: String?

    var terminal: Bool { ["completed", "failed", "cancelled"].contains(status) }

    init(_ value: JSONValue, connection: OwnerConnection) throws {
        source = connection
        guard value["schema_version"].unsigned == 1 else { throw OwnerClientError.invalidResponse }
        id = try OwnerID(value.requiringString("id", maximum: 36))
        conversationID = try OwnerID(value.requiringString("conversation_id", maximum: 36))
        instruction = try value.requiringString("instruction", maximum: 131072)
        status = try value.requiringString("status", maximum: 32)
        guard ["queued", "running", "awaiting_permission", "waiting_for_children", "completed", "failed", "cancelled"].contains(status) else {
            throw OwnerClientError.invalidResponse
        }
        failure = value["failure"]["message"].string
    }
}

struct HostConversation: Identifiable, Sendable {
    let id: OwnerID
    let title: String

    init(_ value: JSONValue) throws {
        id = try OwnerID(value.requiringString("id", maximum: 36))
        title = try value.requiringString("title", maximum: 4096)
    }
}

struct PendingApproval: Identifiable, Sendable {
    let id: String
    let source: OwnerConnection
    let conversationID: OwnerID
    let taskID: OwnerID
    let operationID: OwnerID
    let tool: String
    let argumentsBytes: UInt64
    let requestedAt: UInt64

    init(_ value: JSONValue, conversation: OwnerID, connection: OwnerConnection) throws {
        source = connection
        id = try value.requiringString("id", maximum: 256)
        conversationID = try OwnerID(value.requiringString("conversation_id", maximum: 36))
        taskID = try OwnerID(value.requiringString("task_id", maximum: 36))
        operationID = try OwnerID(value.requiringString("operation_id", maximum: 36))
        tool = try value.requiringString("tool_name", maximum: 128)
        guard !id.isEmpty, conversationID == conversation,
              value["status"]["status"].string == "pending",
              let bytes = value["arguments_bytes"].unsigned,
              let time = value["requested_at"].unsigned else { throw OwnerClientError.invalidResponse }
        argumentsBytes = bytes
        requestedAt = time
    }
}

struct ApprovalDetails: Identifiable, Sendable {
    let summary: PendingApproval
    let value: JSONValue
    var id: String { summary.id }
    var request: JSONValue { value["request"] }
    var pending: Bool { value["status"]["status"].string == "pending" }

    init(summary: PendingApproval, value: JSONValue) throws {
        let scope = value["request"]["scope"]
        guard value["request"]["id"].string == summary.id,
              scope["conversation_id"].string == summary.conversationID.string,
              scope["task_id"].string == summary.taskID.string,
              scope["operation_id"].string == summary.operationID.string,
              scope["tool_name"].string == summary.tool else { throw OwnerClientError.invalidResponse }
        self.summary = summary
        self.value = value
    }
}

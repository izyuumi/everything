import Foundation

struct WireEvent: Sendable, Equatable, Identifiable {
    let raw: WireValue
    let id: UUID
    let conversationID: UUID
    let sequence: UInt64
    let occurred: UInt64
    let causationID: UUID?
    let payload: WireValue
    let kind: String

    init(_ raw: WireValue) throws {
        guard try raw.encoded(maximum: 262_144).count <= 262_144,
              try raw.field("schema_version").integer == 1 else { throw WireError.invalid }
        self.raw = raw
        id = try raw.field("event_id").identifier
        conversationID = try raw.field("conversation_id").identifier
        sequence = try raw.field("sequence").integer
        guard sequence > 0, sequence <= UInt64(Int64.max) else { throw WireError.invalid }
        occurred = try WireID.timestamp(raw.field("occurred_at"))
        let cause = try raw.fields["causation_id"] ?? .null
        causationID = cause == .null ? nil : try cause.identifier
        payload = try raw.field("payload")
        kind = try payload.field("kind").text
        try validatePayload()
    }

    private func validatePayload() throws {
        switch kind {
        case "conversation_created":
            if let title = try payload.fields["title"], title != .null { _ = try title.text }
        case "message_appended": try Self.validateMessage(payload.field("message"))
        case "assistant_text_delta":
            _ = try payload.field("task_id").identifier
            _ = try payload.field("message_id").identifier
            _ = try payload.field("text").text
        case "task_created":
            let task = try payload.field("task")
            guard try task.field("schema_version").integer == 1, try task.field("conversation_id").identifier == conversationID else { throw WireError.invalid }
            for key in ["id", "originating_command_id"] { _ = try task.field(key).identifier }
            if let parent = try task.fields["parent_task_id"], parent != .null { _ = try parent.identifier }
            _ = try task.field("instruction").text
            try Self.validateStatus(task.field("status"))
            for key in ["created_at", "updated_at"] { _ = try WireID.timestamp(task.field(key)) }
            try Self.validateFailure(task.fields["failure"])
        case "task_status_changed":
            _ = try payload.field("task_id").identifier
            try Self.validateStatus(payload.field("status"))
            try Self.validateFailure(payload.fields["failure"])
        case "tool_requested":
            _ = try payload.field("task_id").identifier
            try Self.validateCall(payload.field("call"))
        case "tool_completed":
            _ = try payload.field("task_id").identifier
            try Self.validateResult(payload.field("result"))
        case "permission_requested":
            for key in ["task_id", "operation_id", "tool_call_id"] { _ = try payload.field(key).identifier }
            _ = try payload.field("tool_name").text
            if let inspection = try payload.fields["inspection"], inspection != .null { _ = try WirePermissionInspection(inspection) }
        case "permission_decided":
            _ = try payload.field("operation_id").identifier
            guard try ["allow", "deny"].contains(payload.field("decision").text) else { throw WireError.invalid }
        case "scheduled_run_finished": _ = try WireScheduledRunNotification(payload.field("notification"))
        case "remote_task_finished": _ = try WireRemoteTaskReport(payload.field("report"))
        case "steering_received", "task_wait_requested", "subagent_finished":
            _ = try payload.field("task_id").identifier
            _ = try payload.field("command_id").identifier
            if kind == "steering_received" { _ = try payload.field("instruction").text }
            if kind == "task_wait_requested" {
                let children = try payload.field("children").elements.map { try $0.identifier }
                guard !children.isEmpty, children.count <= 32, Set(children).count == children.count else { throw WireError.invalid }
            }
            if kind == "subagent_finished" {
                _ = try payload.field("wait_id").identifier
                _ = try payload.field("child_task_id").identifier
                try Self.validateStatus(payload.field("status"))
                _ = try payload.field("output").text
                _ = try payload.field("outcome_unknown").flag
            }
        default: throw WireError.invalid
        }
    }

    static func validateMessage(_ value: WireValue) throws {
        _ = try value.field("id").identifier
        guard try ["system", "user", "assistant", "tool"].contains(value.field("role").text) else { throw WireError.invalid }
        _ = try WireID.timestamp(value.field("created_at"))
        for content in try value.field("content").elements {
            switch try content.field("kind").text {
            case "text": _ = try content.field("text").text
            case "tool_call": try validateCall(content)
            case "tool_result": try validateResult(content)
            default: throw WireError.invalid
            }
        }
    }

    static func validateCall(_ value: WireValue) throws {
        _ = try value.field("id").identifier
        _ = try value.field("name").text
        _ = try value.field("arguments")
        if let providerID = try value.fields["provider_call_id"], providerID != .null { _ = try providerID.text }
    }

    static func validateResult(_ value: WireValue) throws {
        _ = try value.field("tool_call_id").identifier
        _ = try value.field("content").text
        _ = try value.field("is_error").flag
    }

    static func validateStatus(_ value: WireValue) throws {
        guard try ["queued", "running", "awaiting_permission", "waiting_for_children", "completed", "failed", "cancelled"].contains(value.text) else { throw WireError.invalid }
    }

    static func validateFailure(_ value: WireValue?) throws {
        guard let value, value != .null else { return }
        _ = try value.field("code").text
        _ = try value.field("message").text
        _ = try value.field("retryable").flag
    }
}

struct WireSyncBatch: Sendable {
    let raw: WireValue
    let conversationID: UUID
    let after: UInt64
    let events: [WireEvent]
    let hasMore: Bool
    var through: UInt64 { events.last?.sequence ?? after }

    init(_ raw: WireValue) throws {
        _ = try raw.encoded(maximum: 524_288)
        guard try raw.field("schema_version").integer == 1 else { throw WireError.invalid }
        self.raw = raw
        let cursor = try raw.field("cursor")
        conversationID = try cursor.field("conversation_id").identifier
        after = try cursor.field("after_sequence").integer
        guard after <= UInt64(Int64.max) else { throw WireError.invalid }
        events = try raw.field("events").elements.map(WireEvent.init)
        hasMore = try raw.field("has_more").flag
        guard events.count <= 256, !hasMore || !events.isEmpty else { throw WireError.invalid }
        var previous = after
        var seen = Set<UUID>()
        for event in events {
            guard previous < UInt64(Int64.max), event.sequence == previous + 1,
                  event.conversationID == conversationID, seen.insert(event.id).inserted else { throw WireError.gap }
            previous = event.sequence
        }
    }
}

enum DevicePackets {
    static func packet(_ payload: WireValue) -> WireValue { .object(["schema_version": .unsigned(1), "payload": payload]) }
    static func cursor(_ conversation: UUID, _ after: UInt64) -> WireValue { .object(["conversation_id": .id(conversation), "after_sequence": .unsigned(after)]) }
    static func sync(_ conversation: UUID, after: UInt64) -> WireValue {
        packet(.object(["kind": .string("sync_request"), "request": .object(["schema_version": .unsigned(1), "cursors": .array([cursor(conversation, after)]), "max_events": .unsigned(256)])]))
    }
    static func command(_ conversation: UUID, device: UUID, payload: WireValue, now: UInt64, id: UUID) -> WireValue {
        .object(["schema_version": .unsigned(1), "command_id": .id(id), "conversation_id": .id(conversation), "issuing_device_id": .id(device), "issued_at": .unsigned(now), "payload": payload])
    }
}

struct WirePermissionInspection: Sendable, Equatable {
    let raw: WireValue
    let approvalID: String
    let workspaceRoot: String
    let mode: String
    let capabilities: [String]
    let targets: [String]
    let argumentsJSON: String
    let argumentsDigest: String
    let requestedAt: UInt64
    let expiresAt: UInt64

    init(_ raw: WireValue) throws {
        _ = try raw.encoded(maximum: 32_768)
        let keys: Set<String> = ["approval_id", "workspace_root", "mode", "capabilities", "targets", "arguments_json", "arguments_digest", "requested_at", "expires_at"]
        let fields = try raw.object(keys: keys, required: keys)
        approvalID = try fields.required("approval_id").text
        workspaceRoot = try fields.required("workspace_root").text
        mode = try fields.required("mode").text
        capabilities = try fields.required("capabilities").elements.map { try $0.text }
        targets = try fields.required("targets").elements.map { try $0.text }
        argumentsJSON = try fields.required("arguments_json").text
        argumentsDigest = try fields.required("arguments_digest").text
        requestedAt = try WireID.timestamp(fields.required("requested_at"))
        expiresAt = try WireID.timestamp(fields.required("expires_at"))
        guard !approvalID.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty, approvalID.utf8.count <= 256,
              !Self.hasControls(approvalID), workspaceRoot.hasPrefix("/"), workspaceRoot.utf8.count <= 4_096,
              !Self.hasControls(workspaceRoot), ["read_only", "trusted_workspace", "full_host"].contains(mode),
              capabilities.count <= 64, capabilities.allSatisfy({ ["filesystem_read", "filesystem_write", "shell", "network", "mcp"].contains($0) }),
              targets.count <= 64, targets.allSatisfy({ !$0.isEmpty && $0.utf8.count <= 4_096 && !Self.hasControls($0) }),
              argumentsJSON.utf8.count <= 16_384, Self.isDigest(argumentsDigest), expiresAt > requestedAt else { throw WireError.invalid }
        _ = try WireValue.parse(Data(argumentsJSON.utf8), maximum: 16_384)
        self.raw = raw
    }

    private static func hasControls(_ value: String) -> Bool { value.unicodeScalars.contains { CharacterSet.controlCharacters.contains($0) } }
    static func isDigest(_ value: String) -> Bool { value.utf8.count == 64 && value.utf8.allSatisfy { (48...57).contains($0) || (97...102).contains($0) } }
}

struct WireScheduledRunNotification: Sendable {
    let raw: WireValue
    let jobID: UUID
    let occurrenceID: UUID
    let generation: UInt64
    let scheduledFor: UInt64
    let finishedAt: UInt64
    let taskID: UUID?
    let executionConversationID: UUID?
    let outcome: String
    let summary: String
    let outcomeDigest: String

    init(_ raw: WireValue) throws {
        let keys: Set<String> = ["job_id", "occurrence_id", "generation", "scheduled_for", "finished_at", "task_id", "execution_conversation_id", "outcome", "summary", "outcome_digest"]
        let fields = try raw.object(keys: keys, required: keys.subtracting(["task_id", "execution_conversation_id"]))
        jobID = try fields.required("job_id").identifier
        occurrenceID = try fields.required("occurrence_id").identifier
        generation = try fields.required("generation").integer
        scheduledFor = try WireID.timestamp(fields.required("scheduled_for"))
        finishedAt = try WireID.timestamp(fields.required("finished_at"))
        let task = fields["task_id"] ?? .null
        let conversation = fields["execution_conversation_id"] ?? .null
        taskID = task == .null ? nil : try task.identifier
        executionConversationID = conversation == .null ? nil : try conversation.identifier
        outcome = try fields.required("outcome").text
        summary = try fields.required("summary").text
        outcomeDigest = try fields.required("outcome_digest").text
        guard generation > 0, generation <= UInt64(Int64.max), (taskID == nil) == (executionConversationID == nil),
              ["succeeded", "failed", "cancelled", "skipped", "unknown"].contains(outcome),
              summary.utf8.count <= 4_096, !summary.contains("\0"), WirePermissionInspection.isDigest(outcomeDigest) else { throw WireError.invalid }
        self.raw = raw
    }
}

struct WireRemoteTaskReport: Sendable {
    let raw: WireValue

    init(_ raw: WireValue) throws {
        _ = try raw.encoded(maximum: 65_536)
        let keys: Set<String> = ["task_id", "command_id", "delegation_id", "attempt_id", "manifest_digest", "executor", "executor_fingerprint", "source_message_id", "envelope_digest", "packet_digest", "source", "outcome", "summary", "usage", "artifact_count", "quarantine_digest", "finished_at", "accepted_at"]
        let fields = try raw.object(keys: keys, required: keys.subtracting(["quarantine_digest", "finished_at"]))
        for key in ["task_id", "command_id", "delegation_id", "attempt_id", "executor", "source_message_id"] { _ = try fields.required(key).identifier }
        for key in ["manifest_digest", "envelope_digest", "packet_digest"] {
            guard try WirePermissionInspection.isDigest(fields.required(key).text) else { throw WireError.invalid }
        }
        let fingerprint = try fields.required("executor_fingerprint").text
        guard fingerprint.utf8.count == 71, fingerprint.utf8.enumerated().allSatisfy({ index, byte in
            index % 9 == 8 ? byte == 45 : (48...57).contains(byte) || (97...102).contains(byte)
        }) else { throw WireError.invalid }
        let outcome = try fields.required("outcome").text
        let summary = try fields.required("summary").text
        let artifactCount = try fields.required("artifact_count").integer
        guard ["succeeded", "failed", "cancelled", "unknown", "rejected"].contains(outcome),
              summary.utf8.count <= 8_192, !summary.contains("\0"), artifactCount <= 64 else { throw WireError.invalid }
        _ = try WireID.timestamp(fields.required("accepted_at"))
        let finished = fields["finished_at"] ?? .null
        if finished != .null { _ = try WireID.timestamp(finished) }
        let quarantine = fields["quarantine_digest"] ?? .null
        let quarantineDigest = quarantine == .null ? nil : try quarantine.text
        if let quarantineDigest, !WirePermissionInspection.isDigest(quarantineDigest) { throw WireError.invalid }
        guard artifactCount == 0 || quarantineDigest != nil,
              !["unknown", "rejected"].contains(outcome) || artifactCount == 0 && quarantineDigest == nil else { throw WireError.invalid }
        let usage = try fields.required("usage")
        let usageKeys: Set<String> = ["model_attempts", "tool_calls", "charged_tokens", "reserved_output_tokens", "reported_input_tokens", "reported_output_tokens"]
        let counters = try usage.object(keys: usageKeys, required: usageKeys.subtracting(["reserved_output_tokens"]))
        var emptyUsage = true
        for key in ["model_attempts", "tool_calls", "charged_tokens", "reported_input_tokens", "reported_output_tokens"] {
            let counter = try counters.required(key).integer
            if ["model_attempts", "tool_calls"].contains(key), counter > UInt64(UInt32.max) { throw WireError.invalid }
            if counter != 0 { emptyUsage = false }
        }
        if let reserved = counters["reserved_output_tokens"], reserved != .null {
            _ = try reserved.integer
            emptyUsage = false
        }
        let source = try fields.required("source")
        switch try source.field("kind").text {
        case "result":
            let sourceKeys: Set<String> = ["kind", "result_digest"]
            let sourceFields = try source.object(keys: sourceKeys, required: sourceKeys)
            let resultDigest = try sourceFields.required("result_digest").text
            guard WirePermissionInspection.isDigest(resultDigest), outcome != "rejected", finished != .null,
                  quarantineDigest == nil || quarantineDigest == resultDigest else { throw WireError.invalid }
        case "rejected":
            let sourceKeys: Set<String> = ["kind", "reason"]
            let sourceFields = try source.object(keys: sourceKeys, required: sourceKeys)
            let reason = try sourceFields.required("reason").text
            guard !reason.isEmpty, reason.utf8.count <= 256,
                  !reason.unicodeScalars.contains(where: { $0.properties.generalCategory == .control }),
                  outcome == "rejected", finished == .null, emptyUsage else { throw WireError.invalid }
        default: throw WireError.invalid
        }
        self.raw = raw
    }
}

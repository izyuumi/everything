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
    let provider: String
    let model: String
    let lastSequence: UInt64
    let messageCount: UInt64
    let agentID: OwnerID?

    init(_ value: JSONValue) throws {
        id = try OwnerID(value.requiringString("id", maximum: 36))
        agentID = value["agent_id"] == .null ? nil : try OwnerID(value.requiringString("agent_id", maximum: 36))
        title = try value.requiringString("title", maximum: 4096)
        provider = try value.requiringString("provider", maximum: 32)
        model = try value.requiringString("model", maximum: 256)
        guard let sequence = value["last_sequence"].unsigned, let count = value["message_count"].unsigned else { throw OwnerClientError.invalidResponse }
        lastSequence = sequence
        messageCount = count
    }
}

struct TranscriptEntry: Identifiable, Sendable {
    enum Kind: Sendable { case text, toolCall, toolResult, toolError }
    let id: String
    let role: String
    let kind: Kind
    let tool: String?
    let text: String
    let date: Date
}

struct TranscriptPage: Sendable {
    let entries: [TranscriptEntry]
    let through: UInt64

    init(_ response: JSONValue, conversation: OwnerID, after: UInt64) throws {
        let batch = response["batch"]
        guard response["kind"].string == "synchronized",
              batch["cursor"]["conversation_id"].string == conversation.string,
              batch["cursor"]["after_sequence"].unsigned == after,
              let events = batch["events"].array, events.count <= 64 else { throw OwnerClientError.invalidResponse }
        var previous = after
        var entries: [TranscriptEntry] = []
        for event in events {
            guard event["conversation_id"].string == conversation.string,
                  let sequence = event["sequence"].unsigned, sequence > previous else { throw OwnerClientError.invalidResponse }
            previous = sequence
            guard event["payload"]["kind"].string == "message_appended" else { continue }
            let message = event["payload"]["message"]
            let identifier = try message.requiringString("id", maximum: 36)
            let role = try message.requiringString("role", maximum: 16)
            guard ["system", "user", "assistant", "tool"].contains(role), let parts = message["content"].array,
                  let created = message["created_at"].unsigned else { throw OwnerClientError.invalidResponse }
            guard role != "system" else { continue }
            let date = try ScheduleFields.date(.unsigned(created))
            for (index, part) in parts.enumerated() {
                let id = identifier + "-\(index)"
                switch part["kind"].string {
                case "text":
                    entries.append(TranscriptEntry(id: id, role: role, kind: .text, tool: nil, text: try part.requiringString("text", maximum: 4 * 1024 * 1024), date: date))
                case "tool_call":
                    entries.append(TranscriptEntry(id: id, role: role, kind: .toolCall, tool: try part.requiringString("name", maximum: 256), text: JSONSummary.lines(part["arguments"]), date: date))
                case "tool_result":
                    guard let error = part["is_error"].bool else { throw OwnerClientError.invalidResponse }
                    entries.append(TranscriptEntry(id: id, role: role, kind: error ? .toolError : .toolResult, tool: nil, text: try part.requiringString("content", maximum: 4 * 1024 * 1024), date: date))
                default: throw OwnerClientError.invalidResponse
                }
            }
        }
        self.entries = entries
        through = previous
    }
}

enum JSONSummary {
    static func lines(_ value: JSONValue, limit: Int = 12) -> String {
        guard case .object(let object) = value else { return clipped(value) }
        let keys = object.keys.sorted()
        var lines = keys.prefix(limit).map { $0 + ": " + clipped(object[$0] ?? .null) }
        if keys.count > limit { lines.append("…and \(keys.count - limit) more") }
        return lines.isEmpty ? "No arguments" : lines.joined(separator: "\n")
    }

    static func clipped(_ value: JSONValue) -> String {
        let text = value.string ?? value.formatted.replacingOccurrences(of: "\n", with: " ")
        return text.count > 160 ? String(text.prefix(160)) + "…" : text
    }

    static func label(_ value: String) -> String { value.replacingOccurrences(of: "_", with: " ").capitalized }

    static func date(_ milliseconds: UInt64?) -> String {
        guard let milliseconds else { return "Unknown" }
        return Date(timeIntervalSince1970: Double(milliseconds) / 1000).formatted(date: .abbreviated, time: .standard)
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
    var targets: [String] { request["scope"]["targets"].array?.compactMap(\.string) ?? [] }
    var vaultUse: Bool { summary.tool == "vault_use" && targets.contains("vault_use") }
    var leasable: Bool { vaultUse && !targets.contains("vault_kind=card") }

    func vaultTargets(_ name: String) -> [String] {
        targets.compactMap { $0.hasPrefix("vault_" + name + "=") ? String($0.dropFirst(name.count + 7)) : nil }
    }

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

struct HostScheduleEngine: Sendable {
    struct Blocked: Identifiable, Sendable {
        let id: OwnerID
        let diagnostic: String
    }
    let active: Bool
    let stopped: Bool
    let blockedCount: UInt64
    let blocked: [Blocked]
    let pendingDeliveries: UInt64
    let errors: [String]

    init(_ response: JSONValue) throws {
        guard response["kind"].string == "schedule_engine",
              let active = response["status"]["active"].bool,
              let stopped = response["status"]["stopped"].bool,
              let blockedCount = response["status"]["blocked_jobs"].unsigned,
              let pending = response["status"]["pending_deliveries"].unsigned,
              let rows = response["status"]["blocked"].array, rows.count <= 64,
              UInt64(rows.count) <= blockedCount else { throw OwnerClientError.invalidResponse }
        self.active = active
        self.stopped = stopped
        self.blockedCount = blockedCount
        pendingDeliveries = pending
        blocked = try rows.map { row in
            Blocked(id: try OwnerID(row.requiringString("job_id", maximum: 36)), diagnostic: try row.requiringString("diagnostic", maximum: 1024))
        }
        guard Set(blocked.map(\.id)).count == blocked.count else { throw OwnerClientError.invalidResponse }
        errors = try ["last_error", "delivery_error", "delivery_preparation_error"].compactMap {
            try ScheduleFields.optionalText(response["status"][$0], maximum: 1024)
        }
    }
}

struct HostScheduleJob: Identifiable, Sendable {
    let id: OwnerID
    let source: OwnerConnection
    let name: String
    let enabled: Bool
    let deleted: Bool
    let generation: UInt64
    let revision: UInt64
    let nextRun: Date?
    let lastRun: Date?
    let frequency: String

    init(_ value: JSONValue, connection: OwnerConnection) throws {
        source = connection
        id = try OwnerID(value.requiringString("id", maximum: 36))
        name = try value.requiringString("name", maximum: 256)
        guard !name.isEmpty, let enabled = value["enabled"].bool,
              let generation = value["generation"].unsigned, generation > 0,
              let revision = value["revision"].unsigned, revision > 0,
              value["definition"]["schema_version"].unsigned == 1 else { throw OwnerClientError.invalidResponse }
        self.enabled = enabled
        self.generation = generation
        self.revision = revision
        deleted = try ScheduleFields.optionalDate(value["deleted_at"]) != nil
        nextRun = try ScheduleFields.optionalDate(value["next_run_at"])
        lastRun = try ScheduleFields.optionalDate(value["last_run_at"])
        let schedule = value["definition"]["schedule"]
        switch schedule["kind"].string {
        case "once":
            _ = try ScheduleFields.timestamp(schedule["at"])
            frequency = "Once"
        case "interval":
            guard let interval = schedule["every_ms"].unsigned, (1000...31_622_400_000).contains(interval) else { throw OwnerClientError.invalidResponse }
            _ = try ScheduleFields.timestamp(schedule["anchor"])
            frequency = "Every \(interval) milliseconds"
        case "cron":
            frequency = try schedule.requiringString("expression", maximum: 512) + " · " + schedule.requiringString("time_zone", maximum: 128)
        default: throw OwnerClientError.invalidResponse
        }
    }
}

struct HostScheduleRun: Identifiable, Sendable {
    let id: OwnerID
    let jobID: OwnerID
    let claimedAt: UInt64
    let scheduledFor: Date
    let finishedAt: Date?
    let state: String
    let summary: String?
    let attempts: Int
    let lastDiagnostic: String?
    let modelRequests: UInt64
    let toolCalls: UInt64
    let reservedOutputTokens: UInt64
    let usageKnown: Bool

    var cursor: JSONValue { .object(["claimed_at": .unsigned(claimedAt), "occurrence_id": .string(id.string)]) }
    var unknown: Bool { state == "unknown" }

    init(_ value: JSONValue, job: OwnerID) throws {
        let run = value["run"]
        id = try OwnerID(run.requiringString("occurrence_id", maximum: 36))
        jobID = try OwnerID(run.requiringString("job_id", maximum: 36))
        guard jobID == job, let generation = run["generation"].unsigned, generation > 0,
              let revision = run["revision"].unsigned, revision > 0 else { throw OwnerClientError.invalidResponse }
        claimedAt = try ScheduleFields.timestamp(run["claimed_at"])
        scheduledFor = try ScheduleFields.date(run["scheduled_for"])
        finishedAt = try ScheduleFields.optionalDate(run["finished_at"])
        state = try run.requiringString("state", maximum: 16)
        guard ["claimed", "running", "succeeded", "failed", "cancelled", "skipped", "unknown"].contains(state),
              let records = value["attempts"].array, records.count <= 8,
              let model = value["usage"]["model_requests"].unsigned,
              let tools = value["usage"]["tool_calls"].unsigned,
              let reserved = value["usage"]["reserved_output_tokens"].unsigned,
              let known = value["usage"]["known"].bool else { throw OwnerClientError.invalidResponse }
        modelRequests = model
        toolCalls = tools
        reservedOutputTokens = reserved
        usageKnown = known
        attempts = records.count
        var numbers = Set<UInt64>()
        var diagnostic: String?
        for record in records {
            guard let number = record["attempt_number"].unsigned, (1...8).contains(number), numbers.insert(number).inserted else { throw OwnerClientError.invalidResponse }
            _ = try OwnerID(record.requiringString("task_id", maximum: 36))
            _ = try OwnerID(record.requiringString("conversation_id", maximum: 36))
            _ = try ScheduleFields.timestamp(record["accepted_at"])
            _ = try ScheduleFields.timestamp(record["deadline"])
            if record["result"] != .null {
                let result = record["result"]
                guard result["occurrence_id"].string == id.string, result["attempt_number"].unsigned == number,
                      ["factory_failure", "terminal"].contains(result["kind"].string ?? "") else { throw OwnerClientError.invalidResponse }
                diagnostic = try result.requiringString("diagnostic", maximum: 1024)
            }
        }
        lastDiagnostic = diagnostic
        let outcome = run["outcome"]
        let terminal = !["claimed", "running"].contains(state)
        guard terminal == (outcome != .null), terminal == (finishedAt != nil) else { throw OwnerClientError.invalidResponse }
        if terminal {
            guard let digest = value["outcome_digest"].string, digest.utf8.count == 64,
                  digest.utf8.allSatisfy({ (48...57).contains($0) || (97...102).contains($0) }) else { throw OwnerClientError.invalidResponse }
        } else if value["outcome_digest"] != .null { throw OwnerClientError.invalidResponse }
        switch outcome["kind"].string {
        case nil where !terminal: summary = nil
        case "succeeded" where state == "succeeded": summary = try outcome.requiringString("summary", maximum: 16384)
        case "failed" where state == "failed": summary = try outcome.requiringString("code", maximum: 64) + ": " + outcome.requiringString("message", maximum: 16384)
        case "cancelled" where state == "cancelled", "unknown" where state == "unknown": summary = try outcome.requiringString("reason", maximum: 16384)
        case "skipped" where state == "skipped":
            guard ["overlap", "missed"].contains(outcome["reason"].string ?? "") else { throw OwnerClientError.invalidResponse }
            summary = outcome["reason"].string == "overlap" ? "Skipped because another occurrence holds the overlap slot." : "Skipped after the scheduled time was missed."
        case "skipped_missed_range" where state == "skipped":
            _ = try ScheduleFields.timestamp(outcome["through"])
            summary = "Skipped a range of missed occurrences."
        default: throw OwnerClientError.invalidResponse
        }
    }
}

enum ScheduleFields {
    static func timestamp(_ value: JSONValue) throws -> UInt64 {
        guard let time = value.unsigned, time <= 253_402_300_799_999 else { throw OwnerClientError.invalidResponse }
        return time
    }
    static func date(_ value: JSONValue) throws -> Date { Date(timeIntervalSince1970: Double(try timestamp(value)) / 1000) }
    static func optionalDate(_ value: JSONValue) throws -> Date? { value == .null ? nil : try date(value) }
    static func optionalText(_ value: JSONValue, maximum: Int) throws -> String? {
        guard value != .null else { return nil }
        guard let text = value.string, text.utf8.count <= maximum, !text.utf8.contains(0) else { throw OwnerClientError.invalidResponse }
        return text
    }
}

struct HostSchedulePage: Sendable {
    let jobs: [HostScheduleJob]
    let next: OwnerID?

    init(_ response: JSONValue, connection: OwnerConnection, after: OwnerID?) throws {
        guard response["kind"].string == "schedule", response["result"]["kind"].string == "jobs",
              let rows = response["result"]["jobs"].array, rows.count <= 10 else { throw OwnerClientError.invalidResponse }
        jobs = try rows.map { try HostScheduleJob($0, connection: connection) }
        var previous = after?.string
        for job in jobs {
            guard previous.map({ $0 < job.id.string }) ?? true else { throw OwnerClientError.invalidResponse }
            previous = job.id.string
        }
        let cursor = response["result"]["next_after"]
        next = cursor == .null ? nil : try OwnerID(cursor.string ?? "")
        if let next { guard next == jobs.last?.id else { throw OwnerClientError.invalidResponse } }
    }
}

struct HostScheduleHistory: Sendable {
    let runs: [HostScheduleRun]
    let next: JSONValue

    init(_ response: JSONValue, job: OwnerID, before: JSONValue) throws {
        guard response["kind"].string == "schedule", response["result"]["kind"].string == "history",
              let rows = response["result"]["runs"].array, rows.count <= 10 else { throw OwnerClientError.invalidResponse }
        runs = try rows.map { try HostScheduleRun($0, job: job) }
        var previous: (UInt64, String)?
        if before != .null {
            previous = (try ScheduleFields.timestamp(before["claimed_at"]), try OwnerID(before.requiringString("occurrence_id", maximum: 36)).string)
        }
        for run in runs {
            let key = (run.claimedAt, run.id.string)
            guard previous.map({ key < $0 }) ?? true else { throw OwnerClientError.invalidResponse }
            previous = key
        }
        next = response["result"]["next_before"]
        if next != .null {
            guard let last = runs.last,
                  try ScheduleFields.timestamp(next["claimed_at"]) == last.claimedAt,
                  try OwnerID(next.requiringString("occurrence_id", maximum: 36)) == last.id else { throw OwnerClientError.invalidResponse }
        }
    }
}

enum DisplayText {
    static func isFormat(_ scalar: Unicode.Scalar) -> Bool {
        scalar.value == 0x061C || (0x200B...0x200F).contains(scalar.value) || (0x202A...0x202E).contains(scalar.value)
            || (0x2066...0x2069).contains(scalar.value) || scalar.value == 0xFEFF || (0xE0000...0xE007F).contains(scalar.value)
    }

    static func escaped(_ value: String, multiline: Bool = false) -> String {
        var result = ""
        for scalar in value.unicodeScalars {
            let format = isFormat(scalar)
            let control = scalar.properties.generalCategory == .control && !(multiline && (scalar == "\n" || scalar == "\t"))
            if format || control { result += "\\u{" + String(scalar.value, radix: 16) + "}" }
            else { result.unicodeScalars.append(scalar) }
        }
        return result
    }

    static func clipped(_ value: String, maximum: Int) -> String {
        guard value.utf8.count > maximum else { return value }
        var result = ""
        var size = 0
        for character in value {
            size += character.utf8.count
            if size > maximum { break }
            result.append(character)
        }
        return result + "…"
    }
}

struct HostAgent: Identifiable, Sendable, Equatable {
    let id: OwnerID
    let name: String
    let description: String
    let persona: String?
    let revision: UInt64
    let model: String?
    let toolMode: String
    let tools: String
    let memory: String
    let skills: String
    let definition: JSONValue

    static let modes = ["off", "read_only", "trusted_workspace", "full_host"]
    static let providers = ["anthropic", "openai"]

    static func handle(_ value: String) -> Bool {
        value.utf8.count <= 32 && value.split(separator: "-", omittingEmptySubsequences: false).allSatisfy { part in
            !part.isEmpty && part.unicodeScalars.allSatisfy { ("a"..."z").contains($0) || ("0"..."9").contains($0) }
        }
    }

    static func identifier(_ value: String) -> Bool {
        !value.isEmpty && value.utf8.count <= 256
            && !value.unicodeScalars.contains { $0.properties.generalCategory == .control || $0.properties.isWhitespace }
    }

    static func loopbackEndpoint(_ text: String) -> String? {
        let allowed = Set("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-._~!$&'()*+,;=:@/%[]".unicodeScalars)
        guard text.utf8.count <= 2048, text.unicodeScalars.allSatisfy(allowed.contains),
              let separator = text.range(of: "://") else { return nil }
        let scheme = text[..<separator.lowerBound].lowercased()
        guard scheme == "http" || scheme == "https" else { return nil }
        let rest = text[separator.upperBound...]
        let authority = rest.prefix { $0 != "/" }
        let path = rest.dropFirst(authority.count)
        guard !authority.contains("@"), !path.contains("["), !path.contains("]") else { return nil }
        let bytes = Array(path.utf8)
        for (index, byte) in bytes.enumerated() where byte == UInt8(ascii: "%") {
            guard index + 2 < bytes.count, Character(Unicode.Scalar(bytes[index + 1])).isHexDigit,
                  Character(Unicode.Scalar(bytes[index + 2])).isHexDigit else { return nil }
        }
        let dots: Set<String> = [".", "..", "%2e", ".%2e", "%2e.", "%2e%2e"]
        guard !path.split(separator: "/", omittingEmptySubsequences: false).contains(where: { dots.contains($0.lowercased()) }) else { return nil }
        let host: String
        let portText: Substring
        if authority.hasPrefix("[") {
            guard let close = authority.firstIndex(of: "]") else { return nil }
            var address = in6_addr()
            guard inet_pton(AF_INET6, String(authority[authority.index(after: authority.startIndex)..<close]), &address) == 1,
                  withUnsafeBytes(of: &address, { Array($0) }) == Array(repeating: 0, count: 15) + [1] else { return nil }
            host = "[::1]"
            portText = authority[authority.index(after: close)...]
        } else {
            let name = authority.prefix { $0 != ":" }
            var address = in_addr()
            guard !name.contains("["), inet_pton(AF_INET, String(name), &address) == 1,
                  UInt32(bigEndian: address.s_addr) >> 24 == 127 else { return nil }
            var buffer = [CChar](repeating: 0, count: Int(INET_ADDRSTRLEN))
            guard inet_ntop(AF_INET, &address, &buffer, socklen_t(buffer.count)) != nil else { return nil }
            host = String(decoding: buffer.prefix { $0 != 0 }.map { UInt8(bitPattern: $0) }, as: UTF8.self)
            portText = authority.dropFirst(name.count)
        }
        var port = ""
        if !portText.isEmpty {
            guard portText.first == ":" else { return nil }
            let digits = portText.dropFirst()
            let significant = digits.drop { $0 == "0" }
            guard digits.allSatisfy({ $0.isASCII && $0.isNumber }), significant.count <= 5 else { return nil }
            let number = Int(significant) ?? 0
            guard number <= 65535 else { return nil }
            if !digits.isEmpty && number != (scheme == "https" ? 443 : 80) { port = ":" + String(number) }
        }
        let canonical = scheme + "://" + host + port + (path.isEmpty ? "/" : String(path))
        return canonical.utf8.count <= 2048 ? canonical : nil
    }

    func skillAssigned(_ skill: HostSkill) -> Bool {
        switch definition["skills"] {
        case .string("all"): skill.offered
        case .string("none"): false
        default: skill.offered && (definition["skills"]["only"].array ?? []).contains { entry in
            entry.string.map { !$0.unicodeScalars.contains(where: DisplayText.isFormat) && skill.matches($0) } == true
        }
        }
    }

    init(_ value: JSONValue) throws {
        id = try OwnerID(value.requiringString("id", maximum: 36))
        name = try value.requiringString("name", maximum: 32)
        description = try value.requiringString("description", maximum: 1024)
        persona = try ScheduleFields.optionalText(value["persona"], maximum: 32768)
        guard !name.isEmpty, let revision = value["revision"].unsigned, revision > 0, revision <= UInt64(Int64.max),
              let memory = value["memory"].string, ["shared", "off"].contains(memory) else { throw OwnerClientError.invalidResponse }
        self.revision = revision
        self.memory = memory
        let model = value["model"]
        self.model = model == .null ? nil : try model.requiringString("provider", maximum: 32) + " · " + model.requiringString("model", maximum: 256)
        if value["tools"] == .string("off") {
            toolMode = "off"
            tools = "Tools off"
        } else {
            let on = value["tools"]["on"]
            toolMode = try on.requiringString("max_mode", maximum: 32)
            guard HostAgent.modes.dropFirst().contains(toolMode) else { throw OwnerClientError.invalidResponse }
            let ceiling = JSONSummary.label(toolMode)
            if on["allow"] == .string("all") { tools = ceiling + " · all tools" }
            else if let only = on["allow"]["only"].array, only.count <= 256 {
                let names = try only.map { reference -> String in
                    if let tool = reference["tool"].string, tool.utf8.count <= 256 { return tool.hasPrefix("mcp:") ? "tool \"" + tool + "\"" : tool }
                    if let server = reference["mcp_server"].string, server.utf8.count <= 256 { return "mcp:" + server }
                    throw OwnerClientError.invalidResponse
                }
                tools = ceiling + (names.isEmpty ? " · no tools" : " · only " + names.joined(separator: ", "))
            } else { throw OwnerClientError.invalidResponse }
        }
        switch value["skills"] {
        case .string("none"): skills = "No skills"
        case .string("all"): skills = "All skills"
        default:
            guard let only = value["skills"]["only"].array, only.count <= 256 else { throw OwnerClientError.invalidResponse }
            skills = "Only " + (try only.map { guard let name = $0.string, name.utf8.count <= 256 else { throw OwnerClientError.invalidResponse }; return name }).joined(separator: ", ")
        }
        definition = value
    }
}

struct HostAgents: Sendable, Equatable {
    let revision: UInt64
    let defaultID: OwnerID
    let agents: [HostAgent]

    init(_ response: JSONValue) throws {
        let file = response["agents"]
        guard response["kind"].string == "agents", file["schema"].unsigned == 1, let revision = file["revision"].unsigned,
              let rows = file["agents"].array, rows.count <= 64 else { throw OwnerClientError.invalidResponse }
        self.revision = revision
        defaultID = try OwnerID(file.requiringString("default_agent", maximum: 36))
        agents = try rows.map(HostAgent.init)
        guard Set(agents.map(\.id)).count == agents.count, agents.contains(where: { $0.id == defaultID }) else { throw OwnerClientError.invalidResponse }
    }

    func name(_ id: OwnerID?) -> String? { agents.first { $0.id == (id ?? defaultID) }?.name }
}

struct HostSkill: Identifiable, Sendable {
    let id: Int
    let name: String
    let status: String
    let root: String?
    let description: String?
    let source: String?
    let path: String?
    let allowedTools: String?
    let preview: String
    let offered: Bool
    let shadowed: Bool
    let issues: [String]
    let assignableName: String?
    let folder: String?

    static func dirName(_ raw: String) -> String {
        var result = ""
        var run = false
        for scalar in raw.lowercased().unicodeScalars {
            if ("a"..."z").contains(scalar) || ("0"..."9").contains(scalar) || scalar == "." || scalar == "_" {
                result.unicodeScalars.append(scalar)
                run = false
            } else if !run {
                result += "-"
                run = true
            }
        }
        let trimmed = result.trimmingCharacters(in: CharacterSet(charactersIn: ".-"))
        return trimmed.isEmpty ? "unnamed-skill" : String(trimmed.prefix(255))
    }

    func matches(_ assigned: String) -> Bool {
        guard let assignableName else { return false }
        let wanted = HostSkill.dirName(assigned)
        return wanted == HostSkill.dirName(assignableName) || folder.map { HostSkill.dirName($0) == wanted } == true
    }

    init(_ value: JSONValue, index: Int) {
        func text(_ key: String, _ maximum: Int) -> String? { value[key].string.map { DisplayText.clipped($0, maximum: maximum) } }
        id = index
        root = text("root", 16)
        description = text("description", 4096)
        source = text("source", 4096)
        path = text("path", 4096)
        allowedTools = text("allowed_tools", 4096)
        preview = text("preview", 1024) ?? ""
        shadowed = value["shadowed"].bool ?? false
        let rows = value["issues"].array ?? []
        var issues = rows.prefix(64).compactMap { $0.string.map { DisplayText.clipped($0, maximum: 4096) } }
        if rows.count > 64 { issues.append("\(rows.count - 64) more issues") }
        let status = value["status"].string ?? ""
        let wellFormed = value["name"].string != nil && ["tracked", "untracked", "rejected", "missing"].contains(status)
            && value["offered"].bool != nil && (value["issues"] == .null || value["issues"].array != nil)
            && rows.allSatisfy { $0.string != nil }
        name = text("name", 1024) ?? "unnamed skill"
        if wellFormed {
            self.status = status
            offered = value["offered"].bool ?? false
        } else {
            self.status = "rejected"
            offered = false
            issues.append("The Host sent this skill in a shape this app does not recognize; it is shown as rejected.")
        }
        self.issues = issues
        assignableName = offered ? value["name"].string.flatMap { $0.utf8.count <= 1024 ? $0 : nil } : nil
        folder = value["path"].string.flatMap { $0.utf8.count <= 4096 ? URL(fileURLWithPath: $0).lastPathComponent : nil }
    }
}

struct HostSkills: Sendable {
    let skills: [HostSkill]
    let warnings: [String]
    let omitted: UInt64

    init(_ response: JSONValue) throws {
        guard response["kind"].string == "skills", let rows = response["skills"].array, rows.count <= 8192 else { throw OwnerClientError.invalidResponse }
        skills = rows.enumerated().map { HostSkill($1, index: $0) }
        warnings = (response["warnings"].array ?? []).prefix(64).compactMap { $0.string.map { DisplayText.clipped($0, maximum: 1024) } }
        omitted = response["omitted"].unsigned ?? 0
    }
}

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

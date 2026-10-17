import Foundation

struct RemoteSettingsRequestTime: Sendable, Equatable, CustomStringConvertible, CustomDebugStringConvertible {
    let issuedAt: UInt64
    let expiresAt: UInt64

    init(issuedAt: UInt64, expiresAt: UInt64) throws {
        guard issuedAt > 0, issuedAt <= UInt64(Int64.max), expiresAt <= UInt64(Int64.max),
              expiresAt > issuedAt, expiresAt - issuedAt <= 30_000 else { throw WireError.invalid }
        self.issuedAt = issuedAt
        self.expiresAt = expiresAt
    }

    var description: String { "RemoteSettingsRequestTime([redacted])" }
    var debugDescription: String { description }

    fileprivate var wire: WireValue {
        .object(["issued_at": .unsigned(issuedAt), "expires_at": .unsigned(expiresAt)])
    }
}

struct RemoteSettingsGeneration: Sendable, Equatable, CustomStringConvertible, CustomDebugStringConvertible {
    let grantID: UUID
    let grantRevision: UInt64
    let hostEpoch: UUID
    let connectionEpoch: UUID

    init(grantID: UUID, grantRevision: UInt64, hostEpoch: UUID, connectionEpoch: UUID) throws {
        try RemoteSettingsWire.validateIdentifier(grantID)
        try RemoteSettingsWire.validateIdentifier(hostEpoch)
        try RemoteSettingsWire.validateIdentifier(connectionEpoch)
        guard grantRevision > 0, grantRevision <= UInt64(Int64.max) else { throw WireError.invalid }
        self.grantID = grantID
        self.grantRevision = grantRevision
        self.hostEpoch = hostEpoch
        self.connectionEpoch = connectionEpoch
    }

    var description: String { "RemoteSettingsGeneration([redacted])" }
    var debugDescription: String { description }

    fileprivate var wire: WireValue {
        .object([
            "grant_id": .id(grantID), "grant_revision": .unsigned(grantRevision),
            "host_epoch": .id(hostEpoch), "connection_epoch": .id(connectionEpoch)
        ])
    }
}

struct RemoteSettingsParent: Sendable, Equatable, CustomStringConvertible, CustomDebugStringConvertible {
    let parentID: UUID
    let generation: RemoteSettingsGeneration

    fileprivate init(parentID: UUID, generation: RemoteSettingsGeneration) {
        self.parentID = parentID
        self.generation = generation
    }

    var description: String { "RemoteSettingsParent([redacted])" }
    var debugDescription: String { description }
}

enum RemoteSettingsHostAction: String, CaseIterable, Sendable {
    case credentialEntry = "credential_entry"
    case pairingInvitation = "pairing_invitation"
    case pairingApproval = "pairing_approval"
    case deviceRevocation = "device_revocation"
    case grantChange = "grant_change"
    case executionPolicy = "execution_policy"
    case recipientConsent = "recipient_consent"
    case lifecycle
}

enum RemoteSettingsQuery: Sendable, Equatable, CustomStringConvertible, CustomDebugStringConvertible {
    case accessStatus
    case snapshot(keys: [SettingKey])
    case changes(afterSequence: UInt64, limit: Int)
    case update(requestID: UUID, expectedRevision: UInt64, changes: [SettingChange])
    case hostAction(RemoteSettingsHostAction)
    case sessionOpen(openRequestID: UUID)
    case sessionSend(chatID: UUID, turnID: UUID, providerBindingSHA256: String, text: String)
    case sessionPoll(chatID: UUID, textOffset: UInt64)
    case sessionClose(chatID: UUID)

    static let maximumSessionTextBytes = 8 * 1024
    static let maximumTextOffset: UInt64 = 16 * 1024

    var requiresGeneration: Bool {
        switch self {
        case .accessStatus, .hostAction: false
        default: true
        }
    }

    func validate() throws {
        switch self {
        case .accessStatus, .hostAction: break
        case .snapshot(let keys):
            guard !keys.isEmpty, keys.allSatisfy({ $0.scope == .host }),
                  zip(keys, keys.dropFirst()).allSatisfy({ $0.order < $1.order }) else { throw WireError.invalid }
        case .changes(let afterSequence, let limit):
            guard (1...64).contains(limit), afterSequence <= UInt64(Int64.max) else { throw WireError.invalid }
        case .update(let requestID, let expectedRevision, let changes):
            try RemoteSettingsWire.validateIdentifier(requestID)
            guard expectedRevision > 0, expectedRevision <= UInt64(Int64.max), !changes.isEmpty,
                  Set(changes.map(\.key)).count == changes.count,
                  changes.allSatisfy({ $0.key.scope == .host && $0.key.agentMutability == .allowlistedPreference }) else {
                throw WireError.invalid
            }
            for change in changes { try change.validate() }
        case .sessionOpen(let openRequestID):
            try RemoteSettingsWire.validateIdentifier(openRequestID)
        case .sessionSend(let chatID, let turnID, let digest, let text):
            try RemoteSettingsWire.validateIdentifier(chatID)
            try RemoteSettingsWire.validateIdentifier(turnID)
            guard digest.utf8.count == 64,
                  digest.utf8.allSatisfy({ (48...57).contains($0) || (97...102).contains($0) }),
                  !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty, !text.contains("\0"),
                  text.utf8.count <= Self.maximumSessionTextBytes else { throw WireError.invalid }
        case .sessionPoll(let chatID, let textOffset):
            try RemoteSettingsWire.validateIdentifier(chatID)
            guard textOffset <= Self.maximumTextOffset else { throw WireError.invalid }
        case .sessionClose(let chatID):
            try RemoteSettingsWire.validateIdentifier(chatID)
        }
    }

    fileprivate func body(time: RemoteSettingsRequestTime, generation: RemoteSettingsGeneration?) throws -> WireValue {
        var fields: [String: WireValue] = ["time": time.wire]
        if let generation { fields["generation"] = generation.wire }
        switch self {
        case .accessStatus:
            fields["kind"] = .string("access_status")
        case .snapshot(let keys):
            fields["kind"] = .string("snapshot")
            fields["keys"] = .array(keys.map { .string($0.rawValue) })
        case .changes(let afterSequence, let limit):
            fields["kind"] = .string("changes")
            fields["after_sequence"] = .unsigned(afterSequence)
            fields["limit"] = .unsigned(UInt64(limit))
        case .update(let requestID, let expectedRevision, let changes):
            fields["kind"] = .string("update")
            fields["request_id"] = .id(requestID)
            fields["expected_revision"] = .unsigned(expectedRevision)
            fields["changes"] = .array(try changes.map {
                try WireValue.parse(SettingsWire.encode($0), maximum: SettingsWire.patchLimit)
            })
        case .hostAction(let action):
            fields["kind"] = .string("host_action")
            fields["action"] = .string(action.rawValue)
        case .sessionOpen(let openRequestID):
            fields["kind"] = .string("session_open")
            fields["open_request_id"] = .id(openRequestID)
        case .sessionSend(let chatID, let turnID, let digest, let text):
            fields["kind"] = .string("session_send")
            fields["chat_id"] = .id(chatID)
            fields["turn_id"] = .id(turnID)
            fields["provider_binding_sha256"] = .string(digest)
            fields["text"] = .string(text)
        case .sessionPoll(let chatID, let textOffset):
            fields["kind"] = .string("session_poll")
            fields["chat_id"] = .id(chatID)
            fields["text_offset"] = .unsigned(textOffset)
        case .sessionClose(let chatID):
            fields["kind"] = .string("session_close")
            fields["chat_id"] = .id(chatID)
        }
        return .object(fields)
    }

    var description: String { "RemoteSettingsQuery([redacted])" }
    var debugDescription: String { description }
}

struct RemoteSettingsAccessRequest: Sendable, Equatable, CustomStringConvertible, CustomDebugStringConvertible {
    let hostID: UUID
    let queryID: UUID
    let time: RemoteSettingsRequestTime
    let query: RemoteSettingsQuery
    let generation: RemoteSettingsGeneration?

    init(hostID: UUID, queryID: UUID, time: RemoteSettingsRequestTime,
         query: RemoteSettingsQuery = .accessStatus, generation: RemoteSettingsGeneration? = nil) throws {
        try RemoteSettingsWire.validateIdentifier(hostID)
        try RemoteSettingsWire.validateIdentifier(queryID)
        try query.validate()
        guard query.requiresGeneration == (generation != nil) else { throw WireError.invalid }
        self.hostID = hostID
        self.queryID = queryID
        self.time = time
        self.query = query
        self.generation = generation
    }

    func encoded() throws -> Data {
        try RemoteSettingsWire.encodeRequest(hostID: hostID, message: .object([
            "kind": .string("query"), "query_id": .id(queryID),
            "body": try query.body(time: time, generation: generation)
        ]))
    }

    func accepts(_ reply: RemoteSettingsAccessReply) -> Bool {
        guard reply.hostID == hostID, reply.queryID == queryID else { return false }
        switch reply.body {
        case .grantUnavailable, .denied: return true
        default: break
        }
        if let generation, reply.generation != generation { return false }
        switch (query, reply.body) {
        case (.accessStatus, .summary): return true
        case (.snapshot(let keys), .snapshot(let snapshot)):
            return snapshot.scope == .host(hostID: hostID) && snapshot.entries.allSatisfy { keys.contains($0.key) }
        case (.changes(let afterSequence, let limit), .changes(let page)):
            return (try? page.validate(afterSequence: afterSequence, limit: limit)) != nil
                && page.changes.allSatisfy { $0.scope == .host(hostID: hostID) }
        case (.update(let requestID, _, _), .update(let replied, _)): return requestID == replied
        case (.hostAction(let action), .hostActionRequired(let replied)): return action == replied
        case (.sessionOpen, .sessionOpened): return true
        case (.sessionSend(let chatID, let turnID, _, _), .sessionAccepted(let repliedChat, let repliedTurn)):
            return chatID == repliedChat && turnID == repliedTurn
        case (.sessionPoll(let chatID, let textOffset), .sessionStatus(let status)):
            return status.chatID == chatID && status.textOffset == textOffset
        case (.sessionClose(let chatID), .sessionStatus(let status)): return status.chatID == chatID
        default: return false
        }
    }

    var description: String { "RemoteSettingsAccessRequest([redacted])" }
    var debugDescription: String { description }
}

struct RemoteSettingsBindRequest: Sendable, Equatable, CustomStringConvertible, CustomDebugStringConvertible {
    let hostID: UUID
    let generation: RemoteSettingsGeneration
    let bindingRequestID: UUID
    let time: RemoteSettingsRequestTime
    let requestedParentExpiresAt: UInt64

    init(hostID: UUID, generation: RemoteSettingsGeneration, bindingRequestID: UUID,
         time: RemoteSettingsRequestTime, requestedParentExpiresAt: UInt64) throws {
        try RemoteSettingsWire.validateIdentifier(hostID)
        try RemoteSettingsWire.validateIdentifier(bindingRequestID)
        let maximum = time.issuedAt.addingReportingOverflow(300_000)
        guard !maximum.overflow, requestedParentExpiresAt > time.issuedAt,
              requestedParentExpiresAt <= UInt64(Int64.max),
              requestedParentExpiresAt <= maximum.partialValue else { throw WireError.invalid }
        self.hostID = hostID
        self.generation = generation
        self.bindingRequestID = bindingRequestID
        self.time = time
        self.requestedParentExpiresAt = requestedParentExpiresAt
    }

    func encoded() throws -> Data {
        try RemoteSettingsWire.encodeRequest(hostID: hostID, message: .object([
            "kind": .string("bind"), "generation": generation.wire,
            "binding_request_id": .id(bindingRequestID), "time": time.wire,
            "requested_parent_expires_at": .unsigned(requestedParentExpiresAt)
        ]))
    }

    var description: String { "RemoteSettingsBindRequest([redacted])" }
    var debugDescription: String { description }
}

enum RemoteSettingsConsent: Sendable, Equatable, CustomStringConvertible, CustomDebugStringConvertible {
    case none
    case configuredProvider(provider: SettingsProviderKind, model: String, recipient: String)

    var description: String { "RemoteSettingsConsent([redacted])" }
    var debugDescription: String { description }
}

struct RemoteSettingsAccessSummary: Sendable, Equatable, CustomStringConvertible, CustomDebugStringConvertible {
    let generation: RemoteSettingsGeneration
    let expiresAt: UInt64
    let readableKeys: [SettingKey]
    let writableKeys: [SettingKey]
    let sessionConsent: RemoteSettingsConsent
    let configuredSettingsRevision: UInt64?

    fileprivate init(generation: RemoteSettingsGeneration, expiresAt: UInt64,
                     readableKeys: [SettingKey], writableKeys: [SettingKey],
                     sessionConsent: RemoteSettingsConsent, configuredSettingsRevision: UInt64?) {
        self.generation = generation
        self.expiresAt = expiresAt
        self.readableKeys = readableKeys
        self.writableKeys = writableKeys
        self.sessionConsent = sessionConsent
        self.configuredSettingsRevision = configuredSettingsRevision
    }

    var description: String { "RemoteSettingsAccessSummary([redacted])" }
    var debugDescription: String { description }
}

enum RemoteSettingsDenial: String, Sendable, Equatable {
    case grantUnavailable = "grant_unavailable"
    case grantChanged = "grant_changed"
    case outsideScope = "outside_scope"
    case conflict
    case writeBlocked = "write_blocked"
    case capacity
    case invalid
    case outcomeUnknown = "outcome_unknown"
    case unavailable
}

struct RemoteSettingsOperation: Decodable, Sendable, Equatable, CustomStringConvertible, CustomDebugStringConvertible {
    let phase: SettingsOperationPhase
    let updatedAt: UInt64
    let write: SettingsWriteInspection?
    let application: SettingsApplicationState?

    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        try fields.only(["phase", "updated_at", "write", "application"])
        phase = try fields.required(SettingsOperationPhase.self, "phase")
        updatedAt = try fields.required(UInt64.self, "updated_at")
        write = try fields.optional(SettingsWriteInspection.self, "write")
        application = try fields.optional(SettingsApplicationState.self, "application")
        try phase.validate()
        try write?.validate()
        try SettingsWire.timestamp(updatedAt)
        guard updatedAt > 0 else { throw SettingsWireError.invalid }
    }

    var description: String { "RemoteSettingsOperation([redacted])" }
    var debugDescription: String { description }
}

enum RemoteSettingsChatPhase: String, Decodable, Sendable {
    case ready
    case running
    case completed
    case closing
    case closed
    case unavailable
    case stoppedUncertain = "stopped_uncertain"
    case stoppedUnusable = "stopped_unusable"
    case unknown
}

enum RemoteSettingsProvisionalState: String, Decodable, Sendable {
    case streaming
    case interrupted
}

struct RemoteSettingsChatOpened: Decodable, Sendable, Equatable, CustomStringConvertible, CustomDebugStringConvertible {
    let chatID: UUID
    let provider: SettingsProviderKind
    let model: String
    let recipient: String
    let providerBindingSHA256: String
    let expiresAt: UInt64

    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        try fields.only(["chat_id", "provider", "model", "recipient", "provider_binding_sha256", "tools", "expires_at"])
        chatID = try fields.required(UUID.self, "chat_id")
        provider = try fields.required(SettingsProviderKind.self, "provider")
        model = try fields.required(String.self, "model")
        recipient = try fields.required(String.self, "recipient")
        providerBindingSHA256 = try fields.required(String.self, "provider_binding_sha256")
        let tools = try fields.required([String].self, "tools")
        expiresAt = try fields.required(UInt64.self, "expires_at")
        try SettingsWire.identifier(chatID)
        try SettingsWire.digest(providerBindingSHA256)
        try SettingsWire.timestamp(expiresAt)
        try SettingsWire.text(model, maximum: 256)
        try SettingsWire.text(recipient, maximum: 4096)
        guard expiresAt > 0, tools == SettingsToolName.allCases.map(\.providerName) else { throw SettingsWireError.invalid }
    }

    var description: String { "RemoteSettingsChatOpened([private])" }
    var debugDescription: String { description }
}

struct RemoteSettingsChatContext: Decodable, Sendable, Equatable {
    let revision: UInt64
    let observedAt: UInt64
    let stale: Bool

    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        try fields.only(["revision", "observed_at", "stale"])
        revision = try fields.required(UInt64.self, "revision")
        observedAt = try fields.required(UInt64.self, "observed_at")
        stale = try fields.required(Bool.self, "stale")
        try SettingsWire.revision(revision)
        try SettingsWire.timestamp(observedAt)
    }
}

struct RemoteSettingsChatStatus: Decodable, Sendable, Equatable, CustomStringConvertible, CustomDebugStringConvertible {
    let chatID: UUID
    let phase: RemoteSettingsChatPhase
    let turnID: UUID?
    let textRevision: UInt64
    let textOffset: UInt64
    let textBytes: UInt64
    let text: String
    let provisional: RemoteSettingsProvisionalState?
    let context: RemoteSettingsChatContext?
    let operationIDs: [UUID]
    let modelAttempts: UInt32
    let chargedTokens: UInt64
    let reservedOutputTokens: UInt64
    let providerComplete: Bool
    let usageComplete: Bool

    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        try fields.only(["chat_id", "phase", "turn_id", "text_revision", "text_offset", "text_bytes", "text", "provisional", "context", "operation_ids", "model_attempts", "charged_tokens", "reserved_output_tokens", "provider_complete", "usage_complete"])
        chatID = try fields.required(UUID.self, "chat_id")
        phase = try fields.required(RemoteSettingsChatPhase.self, "phase")
        turnID = try fields.optional(UUID.self, "turn_id")
        textRevision = try fields.required(UInt64.self, "text_revision")
        textOffset = try fields.required(UInt64.self, "text_offset")
        textBytes = try fields.required(UInt64.self, "text_bytes")
        text = try fields.required(String.self, "text")
        provisional = try fields.optional(RemoteSettingsProvisionalState.self, "provisional")
        context = try fields.optional(RemoteSettingsChatContext.self, "context")
        operationIDs = try fields.required([UUID].self, "operation_ids")
        modelAttempts = try fields.required(UInt32.self, "model_attempts")
        chargedTokens = try fields.required(UInt64.self, "charged_tokens")
        reservedOutputTokens = try fields.required(UInt64.self, "reserved_output_tokens")
        providerComplete = try fields.required(Bool.self, "provider_complete")
        usageComplete = try fields.required(Bool.self, "usage_complete")
        try SettingsWire.identifier(chatID)
        if let turnID { try SettingsWire.identifier(turnID) }
        for operationID in operationIDs { try SettingsWire.identifier(operationID) }
        let end = textOffset.addingReportingOverflow(UInt64(text.utf8.count))
        guard !end.overflow, end.partialValue <= textBytes, textBytes <= RemoteSettingsQuery.maximumTextOffset,
              textOffset <= RemoteSettingsQuery.maximumTextOffset, !text.contains("\0"),
              operationIDs.count <= 8, Set(operationIDs).count == operationIDs.count,
              modelAttempts <= 4, reservedOutputTokens <= 8192 else { throw SettingsWireError.invalid }
    }

    var description: String { "RemoteSettingsChatStatus([private])" }
    var debugDescription: String { description }
}

enum RemoteSettingsAccessBody: Sendable, Equatable, CustomStringConvertible, CustomDebugStringConvertible {
    case summary(RemoteSettingsAccessSummary)
    case grantUnavailable
    case snapshot(SettingsSnapshot)
    case changes(SettingsChangePage)
    case update(requestID: UUID, operation: RemoteSettingsOperation)
    case hostActionRequired(RemoteSettingsHostAction)
    case sessionOpened(RemoteSettingsChatOpened)
    case sessionAccepted(chatID: UUID, turnID: UUID)
    case sessionStatus(RemoteSettingsChatStatus)
    case denied(RemoteSettingsDenial)

    var description: String { "RemoteSettingsAccessBody([redacted])" }
    var debugDescription: String { description }
}

struct RemoteSettingsAccessReply: Sendable, Equatable, CustomStringConvertible, CustomDebugStringConvertible {
    let hostID: UUID
    let inReplyTo: UUID
    let queryID: UUID
    let generation: RemoteSettingsGeneration?
    let body: RemoteSettingsAccessBody

    fileprivate init(hostID: UUID, inReplyTo: UUID, queryID: UUID,
                     generation: RemoteSettingsGeneration?, body: RemoteSettingsAccessBody) {
        self.hostID = hostID
        self.inReplyTo = inReplyTo
        self.queryID = queryID
        self.generation = generation
        self.body = body
    }

    var description: String { "RemoteSettingsAccessReply([redacted])" }
    var debugDescription: String { description }
}

struct RemoteSettingsBindReply: Sendable, Equatable, CustomStringConvertible, CustomDebugStringConvertible {
    let hostID: UUID
    let inReplyTo: UUID
    let bindingRequestID: UUID
    let parent: RemoteSettingsParent
    let expiresAt: UInt64

    fileprivate init(hostID: UUID, inReplyTo: UUID, bindingRequestID: UUID,
                     parent: RemoteSettingsParent, expiresAt: UInt64) {
        self.hostID = hostID
        self.inReplyTo = inReplyTo
        self.bindingRequestID = bindingRequestID
        self.parent = parent
        self.expiresAt = expiresAt
    }

    var description: String { "RemoteSettingsBindReply([redacted])" }
    var debugDescription: String { description }
}

enum RemoteSettingsWire {
    static func decodeAccessReply(_ plaintext: Data) throws -> RemoteSettingsAccessReply {
        do {
            let response = try responseFields(plaintext)
            let inReplyTo = try identifier(response.message.required("in_reply_to"))
            let correlation = try object(response.message.required("correlation"), keys: ["family", "query_id"])
            guard try correlation.required("family").text == "query" else { throw WireError.invalid }
            let queryID = try identifier(correlation.required("query_id"))
            let generationValue = try response.message.required("generation")
            let body = try response.message.required("body")
            let kind = try body.field("kind").text
            if kind == "denied" {
                let fields = try object(body, keys: ["kind", "reason"])
                guard generationValue == .null,
                      let reason = RemoteSettingsDenial(rawValue: try fields.required("reason").text) else {
                    throw WireError.invalid
                }
                guard plaintext.count <= 2048 else { throw WireError.size }
                return RemoteSettingsAccessReply(hostID: response.hostID, inReplyTo: inReplyTo, queryID: queryID,
                                                 generation: nil,
                                                 body: reason == .grantUnavailable ? .grantUnavailable : .denied(reason))
            }
            let generation = try Self.generation(generationValue)
            let result: RemoteSettingsAccessBody
            switch kind {
            case "access_status":
                let fields = try object(body, keys: ["kind", "summary"])
                let summary = try accessSummary(fields.required("summary"))
                guard generation == summary.generation else { throw WireError.invalid }
                result = .summary(summary)
            case "snapshot":
                let fields = try object(body, keys: ["kind", "snapshot"])
                result = .snapshot(try SettingsWire.decode(SettingsSnapshot.self, from: fields.required("snapshot").encoded(maximum: 32_768)))
            case "changes":
                let fields = try object(body, keys: ["kind", "page"])
                result = .changes(try SettingsWire.decode(SettingsChangePage.self, from: fields.required("page").encoded(maximum: 32_768)))
            case "update":
                let fields = try object(body, keys: ["kind", "request_id", "operation"])
                result = .update(requestID: try identifier(fields.required("request_id")),
                                 operation: try decoded(RemoteSettingsOperation.self, fields.required("operation")))
            case "host_action_required":
                let fields = try object(body, keys: ["kind", "action"])
                guard let action = RemoteSettingsHostAction(rawValue: try fields.required("action").text) else {
                    throw WireError.invalid
                }
                result = .hostActionRequired(action)
            case "session_opened":
                let fields = try object(body, keys: ["kind", "chat"])
                result = .sessionOpened(try decoded(RemoteSettingsChatOpened.self, fields.required("chat")))
            case "session_accepted":
                let fields = try object(body, keys: ["kind", "chat_id", "turn_id"])
                result = .sessionAccepted(chatID: try identifier(fields.required("chat_id")),
                                          turnID: try identifier(fields.required("turn_id")))
            case "session_status":
                let fields = try object(body, keys: ["kind", "status"])
                result = .sessionStatus(try decoded(RemoteSettingsChatStatus.self, fields.required("status")))
            default: throw WireError.invalid
            }
            return RemoteSettingsAccessReply(hostID: response.hostID, inReplyTo: inReplyTo,
                                             queryID: queryID, generation: generation, body: result)
        } catch let error as WireError {
            throw error == .size ? WireError.size : WireError.invalid
        } catch {
            throw WireError.invalid
        }
    }

    static func decodeBindReply(_ plaintext: Data) throws -> RemoteSettingsBindReply {
        do {
            let response = try responseFields(plaintext)
            let inReplyTo = try identifier(response.message.required("in_reply_to"))
            let correlation = try object(response.message.required("correlation"), keys: ["family", "binding_request_id"])
            guard try correlation.required("family").text == "bind" else { throw WireError.invalid }
            let bindingRequestID = try identifier(correlation.required("binding_request_id"))
            let generation = try Self.generation(response.message.required("generation"))
            let body = try object(response.message.required("body"), keys: ["kind", "binding_request_id", "parent", "expires_at"])
            guard try body.required("kind").text == "bound",
                  try identifier(body.required("binding_request_id")) == bindingRequestID else { throw WireError.invalid }
            let fields = try object(body.required("parent"), keys: ["parent_id", "generation"])
            let parentID = try identifier(fields.required("parent_id"))
            let parentGeneration = try Self.generation(fields.required("generation"))
            guard parentGeneration == generation else { throw WireError.invalid }
            let expiresAt = try positiveTimestamp(body.required("expires_at"))
            return RemoteSettingsBindReply(hostID: response.hostID, inReplyTo: inReplyTo,
                                           bindingRequestID: bindingRequestID,
                                           parent: RemoteSettingsParent(parentID: parentID, generation: parentGeneration),
                                           expiresAt: expiresAt)
        } catch let error as WireError {
            throw error == .size ? WireError.size : WireError.invalid
        } catch {
            throw WireError.invalid
        }
    }

    fileprivate static func validateIdentifier(_ value: UUID) throws {
        guard value != WireID.zero else { throw WireError.invalid }
    }

    fileprivate static func encodeRequest(hostID: UUID, message: WireValue) throws -> Data {
        do {
            return try WireValue.object([
                "schema_version": .unsigned(1),
                "payload": .object([
                    "kind": .string("settings"), "version": .unsigned(1),
                    "host_id": .id(hostID), "message": message
                ])
            ]).encoded(maximum: 32_768)
        } catch let error as WireError {
            throw error == .size ? WireError.size : WireError.invalid
        } catch {
            throw WireError.invalid
        }
    }

    private static func responseFields(_ plaintext: Data) throws -> (hostID: UUID, message: [String: WireValue]) {
        let packet = try object(WireValue.parse(plaintext, maximum: 32_768), keys: ["schema_version", "payload"])
        guard try packet.required("schema_version").integer == 1 else { throw WireError.invalid }
        let payload = try object(packet.required("payload"), keys: ["kind", "version", "host_id", "message"])
        guard try payload.required("kind").text == "settings",
              try payload.required("version").integer == 1 else { throw WireError.invalid }
        let hostID = try identifier(payload.required("host_id"))
        let message = try object(payload.required("message"), keys: ["kind", "in_reply_to", "correlation", "generation", "body"])
        guard try message.required("kind").text == "response" else { throw WireError.invalid }
        return (hostID, message)
    }

    private static func object(_ value: WireValue, keys: Set<String>) throws -> [String: WireValue] {
        try value.object(keys: keys, required: keys)
    }

    private static func decoded<Value: Decodable>(_ type: Value.Type, _ value: WireValue) throws -> Value {
        guard case .object = value else { throw WireError.invalid }
        do { return try JSONDecoder().decode(type, from: value.encoded(maximum: 32_768)) }
        catch { throw WireError.invalid }
    }

    private static func identifier(_ value: WireValue) throws -> UUID {
        let text = try value.text
        guard let identifier = UUID(uuidString: text), identifier != WireID.zero,
              identifier.uuidString.lowercased() == text else { throw WireError.invalid }
        return identifier
    }

    private static func positiveTimestamp(_ value: WireValue) throws -> UInt64 {
        let timestamp = try WireID.timestamp(value)
        guard timestamp > 0 else { throw WireError.invalid }
        return timestamp
    }

    private static func generation(_ value: WireValue) throws -> RemoteSettingsGeneration {
        let fields = try object(value, keys: ["grant_id", "grant_revision", "host_epoch", "connection_epoch"])
        return try RemoteSettingsGeneration(
            grantID: identifier(fields.required("grant_id")),
            grantRevision: positiveTimestamp(fields.required("grant_revision")),
            hostEpoch: identifier(fields.required("host_epoch")),
            connectionEpoch: identifier(fields.required("connection_epoch"))
        )
    }

    private static func accessSummary(_ value: WireValue) throws -> RemoteSettingsAccessSummary {
        let fields = try object(value, keys: ["generation", "expires_at", "readable_keys", "writable_keys", "session_consent", "configured_settings_revision"])
        let generation = try Self.generation(fields.required("generation"))
        let expiresAt = try positiveTimestamp(fields.required("expires_at"))
        let readableKeys = try keys(fields.required("readable_keys"), maximum: 5)
        let writableKeys = try keys(fields.required("writable_keys"), maximum: 2)
        guard writableKeys.allSatisfy({ key in
            (key == .embeddingsEnabled || key == .embeddingsDimensions) && readableKeys.contains(key)
        }) else { throw WireError.invalid }
        let consent = try Self.consent(fields.required("session_consent"))
        let configuredRevision = try fields.required("configured_settings_revision")
        let revision: UInt64?
        switch consent {
        case .none:
            guard configuredRevision == .null else { throw WireError.invalid }
            revision = nil
        case .configuredProvider:
            revision = try positiveTimestamp(configuredRevision)
        }
        return RemoteSettingsAccessSummary(generation: generation, expiresAt: expiresAt,
                                           readableKeys: readableKeys, writableKeys: writableKeys,
                                           sessionConsent: consent, configuredSettingsRevision: revision)
    }

    private static func keys(_ value: WireValue, maximum: Int) throws -> [SettingKey] {
        let values = try value.elements
        guard values.count <= maximum else { throw WireError.invalid }
        let keys = try values.map { value in
            guard let key = SettingKey(rawValue: try value.text), key.scope == .host else { throw WireError.invalid }
            return key
        }
        guard zip(keys, keys.dropFirst()).allSatisfy({ $0.order < $1.order }) else { throw WireError.invalid }
        return keys
    }

    private static func consent(_ value: WireValue) throws -> RemoteSettingsConsent {
        switch try value.field("kind").text {
        case "none":
            _ = try object(value, keys: ["kind"])
            return .none
        case "configured_provider":
            let fields = try object(value, keys: ["kind", "provider", "model", "recipient"])
            guard let provider = SettingsProviderKind(rawValue: try fields.required("provider").text) else { throw WireError.invalid }
            let model = try fields.required("model").text
            let recipient = try fields.required("recipient").text
            guard !model.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
                  model.utf8.count <= 256, !model.contains("\0"),
                  recipient.utf8.count <= 4096, !recipient.contains("\0"),
                  let components = URLComponents(string: recipient),
                  components.user == nil, components.password == nil,
                  components.query == nil, components.fragment == nil else { throw WireError.invalid }
            return .configuredProvider(provider: provider, model: model, recipient: recipient)
        default: throw WireError.invalid
        }
    }
}

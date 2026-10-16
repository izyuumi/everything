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

struct RemoteSettingsAccessRequest: Sendable, Equatable, CustomStringConvertible, CustomDebugStringConvertible {
    let hostID: UUID
    let queryID: UUID
    let time: RemoteSettingsRequestTime

    init(hostID: UUID, queryID: UUID, time: RemoteSettingsRequestTime) throws {
        try RemoteSettingsWire.validateIdentifier(hostID)
        try RemoteSettingsWire.validateIdentifier(queryID)
        self.hostID = hostID
        self.queryID = queryID
        self.time = time
    }

    func encoded() throws -> Data {
        try RemoteSettingsWire.encodeRequest(hostID: hostID, message: .object([
            "kind": .string("query"), "query_id": .id(queryID),
            "body": .object(["kind": .string("access_status"), "time": time.wire])
        ]))
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

enum RemoteSettingsAccessBody: Sendable, Equatable, CustomStringConvertible, CustomDebugStringConvertible {
    case summary(RemoteSettingsAccessSummary)
    case grantUnavailable

    var description: String { "RemoteSettingsAccessBody([redacted])" }
    var debugDescription: String { description }
}

struct RemoteSettingsAccessReply: Sendable, Equatable, CustomStringConvertible, CustomDebugStringConvertible {
    let hostID: UUID
    let inReplyTo: UUID
    let queryID: UUID
    let body: RemoteSettingsAccessBody

    fileprivate init(hostID: UUID, inReplyTo: UUID, queryID: UUID, body: RemoteSettingsAccessBody) {
        self.hostID = hostID
        self.inReplyTo = inReplyTo
        self.queryID = queryID
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
            let generation = try response.message.required("generation")
            let body = try response.message.required("body")
            let result: RemoteSettingsAccessBody
            switch try body.field("kind").text {
            case "access_status":
                let fields = try object(body, keys: ["kind", "summary"])
                let summary = try accessSummary(fields.required("summary"))
                guard try Self.generation(generation) == summary.generation else { throw WireError.invalid }
                result = .summary(summary)
            case "denied":
                let fields = try object(body, keys: ["kind", "reason"])
                guard try fields.required("reason").text == "grant_unavailable", generation == .null else {
                    throw WireError.invalid
                }
                guard plaintext.count <= 2048 else { throw WireError.size }
                result = .grantUnavailable
            default: throw WireError.invalid
            }
            return RemoteSettingsAccessReply(hostID: response.hostID, inReplyTo: inReplyTo,
                                             queryID: queryID, body: result)
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

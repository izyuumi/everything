import Foundation

struct RemoteSettingsSnapshotReadRequest: Sendable, Equatable, CustomStringConvertible, CustomDebugStringConvertible {
    let hostID: UUID
    let queryID: UUID
    let parent: RemoteSettingsParent
    let keys: [SettingKey]
    let time: RemoteSettingsRequestTime

    init(hostID: UUID, queryID: UUID, parent: RemoteSettingsParent,
         keys: [SettingKey], time: RemoteSettingsRequestTime) throws {
        try RemoteSettingsReadWire.validateRequest(hostID: hostID, queryID: queryID, parent: parent, time: time)
        try RemoteSettingsReadWire.validateKeys(keys)
        self.hostID = hostID
        self.queryID = queryID
        self.parent = parent
        self.keys = keys
        self.time = time
    }

    func encoded() throws -> Data { try RemoteSettingsReadWire.encode(self) }
    var description: String { "RemoteSettingsSnapshotReadRequest([redacted])" }
    var debugDescription: String { description }
}

struct RemoteSettingsOriginalInspectionRequest: Sendable, Equatable, CustomStringConvertible, CustomDebugStringConvertible {
    let hostID: UUID
    let queryID: UUID
    let parent: RemoteSettingsParent
    let requestID: UUID
    let operationID: UUID?
    let time: RemoteSettingsRequestTime

    init(hostID: UUID, queryID: UUID, parent: RemoteSettingsParent, requestID: UUID,
         operationID: UUID?, time: RemoteSettingsRequestTime) throws {
        try RemoteSettingsReadWire.validateRequest(hostID: hostID, queryID: queryID, parent: parent, time: time)
        try RemoteSettingsReadWire.validateIdentifier(requestID)
        if let operationID { try RemoteSettingsReadWire.validateIdentifier(operationID) }
        guard queryID != requestID else { throw WireError.invalid }
        self.hostID = hostID
        self.queryID = queryID
        self.parent = parent
        self.requestID = requestID
        self.operationID = operationID
        self.time = time
    }

    func encoded() throws -> Data { try RemoteSettingsReadWire.encode(self) }
    var description: String { "RemoteSettingsOriginalInspectionRequest([redacted])" }
    var debugDescription: String { description }
}

struct RemoteSettingsModelSelection: Sendable, Equatable, CustomStringConvertible, CustomDebugStringConvertible {
    let provider: SettingsProviderKind
    let model: String

    fileprivate init(provider: SettingsProviderKind, model: String) {
        self.provider = provider
        self.model = model
    }

    var description: String { "RemoteSettingsModelSelection([redacted])" }
    var debugDescription: String { description }
}

enum RemoteSettingsToolSelection: Sendable, Equatable, CustomStringConvertible, CustomDebugStringConvertible {
    case local
    case systemOne(model: String)

    var description: String { "RemoteSettingsToolSelection([redacted])" }
    var debugDescription: String { description }
}

enum RemoteSettingsValue: Sendable, Equatable, CustomStringConvertible, CustomDebugStringConvertible {
    case model(selection: RemoteSettingsModelSelection?)
    case toolSelection(selection: RemoteSettingsToolSelection)
    case boolean(value: Bool)
    case dimensions(value: UInt32?)
    case embeddingModel(model: String?)

    var description: String { "RemoteSettingsValue([redacted])" }
    var debugDescription: String { description }
}

struct RemoteSettingsEntry: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    let key: SettingKey
    let defaultValue: RemoteSettingsValue
    let desired: RemoteSettingsValue?
    let effective: RemoteSettingsValue?
    let state: SettingState
    let unavailableReason: SettingsUnavailableReason?

    fileprivate init(key: SettingKey, defaultValue: RemoteSettingsValue,
                     desired: RemoteSettingsValue?, effective: RemoteSettingsValue?,
                     state: SettingState, unavailableReason: SettingsUnavailableReason?) {
        self.key = key
        self.defaultValue = defaultValue
        self.desired = desired
        self.effective = effective
        self.state = state
        self.unavailableReason = unavailableReason
    }

    var description: String { "RemoteSettingsEntry([redacted])" }
    var debugDescription: String { description }
}

struct RemoteSettingsHostSnapshot: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    let revision: UInt64
    let observedAt: UInt64
    let stale: Bool
    let entries: [RemoteSettingsEntry]

    fileprivate init(revision: UInt64, observedAt: UInt64, stale: Bool, entries: [RemoteSettingsEntry]) {
        self.revision = revision
        self.observedAt = observedAt
        self.stale = stale
        self.entries = entries
    }

    var description: String { "RemoteSettingsHostSnapshot([redacted])" }
    var debugDescription: String { description }
}

enum RemoteSettingsOriginalKind: String, Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    case chatOpen = "chat_open"
    case send
    case update

    var description: String { "RemoteSettingsOriginalKind([redacted])" }
    var debugDescription: String { description }
}

enum RemoteSettingsOriginalTerminal: String, Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    case completed
    case denied
    case suppressed
    case retiredLostRAM = "retired_lost_ram"
    case unknown

    var description: String { "RemoteSettingsOriginalTerminal([redacted])" }
    var debugDescription: String { description }
}

enum RemoteSettingsOriginalLink: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    case unlinked
    case chat(chatID: UUID)
    case turn(chatID: UUID, turnID: UUID)
    case operation(operationID: UUID)

    var description: String { "RemoteSettingsOriginalLink([redacted])" }
    var debugDescription: String { description }
}

enum RemoteSettingsOperationState: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    case applied(revision: UInt64)
    case pendingApplication(revision: UInt64)
    case restartRequired(revision: UInt64)
    case conflict(observedRevision: UInt64)
    case hostActionRequired
    case unavailable(reason: SettingsUnavailableReason)
    case failed(reason: SettingsFailureReason)
    case unknown
    case cancelled

    var description: String { "RemoteSettingsOperationState([redacted])" }
    var debugDescription: String { description }
}

enum RemoteSettingsOperationPhase: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    case applying(expectedRevision: UInt64, revision: UInt64)
    case finished(state: RemoteSettingsOperationState)

    var description: String { "RemoteSettingsOperationPhase([redacted])" }
    var debugDescription: String { description }
}

struct RemoteSettingsServiceReceipt: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    let operationID: UUID
    let expectedRevision: UInt64?
    let updatedAt: UInt64
    let phase: RemoteSettingsOperationPhase
    let write: SettingsWriteInspection?
    let application: SettingsApplicationState?

    fileprivate init(operationID: UUID, expectedRevision: UInt64?, updatedAt: UInt64,
                     phase: RemoteSettingsOperationPhase, write: SettingsWriteInspection?,
                     application: SettingsApplicationState?) {
        self.operationID = operationID
        self.expectedRevision = expectedRevision
        self.updatedAt = updatedAt
        self.phase = phase
        self.write = write
        self.application = application
    }

    var description: String { "RemoteSettingsServiceReceipt([redacted])" }
    var debugDescription: String { description }
}

enum RemoteSettingsServiceLookup: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    case notRequested
    case notRecorded
    case recorded(receipt: RemoteSettingsServiceReceipt)

    var description: String { "RemoteSettingsServiceLookup([redacted])" }
    var debugDescription: String { description }
}

struct RemoteSettingsOriginalView: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    let originalKind: RemoteSettingsOriginalKind
    let terminal: RemoteSettingsOriginalTerminal?
    let link: RemoteSettingsOriginalLink
    let mutationOperationIDs: [UUID]
    let service: RemoteSettingsServiceLookup

    fileprivate init(originalKind: RemoteSettingsOriginalKind, terminal: RemoteSettingsOriginalTerminal?,
                     link: RemoteSettingsOriginalLink, mutationOperationIDs: [UUID],
                     service: RemoteSettingsServiceLookup) {
        self.originalKind = originalKind
        self.terminal = terminal
        self.link = link
        self.mutationOperationIDs = mutationOperationIDs
        self.service = service
    }

    var description: String { "RemoteSettingsOriginalView([redacted])" }
    var debugDescription: String { description }
}

enum RemoteSettingsOriginalInspectionLookup: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    case notRecorded
    case recorded(originalGeneration: RemoteSettingsGeneration, view: RemoteSettingsOriginalView)

    var description: String { "RemoteSettingsOriginalInspectionLookup([redacted])" }
    var debugDescription: String { description }
}

enum RemoteSettingsReadDenial: String, Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    case grantUnavailable = "grant_unavailable"
    case outsideScope = "outside_scope"

    var description: String { "RemoteSettingsReadDenial([redacted])" }
    var debugDescription: String { description }
}

enum RemoteSettingsReadBody: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    case snapshot(RemoteSettingsHostSnapshot)
    case originalInspection(requestID: UUID, operationID: UUID?, observedAt: UInt64,
                            lookup: RemoteSettingsOriginalInspectionLookup)
    case denied(RemoteSettingsReadDenial)

    var description: String { "RemoteSettingsReadBody([redacted])" }
    var debugDescription: String { description }
}

struct RemoteSettingsReadReply: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    let hostID: UUID
    let inReplyTo: UUID
    let queryID: UUID
    let generation: RemoteSettingsGeneration?
    let body: RemoteSettingsReadBody

    fileprivate init(hostID: UUID, inReplyTo: UUID, queryID: UUID,
                     generation: RemoteSettingsGeneration?, body: RemoteSettingsReadBody) {
        self.hostID = hostID
        self.inReplyTo = inReplyTo
        self.queryID = queryID
        self.generation = generation
        self.body = body
    }

    var description: String { "RemoteSettingsReadReply([redacted])" }
    var debugDescription: String { description }
}

enum RemoteSettingsReadWire {
    private enum Expectation {
        case snapshot(RemoteSettingsSnapshotReadRequest)
        case inspection(RemoteSettingsOriginalInspectionRequest)

        var hostID: UUID {
            switch self {
            case .snapshot(let request): request.hostID
            case .inspection(let request): request.hostID
            }
        }

        var queryID: UUID {
            switch self {
            case .snapshot(let request): request.queryID
            case .inspection(let request): request.queryID
            }
        }

        var generation: RemoteSettingsGeneration {
            switch self {
            case .snapshot(let request): request.parent.generation
            case .inspection(let request): request.parent.generation
            }
        }

        var packetLimit: Int {
            switch self {
            case .snapshot: 32_768
            case .inspection: 65_536
            }
        }
    }

    static func decodeReply(_ data: Data, matching request: RemoteSettingsSnapshotReadRequest) throws -> RemoteSettingsReadReply {
        try decodeReply(data, expectation: .snapshot(request))
    }

    static func decodeReply(_ data: Data, matching request: RemoteSettingsOriginalInspectionRequest) throws -> RemoteSettingsReadReply {
        try decodeReply(data, expectation: .inspection(request))
    }

    fileprivate static func validateIdentifier(_ value: UUID) throws {
        guard value != WireID.zero else { throw WireError.invalid }
    }

    fileprivate static func validateRequest(hostID: UUID, queryID: UUID, parent: RemoteSettingsParent,
                                            time: RemoteSettingsRequestTime) throws {
        try validateIdentifier(hostID)
        try validateIdentifier(queryID)
        try validateIdentifier(parent.parentID)
        try validateIdentifier(parent.generation.grantID)
        try validateIdentifier(parent.generation.hostEpoch)
        try validateIdentifier(parent.generation.connectionEpoch)
        guard parent.generation.grantRevision > 0, parent.generation.grantRevision <= UInt64(Int64.max),
              time.issuedAt > 0, time.issuedAt <= UInt64(Int64.max), time.expiresAt <= UInt64(Int64.max),
              time.expiresAt > time.issuedAt, time.expiresAt - time.issuedAt <= 30_000 else { throw WireError.invalid }
    }

    fileprivate static func validateKeys(_ keys: [SettingKey]) throws {
        guard !keys.isEmpty, keys.count <= 5, keys.allSatisfy({ $0.scope == .host }),
              zip(keys, keys.dropFirst()).allSatisfy({ $0.order < $1.order }) else { throw WireError.invalid }
    }

    fileprivate static func encode(_ request: RemoteSettingsSnapshotReadRequest) throws -> Data {
        try validateRequest(hostID: request.hostID, queryID: request.queryID, parent: request.parent, time: request.time)
        try validateKeys(request.keys)
        return try encodeRequest(hostID: request.hostID, queryID: request.queryID, body: .object([
            "kind": .string("snapshot"), "parent": parentWire(request.parent),
            "keys": .array(request.keys.map { .string($0.rawValue) }), "time": timeWire(request.time)
        ]))
    }

    fileprivate static func encode(_ request: RemoteSettingsOriginalInspectionRequest) throws -> Data {
        try validateRequest(hostID: request.hostID, queryID: request.queryID, parent: request.parent, time: request.time)
        try validateIdentifier(request.requestID)
        if let operationID = request.operationID { try validateIdentifier(operationID) }
        guard request.queryID != request.requestID else { throw WireError.invalid }
        return try encodeRequest(hostID: request.hostID, queryID: request.queryID, body: .object([
            "kind": .string("inspect_original"), "parent": parentWire(request.parent),
            "request_id": .id(request.requestID), "operation_id": request.operationID.map { .id($0) } ?? .null,
            "time": timeWire(request.time)
        ]))
    }

    private static func encodeRequest(hostID: UUID, queryID: UUID, body: WireValue) throws -> Data {
        do {
            return try WireValue.object([
                "schema_version": .unsigned(1),
                "payload": .object([
                    "kind": .string("settings"), "version": .unsigned(1), "host_id": .id(hostID),
                    "message": .object(["kind": .string("query"), "query_id": .id(queryID), "body": body])
                ])
            ]).encoded(maximum: 32_768)
        } catch let error as WireError {
            throw error == .size ? WireError.size : WireError.invalid
        } catch {
            throw WireError.invalid
        }
    }

    private static func parentWire(_ parent: RemoteSettingsParent) -> WireValue {
        .object([
            "parent_id": .id(parent.parentID),
            "generation": .object([
                "grant_id": .id(parent.generation.grantID), "grant_revision": .unsigned(parent.generation.grantRevision),
                "host_epoch": .id(parent.generation.hostEpoch), "connection_epoch": .id(parent.generation.connectionEpoch)
            ])
        ])
    }

    private static func timeWire(_ time: RemoteSettingsRequestTime) -> WireValue {
        .object(["issued_at": .unsigned(time.issuedAt), "expires_at": .unsigned(time.expiresAt)])
    }

    private static func decodeReply(_ data: Data, expectation: Expectation) throws -> RemoteSettingsReadReply {
        do {
            let packet = try object(WireValue.parse(data, maximum: expectation.packetLimit), ["schema_version", "payload"])
            guard try packet.required("schema_version").integer == 1 else { throw WireError.invalid }
            let payload = try object(packet.required("payload"), ["kind", "version", "host_id", "message"])
            guard try payload.required("kind").text == "settings", try payload.required("version").integer == 1 else {
                throw WireError.invalid
            }
            let hostID = try identifier(payload.required("host_id"))
            let response = try object(payload.required("message"), ["kind", "in_reply_to", "correlation", "generation", "body"])
            guard try response.required("kind").text == "response" else { throw WireError.invalid }
            let inReplyTo = try identifier(response.required("in_reply_to"))
            let correlation = try object(response.required("correlation"), ["family", "query_id"])
            guard try correlation.required("family").text == "query" else { throw WireError.invalid }
            let queryID = try identifier(correlation.required("query_id"))
            guard hostID == expectation.hostID, queryID == expectation.queryID, queryID != inReplyTo else {
                throw WireError.invalid
            }
            let bodyValue = try response.required("body")
            let bodyKind = try bodyValue.field("kind").text
            let generationValue = try response.required("generation")
            let generation: RemoteSettingsGeneration?
            let body: RemoteSettingsReadBody
            switch bodyKind {
            case "denied":
                let fields = try object(bodyValue, ["kind", "reason"])
                guard data.count <= 2048 else { throw WireError.size }
                guard generationValue == .null,
                      let reason = RemoteSettingsReadDenial(rawValue: try fields.required("reason").text) else {
                    throw WireError.invalid
                }
                generation = nil
                body = .denied(reason)
            case "snapshot":
                guard case .snapshot(let request) = expectation else { throw WireError.invalid }
                generation = try decodeGeneration(generationValue)
                guard generation == request.parent.generation else { throw WireError.invalid }
                let fields = try object(bodyValue, ["kind", "snapshot"])
                let snapshot = try decodeSnapshot(fields.required("snapshot"))
                guard snapshot.observedAt >= request.time.issuedAt, snapshot.observedAt < request.time.expiresAt,
                      snapshot.entries.map(\.key) == request.keys else { throw WireError.invalid }
                body = .snapshot(snapshot)
            case "original_inspection":
                guard case .inspection(let request) = expectation else { throw WireError.invalid }
                generation = try decodeGeneration(generationValue)
                guard generation == request.parent.generation else { throw WireError.invalid }
                let fields = try object(bodyValue, ["kind", "request_id", "operation_id", "observed_at", "lookup"])
                let requestID = try identifier(fields.required("request_id"))
                let operationID = try optionalIdentifier(fields.required("operation_id"))
                guard requestID == request.requestID, operationID == request.operationID else { throw WireError.invalid }
                let observedAt = try positive(fields.required("observed_at"))
                guard observedAt >= request.time.issuedAt, observedAt < request.time.expiresAt else { throw WireError.invalid }
                let lookup = try decodeOriginalLookup(fields.required("lookup"))
                if case .recorded(_, let view) = lookup {
                    try matchInspection(view, requestID: request.requestID, operationID: request.operationID)
                }
                body = .originalInspection(requestID: requestID, operationID: operationID, observedAt: observedAt, lookup: lookup)
            default: throw WireError.invalid
            }
            return RemoteSettingsReadReply(hostID: hostID, inReplyTo: inReplyTo, queryID: queryID,
                                           generation: generation, body: body)
        } catch let error as WireError {
            throw error == .size ? WireError.size : WireError.invalid
        } catch {
            throw WireError.invalid
        }
    }

    private static func object(_ value: WireValue, _ keys: Set<String>) throws -> [String: WireValue] {
        try value.object(keys: keys, required: keys)
    }

    private static func identifier(_ value: WireValue) throws -> UUID {
        let text = try value.text
        guard let identifier = UUID(uuidString: text), identifier != WireID.zero,
              identifier.uuidString.lowercased() == text else { throw WireError.invalid }
        return identifier
    }

    private static func optionalIdentifier(_ value: WireValue) throws -> UUID? {
        if value == .null { return nil }
        return try identifier(value)
    }

    private static func positive(_ value: WireValue) throws -> UInt64 {
        let value = try WireID.timestamp(value)
        guard value > 0 else { throw WireError.invalid }
        return value
    }

    private static func optionalPositive(_ value: WireValue) throws -> UInt64? {
        if value == .null { return nil }
        return try positive(value)
    }

    private static func decodeGeneration(_ value: WireValue) throws -> RemoteSettingsGeneration {
        let fields = try object(value, ["grant_id", "grant_revision", "host_epoch", "connection_epoch"])
        return try RemoteSettingsGeneration(grantID: identifier(fields.required("grant_id")),
                                             grantRevision: positive(fields.required("grant_revision")),
                                             hostEpoch: identifier(fields.required("host_epoch")),
                                             connectionEpoch: identifier(fields.required("connection_epoch")))
    }

    private static func modelText(_ value: WireValue) throws -> String {
        let text = try value.text
        guard !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              text.utf8.count <= 256, !text.contains("\0") else { throw WireError.invalid }
        return text
    }

    private static func decodeValue(_ value: WireValue) throws -> RemoteSettingsValue {
        switch try value.field("kind").text {
        case "model":
            let fields = try object(value, ["kind", "selection"])
            let selection = try fields.required("selection")
            if selection == .null { return .model(selection: nil) }
            let model = try object(selection, ["provider", "model"])
            guard let provider = SettingsProviderKind(rawValue: try model.required("provider").text) else {
                throw WireError.invalid
            }
            return .model(selection: RemoteSettingsModelSelection(provider: provider, model: try modelText(model.required("model"))))
        case "tool_selection":
            let fields = try object(value, ["kind", "selection"])
            let selection = try fields.required("selection")
            switch try selection.field("kind").text {
            case "local":
                _ = try object(selection, ["kind"])
                return .toolSelection(selection: .local)
            case "system_one":
                let selected = try selection.object(keys: ["kind", "provider", "model"], required: ["kind", "model"])
                if let provider = selected["provider"] {
                    let text = try provider.text
                    guard !text.isEmpty, text.utf8.count <= 64, !text.unicodeScalars.contains(where: { $0.properties.generalCategory == .control }) else { throw WireError.invalid }
                }
                return .toolSelection(selection: .systemOne(model: try modelText(selected.required("model"))))
            default: throw WireError.invalid
            }
        case "boolean":
            let fields = try object(value, ["kind", "value"])
            return .boolean(value: try fields.required("value").flag)
        case "dimensions":
            let fields = try object(value, ["kind", "value"])
            let selected = try fields.required("value")
            if selected == .null { return .dimensions(value: nil) }
            let dimensions = try positive(selected)
            guard dimensions <= 65_536 else { throw WireError.invalid }
            return .dimensions(value: UInt32(dimensions))
        case "embedding_model":
            let fields = try object(value, ["kind", "model"])
            let model = try fields.required("model")
            if model == .null { return .embeddingModel(model: nil) }
            return .embeddingModel(model: try modelText(model))
        default: throw WireError.invalid
        }
    }

    private static func optionalValue(_ value: WireValue) throws -> RemoteSettingsValue? {
        if value == .null { return nil }
        return try decodeValue(value)
    }

    private static func defaultValue(_ key: SettingKey) throws -> RemoteSettingsValue {
        switch key {
        case .modelSelection: .model(selection: nil)
        case .toolSelection: .toolSelection(selection: .local)
        case .embeddingsEnabled: .boolean(value: false)
        case .embeddingsDimensions: .dimensions(value: nil)
        case .embeddingsModel: .embeddingModel(model: nil)
        default: throw WireError.invalid
        }
    }

    private static func matchesValue(_ value: RemoteSettingsValue, key: SettingKey) -> Bool {
        switch (key, value) {
        case (.modelSelection, .model), (.toolSelection, .toolSelection),
             (.embeddingsEnabled, .boolean), (.embeddingsDimensions, .dimensions), (.embeddingsModel, .embeddingModel): true
        default: false
        }
    }

    private static func decodeEntry(_ value: WireValue) throws -> RemoteSettingsEntry {
        let fields = try object(value, ["key", "default", "desired", "effective", "state", "unavailable_reason"])
        guard let key = SettingKey(rawValue: try fields.required("key").text), key.scope == .host,
              let state = SettingState(rawValue: try fields.required("state").text) else { throw WireError.invalid }
        let defaults = try decodeValue(fields.required("default"))
        let desired = try optionalValue(fields.required("desired"))
        let effective = try optionalValue(fields.required("effective"))
        guard defaults == (try defaultValue(key)),
              desired.map({ matchesValue($0, key: key) }) ?? true,
              effective.map({ matchesValue($0, key: key) }) ?? true,
              key != .embeddingsModel || desired == nil else { throw WireError.invalid }
        if state == .effective {
            guard let effective, desired == nil || desired == effective else { throw WireError.invalid }
        }
        let reasonValue = try fields.required("unavailable_reason")
        let reason: SettingsUnavailableReason?
        if reasonValue == .null { reason = nil }
        else {
            guard let decoded = SettingsUnavailableReason(rawValue: try reasonValue.text) else { throw WireError.invalid }
            reason = decoded
        }
        guard (state == .unavailable) == (reason != nil) else { throw WireError.invalid }
        return RemoteSettingsEntry(key: key, defaultValue: defaults, desired: desired, effective: effective,
                                   state: state, unavailableReason: reason)
    }

    private static func decodeSnapshot(_ value: WireValue) throws -> RemoteSettingsHostSnapshot {
        let fields = try object(value, ["revision", "observed_at", "stale", "entries"])
        let values = try fields.required("entries").elements
        guard !values.isEmpty, values.count <= 5 else { throw WireError.invalid }
        let entries = try values.map(decodeEntry)
        try validateKeys(entries.map(\.key))
        return try RemoteSettingsHostSnapshot(revision: positive(fields.required("revision")),
                                               observedAt: positive(fields.required("observed_at")),
                                               stale: fields.required("stale").flag, entries: entries)
    }

    private static func decodeOriginalLookup(_ value: WireValue) throws -> RemoteSettingsOriginalInspectionLookup {
        switch try value.field("state").text {
        case "not_recorded":
            _ = try object(value, ["state"])
            return .notRecorded
        case "recorded":
            let fields = try object(value, ["state", "original_generation", "view"])
            return try .recorded(originalGeneration: decodeGeneration(fields.required("original_generation")),
                                 view: decodeOriginalView(fields.required("view")))
        default: throw WireError.invalid
        }
    }

    static func decodeOriginalView(_ value: WireValue) throws -> RemoteSettingsOriginalView {
        _ = try value.encoded(maximum: 4096)
        let fields = try object(value, ["original_kind", "terminal", "link", "mutation_operation_ids", "service"])
        guard let kind = RemoteSettingsOriginalKind(rawValue: try fields.required("original_kind").text) else {
            throw WireError.invalid
        }
        let terminalValue = try fields.required("terminal")
        let terminal: RemoteSettingsOriginalTerminal?
        if terminalValue == .null { terminal = nil }
        else {
            let terminalFields = try object(terminalValue, ["kind"])
            guard let decoded = RemoteSettingsOriginalTerminal(rawValue: try terminalFields.required("kind").text) else {
                throw WireError.invalid
            }
            terminal = decoded
        }
        let ids = try fields.required("mutation_operation_ids").elements
        guard ids.count <= 8 else { throw WireError.invalid }
        let mutations = try ids.map(identifier)
        guard zip(mutations, mutations.dropFirst()).allSatisfy({ WireID.bytes($0).lexicographicallyPrecedes(WireID.bytes($1)) }) else {
            throw WireError.invalid
        }
        let view = try RemoteSettingsOriginalView(originalKind: kind, terminal: terminal,
                                                   link: decodeOriginalLink(fields.required("link")),
                                                   mutationOperationIDs: mutations,
                                                   service: decodeService(fields.required("service")))
        try validateOriginalView(view)
        return view
    }

    private static func decodeOriginalLink(_ value: WireValue) throws -> RemoteSettingsOriginalLink {
        switch try value.field("state").text {
        case "unlinked":
            _ = try object(value, ["state"])
            return .unlinked
        case "chat":
            let fields = try object(value, ["state", "chat_id"])
            return try .chat(chatID: identifier(fields.required("chat_id")))
        case "turn":
            let fields = try object(value, ["state", "chat_id", "turn_id"])
            return try .turn(chatID: identifier(fields.required("chat_id")), turnID: identifier(fields.required("turn_id")))
        case "operation":
            let fields = try object(value, ["state", "operation_id"])
            return try .operation(operationID: identifier(fields.required("operation_id")))
        default: throw WireError.invalid
        }
    }

    private static func decodeService(_ value: WireValue) throws -> RemoteSettingsServiceLookup {
        switch try value.field("state").text {
        case "not_requested":
            _ = try object(value, ["state"])
            return .notRequested
        case "not_recorded":
            _ = try object(value, ["state"])
            return .notRecorded
        case "recorded":
            let fields = try object(value, ["state", "receipt"])
            return try .recorded(receipt: decodeReceipt(fields.required("receipt")))
        default: throw WireError.invalid
        }
    }

    private static func decodeReceipt(_ value: WireValue) throws -> RemoteSettingsServiceReceipt {
        _ = try value.encoded(maximum: 2048)
        let fields = try object(value, ["operation_id", "expected_revision", "updated_at", "phase", "write", "application"])
        let writeValue = try fields.required("write")
        let applicationValue = try fields.required("application")
        let receipt = try RemoteSettingsServiceReceipt(
            operationID: identifier(fields.required("operation_id")),
            expectedRevision: optionalPositive(fields.required("expected_revision")),
            updatedAt: positive(fields.required("updated_at")), phase: decodePhase(fields.required("phase")),
            write: writeValue == .null ? nil : decodeWrite(writeValue),
            application: applicationValue == .null ? nil : decodeApplication(applicationValue)
        )
        try validateReceipt(receipt)
        return receipt
    }

    private static func decodePhase(_ value: WireValue) throws -> RemoteSettingsOperationPhase {
        switch try value.field("phase").text {
        case "applying":
            let fields = try object(value, ["phase", "expected_revision", "revision"])
            return try .applying(expectedRevision: positive(fields.required("expected_revision")), revision: positive(fields.required("revision")))
        case "finished":
            let fields = try object(value, ["phase", "state"])
            return try .finished(state: decodeOperationState(fields.required("state")))
        default: throw WireError.invalid
        }
    }

    private static func decodeOperationState(_ value: WireValue) throws -> RemoteSettingsOperationState {
        switch try value.field("kind").text {
        case "applied", "pending_application", "restart_required":
            let fields = try object(value, ["kind", "revision"])
            let revision = try positive(fields.required("revision"))
            switch try fields.required("kind").text {
            case "applied": return .applied(revision: revision)
            case "pending_application": return .pendingApplication(revision: revision)
            default: return .restartRequired(revision: revision)
            }
        case "conflict":
            let fields = try object(value, ["kind", "observed_revision"])
            return try .conflict(observedRevision: positive(fields.required("observed_revision")))
        case "unavailable":
            let fields = try object(value, ["kind", "reason"])
            guard let reason = SettingsUnavailableReason(rawValue: try fields.required("reason").text) else { throw WireError.invalid }
            return .unavailable(reason: reason)
        case "failed":
            let fields = try object(value, ["kind", "reason"])
            guard let reason = SettingsFailureReason(rawValue: try fields.required("reason").text) else { throw WireError.invalid }
            return .failed(reason: reason)
        case "host_action_required", "unknown", "cancelled":
            let fields = try object(value, ["kind"])
            switch try fields.required("kind").text {
            case "host_action_required": return .hostActionRequired
            case "unknown": return .unknown
            default: return .cancelled
            }
        default: throw WireError.invalid
        }
    }

    private static func decodeWrite(_ value: WireValue) throws -> SettingsWriteInspection {
        let fields = try object(value, ["expected_revision", "revision", "changed_keys", "restart_required", "resolution"])
        let keys = try fields.required("changed_keys").elements
        guard !keys.isEmpty, keys.count <= 2 else { throw WireError.invalid }
        let changedKeys = try keys.map { value in
            guard let key = SettingKey(rawValue: try value.text), key == .embeddingsEnabled || key == .embeddingsDimensions else {
                throw WireError.invalid
            }
            return key
        }
        try validateKeys(changedKeys)
        let resolutionValue = try fields.required("resolution")
        let resolution: SettingsWriteResolution?
        if resolutionValue == .null { resolution = nil }
        else {
            guard let decoded = SettingsWriteResolution(rawValue: try resolutionValue.text) else { throw WireError.invalid }
            resolution = decoded
        }
        let expected = try positive(fields.required("expected_revision"))
        let revision = try positive(fields.required("revision"))
        guard expected < UInt64(Int64.max), revision == expected + 1 else { throw WireError.invalid }
        return try SettingsWriteInspection(expectedRevision: expected, revision: revision, changedKeys: changedKeys,
                                            restartRequired: fields.required("restart_required").flag, resolution: resolution)
    }

    private static func decodeApplication(_ value: WireValue) throws -> SettingsApplicationState {
        switch try value.field("state").text {
        case "applied":
            _ = try object(value, ["state"])
            return .applied
        case "restart_required":
            _ = try object(value, ["state"])
            return .restartRequired
        case "unavailable":
            let fields = try object(value, ["state", "reason"])
            guard let reason = SettingsUnavailableReason(rawValue: try fields.required("reason").text) else { throw WireError.invalid }
            return .unavailable(reason: reason)
        case "failed":
            let fields = try object(value, ["state", "reason"])
            guard let reason = SettingsFailureReason(rawValue: try fields.required("reason").text) else { throw WireError.invalid }
            return .failed(reason: reason)
        default: throw WireError.invalid
        }
    }

    private static func validateReceipt(_ receipt: RemoteSettingsServiceReceipt) throws {
        switch receipt.phase {
        case .applying(let expected, let revision):
            guard let write = receipt.write, write.resolution == nil,
                  write.expectedRevision == expected, write.revision == revision else { throw WireError.invalid }
        case .finished(let state):
            switch state {
            case .applied, .pendingApplication, .restartRequired:
                guard receipt.write != nil else { throw WireError.invalid }
            default: break
            }
        }
        if let write = receipt.write {
            guard receipt.expectedRevision == write.expectedRevision else { throw WireError.invalid }
            let valid: Bool
            switch (receipt.phase, write.resolution) {
            case (.applying, nil), (.finished(.unknown), nil): valid = true
            case (.finished(.applied(let revision)), .some(.applied)),
                 (.finished(.pendingApplication(let revision)), .some(.applied)),
                 (.finished(.restartRequired(let revision)), .some(.applied)):
                valid = revision == write.revision
            case (.finished(.failed), .some(.notApplied)), (.finished(.unavailable), .some(.notApplied)),
                 (.finished(.cancelled), .some(.notApplied)), (.finished(.conflict), .some(.notApplied)):
                valid = true
            default: valid = false
            }
            guard valid else { throw WireError.invalid }
        }
        if receipt.application != nil {
            guard case .finished(.pendingApplication) = receipt.phase,
                  receipt.write?.resolution == .applied else { throw WireError.invalid }
        }
    }

    private static func validateOriginalView(_ view: RemoteSettingsOriginalView) throws {
        switch (view.originalKind, view.link) {
        case (.chatOpen, .unlinked), (.chatOpen, .chat), (.send, .unlinked), (.send, .turn),
             (.update, .unlinked), (.update, .operation): break
        default: throw WireError.invalid
        }
        switch view.link {
        case .unlinked, .chat:
            guard view.mutationOperationIDs.isEmpty, case .notRequested = view.service else { throw WireError.invalid }
        case .turn:
            switch view.service {
            case .notRequested: break
            case .notRecorded:
                guard !view.mutationOperationIDs.isEmpty else { throw WireError.invalid }
            case .recorded(let receipt):
                guard view.mutationOperationIDs.contains(receipt.operationID) else { throw WireError.invalid }
            }
        case .operation(let operationID):
            guard view.mutationOperationIDs.isEmpty else { throw WireError.invalid }
            switch view.service {
            case .notRequested: throw WireError.invalid
            case .notRecorded: break
            case .recorded(let receipt):
                guard receipt.operationID == operationID else { throw WireError.invalid }
            }
        }
    }

    private static func matchInspection(_ view: RemoteSettingsOriginalView, requestID: UUID, operationID: UUID?) throws {
        if view.originalKind == .update, case .operation(let linkedID) = view.link {
            guard linkedID == requestID else { throw WireError.invalid }
        }
        if let operationID {
            guard view.originalKind == .send, view.mutationOperationIDs.contains(operationID) else { throw WireError.invalid }
            switch view.service {
            case .notRequested: throw WireError.invalid
            case .notRecorded: break
            case .recorded(let receipt):
                guard receipt.operationID == operationID else { throw WireError.invalid }
            }
        } else if view.originalKind == .send {
            guard case .notRequested = view.service else { throw WireError.invalid }
        }
    }
}

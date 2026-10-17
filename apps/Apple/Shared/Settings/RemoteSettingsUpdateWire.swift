import Foundation

enum RemoteSettingsUpdateDimensions: Sendable, Equatable, CustomStringConvertible, CustomDebugStringConvertible {
    case backendDefault
    case selected(dimensions: UInt32)

    var description: String { "RemoteSettingsUpdateDimensions([redacted])" }
    var debugDescription: String { description }
}

struct RemoteSettingsUpdateRequest: Sendable, Equatable, CustomStringConvertible, CustomDebugStringConvertible {
    let hostID: UUID
    let parent: RemoteSettingsParent
    let requestID: UUID
    let time: RemoteSettingsRequestTime
    let expectedSettingsRevision: UInt64
    let enabled: Bool?
    let dimensions: RemoteSettingsUpdateDimensions?

    init(hostID: UUID, parent: RemoteSettingsParent, requestID: UUID, time: RemoteSettingsRequestTime,
         expectedSettingsRevision: UInt64, enabled: Bool?, dimensions: RemoteSettingsUpdateDimensions?) throws {
        try RemoteSettingsUpdateWire.validateRequest(hostID: hostID, parent: parent, requestID: requestID,
                                                     time: time, expectedSettingsRevision: expectedSettingsRevision,
                                                     enabled: enabled, dimensions: dimensions)
        self.hostID = hostID
        self.parent = parent
        self.requestID = requestID
        self.time = time
        self.expectedSettingsRevision = expectedSettingsRevision
        self.enabled = enabled
        self.dimensions = dimensions
    }

    func encoded() throws -> Data { try RemoteSettingsUpdateWire.encode(self) }
    var description: String { "RemoteSettingsUpdateRequest([redacted])" }
    var debugDescription: String { description }
}

struct RemoteSettingsUpdateReply: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    let hostID: UUID
    let inReplyTo: UUID
    let requestID: UUID
    let generation: RemoteSettingsGeneration
    let view: RemoteSettingsOriginalView

    fileprivate init(hostID: UUID, inReplyTo: UUID, requestID: UUID,
                     generation: RemoteSettingsGeneration, view: RemoteSettingsOriginalView) {
        self.hostID = hostID
        self.inReplyTo = inReplyTo
        self.requestID = requestID
        self.generation = generation
        self.view = view
    }

    var description: String { "RemoteSettingsUpdateReply([redacted])" }
    var debugDescription: String { description }
}

enum RemoteSettingsUpdateWire {
    fileprivate static func validateRequest(hostID: UUID, parent: RemoteSettingsParent, requestID: UUID,
                                            time: RemoteSettingsRequestTime, expectedSettingsRevision: UInt64,
                                            enabled: Bool?, dimensions: RemoteSettingsUpdateDimensions?) throws {
        try validateIdentifier(hostID)
        try validateIdentifier(requestID)
        try validateIdentifier(parent.parentID)
        try validateIdentifier(parent.generation.grantID)
        try validateIdentifier(parent.generation.hostEpoch)
        try validateIdentifier(parent.generation.connectionEpoch)
        guard parent.generation.grantRevision > 0, parent.generation.grantRevision <= UInt64(Int64.max),
              expectedSettingsRevision > 0, expectedSettingsRevision <= UInt64(Int64.max),
              time.issuedAt > 0, time.issuedAt <= UInt64(Int64.max), time.expiresAt <= UInt64(Int64.max),
              time.expiresAt > time.issuedAt, time.expiresAt - time.issuedAt <= 30_000,
              enabled != nil || dimensions != nil else { throw WireError.invalid }
        if case .some(.selected(let value)) = dimensions {
            guard value > 0, value <= 65_536 else { throw WireError.invalid }
        }
    }

    fileprivate static func encode(_ request: RemoteSettingsUpdateRequest) throws -> Data {
        do {
            try validateRequest(hostID: request.hostID, parent: request.parent, requestID: request.requestID,
                                time: request.time, expectedSettingsRevision: request.expectedSettingsRevision,
                                enabled: request.enabled, dimensions: request.dimensions)
            let dimensions: WireValue
            switch request.dimensions {
            case nil: dimensions = .null
            case .some(.backendDefault): dimensions = .object(["kind": .string("backend_default")])
            case .some(.selected(let value)):
                dimensions = .object(["kind": .string("selected"), "dimensions": .unsigned(UInt64(value))])
            }
            return try WireValue.object([
                "schema_version": .unsigned(1),
                "payload": .object([
                    "kind": .string("settings"), "version": .unsigned(1), "host_id": .id(request.hostID),
                    "message": .object([
                        "kind": .string("command"), "parent": parentWire(request.parent),
                        "request_id": .id(request.requestID),
                        "time": .object([
                            "issued_at": .unsigned(request.time.issuedAt), "expires_at": .unsigned(request.time.expiresAt)
                        ]),
                        "body": .object([
                            "kind": .string("update"), "expected_settings_revision": .unsigned(request.expectedSettingsRevision),
                            "enabled": request.enabled.map { .bool($0) } ?? .null, "dimensions": dimensions
                        ])
                    ])
                ])
            ]).encoded(maximum: 32_768)
        } catch let error as WireError {
            throw error == .size ? WireError.size : WireError.invalid
        } catch {
            throw WireError.invalid
        }
    }

    static func decodeReply(_ data: Data, matching request: RemoteSettingsUpdateRequest,
                            encryptedRequestID: UUID) throws -> RemoteSettingsUpdateReply {
        do {
            try validateRequest(hostID: request.hostID, parent: request.parent, requestID: request.requestID,
                                time: request.time, expectedSettingsRevision: request.expectedSettingsRevision,
                                enabled: request.enabled, dimensions: request.dimensions)
            try validateIdentifier(encryptedRequestID)
            let packet = try object(WireValue.parse(data, maximum: 65_536), ["schema_version", "payload"])
            guard try packet.required("schema_version").integer == 1 else { throw WireError.invalid }
            let payload = try object(packet.required("payload"), ["kind", "version", "host_id", "message"])
            guard try payload.required("kind").text == "settings", try payload.required("version").integer == 1 else {
                throw WireError.invalid
            }
            let hostID = try identifier(payload.required("host_id"))
            let response = try object(payload.required("message"), ["kind", "in_reply_to", "correlation", "generation", "body"])
            guard try response.required("kind").text == "response" else { throw WireError.invalid }
            let inReplyTo = try identifier(response.required("in_reply_to"))
            let correlation = try object(response.required("correlation"), ["family", "request_id"])
            guard try correlation.required("family").text == "command" else { throw WireError.invalid }
            let requestID = try identifier(correlation.required("request_id"))
            let generation = try decodeGeneration(response.required("generation"))
            let body = try object(response.required("body"), ["kind", "request_id", "view"])
            guard try body.required("kind").text == "original",
                  try identifier(body.required("request_id")) == request.requestID,
                  hostID == request.hostID, inReplyTo == encryptedRequestID, requestID == request.requestID,
                  generation == request.parent.generation else { throw WireError.invalid }
            let view = try RemoteSettingsReadWire.decodeOriginalView(body.required("view"))
            try matchOriginal(view, request: request)
            return RemoteSettingsUpdateReply(hostID: hostID, inReplyTo: inReplyTo, requestID: requestID,
                                             generation: generation, view: view)
        } catch let error as WireError {
            throw error == .size ? WireError.size : WireError.invalid
        } catch {
            throw WireError.invalid
        }
    }

    private static func matchOriginal(_ view: RemoteSettingsOriginalView, request: RemoteSettingsUpdateRequest) throws {
        guard view.originalKind == .update, view.mutationOperationIDs.isEmpty else { throw WireError.invalid }
        switch (view.link, view.service) {
        case (.unlinked, .notRequested): break
        case (.operation(let operationID), .recorded(let receipt)):
            guard operationID == request.requestID, receipt.operationID == request.requestID,
                  receipt.expectedRevision == request.expectedSettingsRevision else { throw WireError.invalid }
            if let write = receipt.write {
                guard write.changedKeys.allSatisfy({ key in
                    switch key {
                    case .embeddingsEnabled: request.enabled != nil
                    case .embeddingsDimensions: request.dimensions != nil
                    default: false
                    }
                }) else { throw WireError.invalid }
            }
        default: throw WireError.invalid
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

    private static func decodeGeneration(_ value: WireValue) throws -> RemoteSettingsGeneration {
        let fields = try object(value, ["grant_id", "grant_revision", "host_epoch", "connection_epoch"])
        return try RemoteSettingsGeneration(grantID: identifier(fields.required("grant_id")),
                                            grantRevision: fields.required("grant_revision").integer,
                                            hostEpoch: identifier(fields.required("host_epoch")),
                                            connectionEpoch: identifier(fields.required("connection_epoch")))
    }

    private static func object(_ value: WireValue, _ keys: Set<String>) throws -> [String: WireValue] {
        try value.object(keys: keys, required: keys)
    }

    private static func validateIdentifier(_ value: UUID) throws {
        guard value != WireID.zero else { throw WireError.invalid }
    }

    private static func identifier(_ value: WireValue) throws -> UUID {
        let text = try value.text
        guard let identifier = UUID(uuidString: text), identifier != WireID.zero,
              identifier.uuidString.lowercased() == text else { throw WireError.invalid }
        return identifier
    }
}

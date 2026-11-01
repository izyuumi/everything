import Foundation

enum OwnerSettingsMetadataError: Error, LocalizedError, Sendable {
    case invalidDocument
    case invalidEnrollment
    case bindingConflict
    case referenceInUse
    case revisionExhausted
    case unexpectedReceipt
    case conflictingReadback

    var errorDescription: String? {
        switch self {
        case .invalidDocument: "The owner settings document is invalid or exceeds its supported limit."
        case .invalidEnrollment: "The original native credential operation has not established this binding."
        case .bindingConflict: "The credential binding differs from the original settings. Reload and review the configuration."
        case .referenceInUse: "The credential reference is already configured for another binding."
        case .revisionExhausted: "The settings revision cannot accept another change."
        case .unexpectedReceipt: "The response does not match the original owner settings operation. Inspect that operation."
        case .conflictingReadback: "The returned settings differ from the original metadata candidate. Inspect the original operation."
        }
    }
}

struct OwnerSettingsSystemOne: Equatable, Sendable {
    let endpoint: String
    let model: String
    let reference: UUID?
    let enabled: Bool
    var provider: String? = nil
}

struct OwnerSettingsDocument: Sendable {
    static let maximumBytes = 64 * 1024

    let hostID: UUID
    let revision: UInt64
    let defaultModel: SettingsModelSelection?
    let modelCredentials: [SettingsModelCredentialBinding]
    let systemOne: OwnerSettingsSystemOne?
    private let document: JSONValue
    private let reservedReferences: Set<UUID>

    init(settingsResponse: JSONValue, hostID: UUID) throws {
        let fields = try Self.object(settingsResponse)
        guard Set(fields.keys) == ["kind", "settings"], fields["kind"] == .string("settings"),
              let settings = fields["settings"] else { throw OwnerSettingsMetadataError.invalidDocument }
        try self.init(document: settings, hostID: hostID)
    }

    private init(document: JSONValue, hostID: UUID) throws {
        do {
            try SettingsWire.identifier(hostID)
            _ = try JSONValue.decode(document.encoded(maximum: Self.maximumBytes))
            let fields = try Self.object(document)
            guard fields["schema_version"]?.unsigned == 1,
                  let revision = fields["revision"]?.unsigned,
                  let conversation = fields["default_conversation"],
                  let credentials = fields["model_credentials"]?.array,
                  credentials.count <= 16,
                  let selection = fields["selection"], let mcp = fields["mcp"],
                  let overrides = fields["preference_overrides"] else {
                throw OwnerSettingsMetadataError.invalidDocument
            }
            try SettingsWire.revision(revision)
            let defaultModel: SettingsModelSelection?
            if conversation == .null {
                defaultModel = nil
            } else {
                let configuration = try Self.object(conversation)
                guard let provider = configuration["provider"], let endpoint = configuration["endpoint"],
                      let model = configuration["model"] else { throw OwnerSettingsMetadataError.invalidDocument }
                defaultModel = try Self.typed(SettingsModelSelection.self, .object([
                    "provider": provider, "endpoint": endpoint, "model": model
                ]))
            }
            var modelCredentials: [SettingsModelCredentialBinding] = []
            var reserved: Set<UUID> = []
            for credential in credentials {
                let credentialFields = try Self.object(credential)
                guard let provider = credentialFields["provider"], let endpoint = credentialFields["endpoint"],
                      let reference = credentialFields["reference"], let storage = credentialFields["storage"],
                      try Self.optionalReference(reference) != nil else { throw OwnerSettingsMetadataError.invalidDocument }
                let binding = try Self.typed(SettingsModelCredentialBinding.self, .object([
                    "provider": provider, "endpoint": endpoint, "reference": reference, "storage": storage
                ]))
                guard !modelCredentials.contains(where: { $0.provider == binding.provider && $0.endpoint == binding.endpoint }) else {
                    throw OwnerSettingsMetadataError.invalidDocument
                }
                modelCredentials.append(binding)
                reserved.insert(binding.reference)
            }
            let selectionFields = try Self.object(selection)
            let overrideFields = try Self.object(overrides)
            let enabled: Bool?
            switch overrideFields["system_one_enabled"] {
            case .none, .some(.null): enabled = nil
            case .some(.bool(let value)): enabled = value
            default: throw OwnerSettingsMetadataError.invalidDocument
            }
            let systemOne: OwnerSettingsSystemOne?
            switch selectionFields["kind"]?.string {
            case "local":
                systemOne = nil
            case "system_one":
                guard let endpoint = selectionFields["endpoint"]?.string,
                      let model = selectionFields["model"]?.string,
                      let referenceValue = selectionFields["credential_reference"] else {
                    throw OwnerSettingsMetadataError.invalidDocument
                }
                try SettingsWire.text(endpoint, maximum: 4096)
                try SettingsWire.text(model, maximum: 256)
                let provider = selectionFields["provider"]?.string
                if let provider { try SettingsWire.text(provider, maximum: 64) }
                let reference = try Self.optionalReference(referenceValue)
                if let reference { reserved.insert(reference) }
                systemOne = OwnerSettingsSystemOne(endpoint: endpoint, model: model, reference: reference, enabled: enabled ?? true,
                                                   provider: provider)
            default: throw OwnerSettingsMetadataError.invalidDocument
            }
            let mcpFields = try Self.object(mcp)
            guard let servers = mcpFields["servers"]?.array, servers.count <= 16 else {
                throw OwnerSettingsMetadataError.invalidDocument
            }
            for server in servers {
                let serverFields = try Self.object(server)
                guard let environmentValue = serverFields["environment"] else { throw OwnerSettingsMetadataError.invalidDocument }
                let environment = try Self.object(environmentValue)
                guard environment.count <= 64 else { throw OwnerSettingsMetadataError.invalidDocument }
                for value in environment.values {
                    let referenceFields = try Self.object(value)
                    guard let referenceValue = referenceFields["reference"],
                          let reference = try Self.optionalReference(referenceValue),
                          let storage = referenceFields["storage"]?.string,
                          SettingsCredentialStorage(rawValue: storage) != nil else {
                        throw OwnerSettingsMetadataError.invalidDocument
                    }
                    reserved.insert(reference)
                }
            }
            self.hostID = hostID
            self.revision = revision
            self.defaultModel = defaultModel
            self.modelCredentials = modelCredentials
            self.systemOne = systemOne
            self.document = document
            reservedReferences = reserved
        } catch {
            throw OwnerSettingsMetadataError.invalidDocument
        }
    }

    func credentialMetadataPlan(
        request: SettingsActionRequest,
        admittedTarget: SettingsActionTarget,
        admittedOperation: SettingsOperationInspection,
        originalReference: SettingsActionReference,
        completed: SettingsOperationInspection,
        nativeSessionID: UUID,
        ownerRequestID: OwnerID = OwnerID()
    ) throws -> OwnerSettingsCredentialMetadataPlan {
        do {
            try request.validate()
            try admittedTarget.validate()
            try admittedOperation.validate()
            try originalReference.validate()
            try completed.validate()
            try SettingsWire.identifier(nativeSessionID)
        } catch {
            throw OwnerSettingsMetadataError.invalidEnrollment
        }
        guard admittedTarget == .native(request: request),
              admittedOperation.phase == .awaitingAction(reference: originalReference),
              admittedOperation.binding == completed.binding,
              admittedOperation.origin == completed.origin,
              admittedOperation.write == nil, admittedOperation.application == nil,
              request.scope == .host(hostID: hostID), request.expectedRevision == revision,
              completed.binding.requestID == request.requestID,
              completed.binding.clientSessionID == nativeSessionID,
              completed.binding.scope == request.scope, completed.binding.expectedRevision == revision,
              completed.origin == .nativeUI(sessionID: nativeSessionID),
              completed.write == nil, completed.application == nil,
              case .finished(let result) = completed.phase, case .completed = result.state,
              case .credentialEntry(let binding) = request.action else {
            throw OwnerSettingsMetadataError.invalidEnrollment
        }
        var candidate = document
        switch binding {
        case .model(let modelBinding):
            if let configured = modelCredentials.first(where: { $0.provider == modelBinding.provider && $0.endpoint == modelBinding.endpoint }) {
                guard configured == modelBinding else { throw OwnerSettingsMetadataError.bindingConflict }
                return .configuredReplacement
            }
            guard let defaultModel, defaultModel.provider == modelBinding.provider,
                  defaultModel.endpoint == modelBinding.endpoint, modelCredentials.count < 16 else {
                throw OwnerSettingsMetadataError.bindingConflict
            }
            guard !reservedReferences.contains(modelBinding.reference) else { throw OwnerSettingsMetadataError.referenceInUse }
            guard let existing = document["model_credentials"].array else { throw OwnerSettingsMetadataError.invalidDocument }
            let encoded = try Self.modelCredential(modelBinding)
            candidate = try candidate.replacing("model_credentials", with: .array(existing + [encoded]))
        case .systemOne(let endpoint, let reference):
            guard let systemOne, systemOne.endpoint == endpoint else { throw OwnerSettingsMetadataError.bindingConflict }
            if let configured = systemOne.reference {
                guard configured == reference else { throw OwnerSettingsMetadataError.bindingConflict }
                return .configuredReplacement
            }
            guard !reservedReferences.contains(reference) else { throw OwnerSettingsMetadataError.referenceInUse }
            let selection = try document["selection"].replacing("credential_reference", with: .string(reference.uuidString.lowercased()))
            candidate = try candidate.replacing("selection", with: selection)
        }
        guard revision < UInt64(Int64.max) else { throw OwnerSettingsMetadataError.revisionExhausted }
        guard ownerRequestID.value != request.requestID, ownerRequestID.value != nativeSessionID else {
            throw OwnerSettingsMetadataError.invalidEnrollment
        }
        _ = try Self(document: candidate, hostID: hostID)
        return .initialEnrollment(OwnerSettingsPreparedLink(
            hostID: hostID, expectedRevision: revision, binding: binding,
            ownerRequest: OwnerRequest(id: ownerRequestID, body: .object([
                "kind": .string("operation"), "request_id": .string(ownerRequestID.string),
                "operation": .object([
                    "kind": .string("replace_settings"), "expected_revision": .unsigned(revision), "settings": candidate
                ])
            ]), mutating: true), candidate: candidate
        ))
    }

    private static func object(_ value: JSONValue) throws -> [String: JSONValue] {
        guard case .object(let fields) = value else { throw OwnerSettingsMetadataError.invalidDocument }
        return fields
    }

    private static func typed<Value: SettingsWireData>(_ type: Value.Type, _ value: JSONValue) throws -> Value {
        try SettingsWire.decode(type, from: value.encoded(maximum: SettingsWire.snapshotLimit))
    }

    private static func optionalReference(_ value: JSONValue) throws -> UUID? {
        if value == .null { return nil }
        guard let string = value.string, let reference = UUID(uuidString: string),
              string == reference.uuidString.lowercased() else { throw OwnerSettingsMetadataError.invalidDocument }
        try SettingsWire.identifier(reference)
        return reference
    }

    private static func modelCredential(_ binding: SettingsModelCredentialBinding) throws -> JSONValue {
        var value = try JSONValue.decode(SettingsWire.encode(binding))
        value = try value.replacing("reference", with: .string(binding.reference.uuidString.lowercased()))
        return value
    }
}

enum OwnerSettingsCredentialMetadataPlan: Sendable {
    case configuredReplacement
    case initialEnrollment(OwnerSettingsPreparedLink)
}

struct OwnerSettingsPreparedLink: Sendable {
    let hostID: UUID
    let expectedRevision: UInt64
    let binding: SettingsCredentialBinding
    let ownerRequest: OwnerRequest
    private let candidate: JSONValue

    fileprivate init(hostID: UUID, expectedRevision: UInt64, binding: SettingsCredentialBinding, ownerRequest: OwnerRequest, candidate: JSONValue) {
        self.hostID = hostID
        self.expectedRevision = expectedRevision
        self.binding = binding
        self.ownerRequest = ownerRequest
        self.candidate = candidate
    }

    func inspectionRequest() -> OwnerRequest {
        .query("settings_operation", fields: ["request_id": .string(ownerRequest.id.string)])
    }

    func reconciliationRequest() throws -> OwnerRequest {
        let request = OwnerRequest.operation("reconcile_settings", fields: ["operation_id": .string(ownerRequest.id.string)])
        guard request.id != ownerRequest.id else { throw OwnerSettingsMetadataError.unexpectedReceipt }
        return request
    }

    func matchingAppliedSnapshot(_ value: JSONValue) throws -> OwnerSettingsDocument {
        let snapshot = try OwnerSettingsDocument(settingsResponse: .object(["kind": .string("settings"), "settings": value]), hostID: hostID)
        let expected = try candidate.replacing("revision", with: .unsigned(expectedRevision + 1))
        guard Self.normalizedIntegers(value) == Self.normalizedIntegers(expected) else {
            throw OwnerSettingsMetadataError.conflictingReadback
        }
        return snapshot
    }

    private static func normalizedIntegers(_ value: JSONValue) -> JSONValue {
        switch value {
        case .integer(let integer) where integer >= 0: .unsigned(UInt64(integer))
        case .array(let values): .array(values.map(normalizedIntegers))
        case .object(let fields): .object(fields.mapValues(normalizedIntegers))
        default: value
        }
    }
}

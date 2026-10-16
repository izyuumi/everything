import Foundation

struct NativeSettingsDraft: Equatable, Sendable {
    enum Endpoint: String, CaseIterable, Sendable {
        case official
        case loopback
    }

    let baseline: SettingsSnapshot
    let registry: SettingRegistry
    var modelEnabled: Bool
    var provider: SettingsProviderKind
    var endpoint: Endpoint
    var loopbackURL: String
    var model: String
    var systemOneEnabled: Bool
    var systemOneEndpoint: String
    var systemOneModel: String
    var embeddingsEnabled: Bool
    var dimensions: UInt32?

    init(snapshot: SettingsSnapshot, registry: SettingRegistry, systemOne: OwnerSettingsSystemOne?) throws {
        try snapshot.validate()
        try registry.validate()
        guard snapshot.scope.kind == .host,
              case .model(let selection) = snapshot.entry(.modelSelection)?.desired,
              case .toolSelection(let tools) = snapshot.entry(.toolSelection)?.desired,
              case .boolean(let enabled) = snapshot.entry(.embeddingsEnabled)?.desired,
              case .dimensions(let dimensions) = snapshot.entry(.embeddingsDimensions)?.desired else {
            throw SettingsWireError.invalid
        }
        baseline = snapshot
        self.registry = registry
        modelEnabled = selection != nil
        provider = selection?.provider ?? .anthropic
        model = selection?.model ?? ""
        switch selection?.endpoint {
        case .loopback(let url): endpoint = .loopback; loopbackURL = url
        case .official, nil: endpoint = .official; loopbackURL = ""
        }
        switch tools {
        case .local:
            systemOneEnabled = false
            systemOneEndpoint = systemOne?.endpoint ?? "https://api.typesafe.ai/v1/systemone"
            systemOneModel = systemOne?.model ?? "jev-latest"
        case .systemOne(let endpoint, let model):
            systemOneEnabled = true
            systemOneEndpoint = endpoint
            systemOneModel = model
        }
        embeddingsEnabled = enabled
        self.dimensions = dimensions
    }

    var dimensionChoices: [UInt32]? {
        guard case .optionalDimensions(let choices) = registry.descriptors.first(where: { $0.key == .embeddingsDimensions })?.constraints else { return nil }
        return choices
    }

    var isDirty: Bool {
        modelChanged || toolsChanged || baseline.entry(.embeddingsEnabled)?.desired != .boolean(value: embeddingsEnabled)
            || baseline.entry(.embeddingsDimensions)?.desired != .dimensions(value: dimensions)
    }

    var validationMessage: String? {
        do { _ = try changes(); return nil }
        catch { return "Review the changed fields. Model IDs and endpoints must be nonempty and within the Host's limits; dimensions require current model metadata." }
    }

    func patch(requestID: UUID) throws -> SettingsPatch {
        let changes = try changes()
        guard !changes.isEmpty, baseline.revision < UInt64(Int64.max) else { throw SettingsWireError.invalid }
        let patch = SettingsPatch(contractVersion: 1, requestID: requestID, scope: baseline.scope,
                                  expectedRevision: baseline.revision, changes: changes, confirmation: nil)
        try patch.validate()
        return patch
    }

    private var modelChanged: Bool {
        guard modelEnabled else { return baseline.entry(.modelSelection)?.desired != .model(selection: nil) }
        guard case .model(let original?) = baseline.entry(.modelSelection)?.desired else { return true }
        let currentEndpoint: SettingsModelEndpoint = endpoint == .official ? .official : .loopback(url: loopbackURL)
        return original.provider != provider || original.model != model || original.endpoint != currentEndpoint
    }

    private var toolsChanged: Bool {
        let value: ToolSelectionPreference = systemOneEnabled ? .systemOne(endpoint: systemOneEndpoint, model: systemOneModel) : .local
        return baseline.entry(.toolSelection)?.desired != .toolSelection(selection: value)
    }

    private func changes() throws -> [SettingChange] {
        var changes: [SettingChange] = []
        if modelChanged {
            let selection = modelEnabled ? SettingsModelSelection(provider: provider,
                endpoint: endpoint == .official ? .official : .loopback(url: loopbackURL), model: model) : nil
            changes.append(SettingChange(key: .modelSelection, value: .model(selection: selection)))
        }
        if toolsChanged {
            let selection: ToolSelectionPreference = systemOneEnabled ? .systemOne(endpoint: systemOneEndpoint, model: systemOneModel) : .local
            changes.append(SettingChange(key: .toolSelection, value: .toolSelection(selection: selection)))
        }
        if baseline.entry(.embeddingsEnabled)?.desired != .boolean(value: embeddingsEnabled) {
            changes.append(SettingChange(key: .embeddingsEnabled, value: .boolean(value: embeddingsEnabled)))
        }
        if baseline.entry(.embeddingsDimensions)?.desired != .dimensions(value: dimensions) {
            guard let choices = dimensionChoices, dimensions == nil || choices.contains(where: { $0 == dimensions }) else { throw SettingsWireError.invalid }
            changes.append(SettingChange(key: .embeddingsDimensions, value: .dimensions(value: dimensions)))
        }
        for change in changes {
            guard let descriptor = registry.descriptors.first(where: { $0.key == change.key }), descriptor.applyBehavior != .readOnly else {
                throw SettingsWireError.invalid
            }
            try descriptor.validate()
            try change.value.validate(for: change.key)
        }
        return changes
    }
}

extension SettingsSnapshot {
    func entry(_ key: SettingKey) -> SettingEntry? { entries.first { $0.key == key } }
}

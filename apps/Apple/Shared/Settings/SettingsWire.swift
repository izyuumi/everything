import Foundation

enum SettingsWireError: Error {
    case invalid
    case size
}

protocol SettingsWireData: Codable, Equatable, Sendable {
    func validate() throws
}

enum SettingsWire {
    static let patchLimit = 16 * 1024
    static let snapshotLimit = 32 * 1024
    static let zeroID = UUID(uuid: (0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0))

    static func decode<Value: SettingsWireData>(_ type: Value.Type, from data: Data, maximum: Int = snapshotLimit) throws -> Value {
        guard maximum > 0, maximum <= snapshotLimit, !data.isEmpty, data.count <= maximum else {
            throw SettingsWireError.size
        }
        do {
            let value = try JSONDecoder().decode(type, from: data)
            try value.validate()
            return value
        } catch {
            throw SettingsWireError.invalid
        }
    }

    static func encode<Value: SettingsWireData>(_ value: Value, maximum: Int = patchLimit) throws -> Data {
        guard maximum > 0, maximum <= snapshotLimit else { throw SettingsWireError.size }
        try value.validate()
        let data = try encoder().encode(value)
        guard data.count <= maximum else { throw SettingsWireError.size }
        return data
    }

    static func bound<Value: Encodable>(_ value: Value, _ maximum: Int) throws {
        guard try encoder().encode(value).count <= maximum else { throw SettingsWireError.size }
    }

    private static func encoder() -> JSONEncoder {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys, .withoutEscapingSlashes]
        return encoder
    }

    static func identifier(_ value: UUID) throws {
        guard value != zeroID else { throw SettingsWireError.invalid }
    }

    static func revision(_ value: UInt64) throws {
        guard value > 0, value <= UInt64(Int64.max) else { throw SettingsWireError.invalid }
    }

    static func timestamp(_ value: UInt64) throws {
        guard value <= UInt64(Int64.max) else { throw SettingsWireError.invalid }
    }

    static func version(_ value: UInt16) throws {
        guard value == 1 else { throw SettingsWireError.invalid }
    }

    static func text(_ value: String, maximum: Int) throws {
        guard !value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              value.utf8.count <= maximum,
              !value.unicodeScalars.contains(where: { $0.properties.generalCategory == .control }) else {
            throw SettingsWireError.invalid
        }
    }

    static func digest(_ value: String) throws {
        guard value.utf8.count == 64,
              value.utf8.allSatisfy({ (48...57).contains($0) || (97...102).contains($0) }) else {
            throw SettingsWireError.invalid
        }
    }

    static func orderedKeys(_ keys: [SettingKey], scope: SettingScopeKind) throws {
        guard keys.count <= SettingKey.allCases.count,
              keys.allSatisfy({ $0.scope == scope }),
              zip(keys, keys.dropFirst()).allSatisfy({ $0.order < $1.order }) else {
            throw SettingsWireError.invalid
        }
    }
}

struct SettingsWireKey: CodingKey {
    let stringValue: String
    var intValue: Int? { nil }
    init(_ value: String) { stringValue = value }
    init?(stringValue: String) { self.init(stringValue) }
    init?(intValue: Int) { return nil }
}

struct SettingsWireFields {
    let container: KeyedDecodingContainer<SettingsWireKey>

    init(_ decoder: any Decoder) throws {
        guard decoder.codingPath.count <= 32 else { throw SettingsWireError.size }
        container = try decoder.container(keyedBy: SettingsWireKey.self)
    }

    func only(_ names: Set<String>) throws {
        guard container.allKeys.allSatisfy({ names.contains($0.stringValue) }) else {
            throw SettingsWireError.invalid
        }
    }

    func required<Value: Decodable>(_ type: Value.Type, _ name: String) throws -> Value {
        try container.decode(type, forKey: SettingsWireKey(name))
    }

    func optional<Value: Decodable>(_ type: Value.Type, _ name: String) throws -> Value? {
        try container.decodeIfPresent(type, forKey: SettingsWireKey(name))
    }
}

enum SettingKey: String, Codable, CaseIterable, Sendable {
    case modelSelection = "models.selection"
    case toolSelection = "tools.selection"
    case embeddingsEnabled = "memory.embeddings.enabled"
    case embeddingsDimensions = "memory.embeddings.dimensions"
    case embeddingsModel = "memory.embeddings.model"
    case notificationsCompletion = "notifications.completion"
    case notificationsAttention = "notifications.attention"
    case notificationAuthorization = "notifications.authorization"

    var scope: SettingScopeKind {
        switch self {
        case .notificationsCompletion, .notificationsAttention: .devicePreferences
        case .notificationAuthorization: .observedOSState
        default: .host
        }
    }

    var agentMutability: AgentMutability {
        switch self {
        case .modelSelection, .toolSelection: .requiresNativeConfirmation
        case .embeddingsModel, .notificationAuthorization: .readOnly
        default: .allowlistedPreference
        }
    }

    var order: Int {
        switch self {
        case .modelSelection: 0
        case .toolSelection: 1
        case .embeddingsEnabled: 2
        case .embeddingsDimensions: 3
        case .embeddingsModel: 4
        case .notificationsCompletion: 5
        case .notificationsAttention: 6
        case .notificationAuthorization: 7
        }
    }
}

enum SettingScopeKind: String, Codable, CaseIterable, Sendable {
    case host
    case devicePreferences = "device_preferences"
    case observedOSState = "observed_os_state"
}

enum SettingsScope: SettingsWireData {
    case host(hostID: UUID)
    case devicePreferences(deviceID: UUID)
    case observedOSState(deviceID: UUID)

    var kind: SettingScopeKind {
        switch self {
        case .host: .host
        case .devicePreferences: .devicePreferences
        case .observedOSState: .observedOSState
        }
    }

    var identifier: UUID {
        switch self {
        case .host(let id), .devicePreferences(let id), .observedOSState(let id): id
        }
    }

    func validate() throws {
        try SettingsWire.identifier(identifier)
    }

    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        switch try fields.required(String.self, "kind") {
        case "host":
            try fields.only(["kind", "host_id"])
            self = .host(hostID: try fields.required(UUID.self, "host_id"))
        case "device_preferences":
            try fields.only(["kind", "device_id"])
            self = .devicePreferences(deviceID: try fields.required(UUID.self, "device_id"))
        case "observed_os_state":
            try fields.only(["kind", "device_id"])
            self = .observedOSState(deviceID: try fields.required(UUID.self, "device_id"))
        default: throw SettingsWireError.invalid
        }
        try validate()
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        switch self {
        case .host(let hostID):
            try fields.encode("host", forKey: SettingsWireKey("kind"))
            try fields.encode(hostID, forKey: SettingsWireKey("host_id"))
        case .devicePreferences(let deviceID):
            try fields.encode("device_preferences", forKey: SettingsWireKey("kind"))
            try fields.encode(deviceID, forKey: SettingsWireKey("device_id"))
        case .observedOSState(let deviceID):
            try fields.encode("observed_os_state", forKey: SettingsWireKey("kind"))
            try fields.encode(deviceID, forKey: SettingsWireKey("device_id"))
        }
    }
}

enum AgentMutability: String, Codable, CaseIterable, Sendable {
    case allowlistedPreference = "allowlisted_preference"
    case requiresNativeConfirmation = "requires_native_confirmation"
    case readOnly = "read_only"
}

enum SettingSensitivity: String, Codable, CaseIterable, Sendable {
    case nonSecretPreference = "non_secret_preference"
    case recipientMetadata = "recipient_metadata"
    case observedDeviceState = "observed_device_state"
}

enum SettingApplyBehavior: String, Codable, CaseIterable, Sendable {
    case subsequentRun = "subsequent_run"
    case subsequentSelection = "subsequent_selection"
    case stopNewWorkThenApply = "stop_new_work_then_apply"
    case rebuildRequired = "rebuild_required"
    case thisDevice = "this_device"
    case readOnly = "read_only"
}

enum SettingsToolName: String, Codable, CaseIterable, Sendable {
    case list = "settings.list"
    case get = "settings.get"
    case update = "settings.update"
    case requestAction = "settings.request_action"
    case devicesList = "devices.list"
    case devicesPair = "devices.pair"
    case devicesRevoke = "devices.revoke"

    var providerName: String { rawValue.replacingOccurrences(of: ".", with: "_") }
}

enum SettingsProviderKind: String, Codable, CaseIterable, Sendable {
    case anthropic
    case openAI = "openai"
}

enum SettingsNotificationAuthorization: String, Codable, CaseIterable, Sendable {
    case notDetermined = "not_determined"
    case denied
    case authorized
    case provisional
    case ephemeral
    case unavailable
}

enum SettingState: String, Codable, CaseIterable, Sendable {
    case effective
    case pendingApplication = "pending_application"
    case restartRequired = "restart_required"
    case unavailable
}

enum SettingsUnavailableReason: String, Codable, CaseIterable, Sendable {
    case hostOffline = "host_offline"
    case providerUnavailable = "provider_unavailable"
    case embeddingMetadataUnavailable = "embedding_metadata_unavailable"
    case embeddingBoundaryUnavailable = "embedding_boundary_unavailable"
    case nativeActionUnavailable = "native_action_unavailable"
    case permissionDenied = "permission_denied"
    case expired
}

enum SettingsFailureReason: String, Codable, CaseIterable, Sendable {
    case invalidRequest = "invalid_request"
    case storage
    case applyFailed = "apply_failed"
    case nativeActionFailed = "native_action_failed"
}

enum SettingsModelEndpoint: SettingsWireData {
    case official
    case loopback(url: String)

    func validate() throws {
        if case .loopback(let url) = self {
            try SettingsWire.text(url, maximum: 2048)
        }
    }

    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        switch try fields.required(String.self, "kind") {
        case "official":
            try fields.only(["kind"])
            self = .official
        case "loopback":
            try fields.only(["kind", "url"])
            self = .loopback(url: try fields.required(String.self, "url"))
        default: throw SettingsWireError.invalid
        }
        try validate()
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        switch self {
        case .official:
            try fields.encode("official", forKey: SettingsWireKey("kind"))
        case .loopback(let url):
            try fields.encode("loopback", forKey: SettingsWireKey("kind"))
            try fields.encode(url, forKey: SettingsWireKey("url"))
        }
    }
}

struct SettingsModelSelection: SettingsWireData {
    let provider: SettingsProviderKind
    let endpoint: SettingsModelEndpoint
    let model: String

    func validate() throws {
        try endpoint.validate()
        try SettingsWire.text(model, maximum: 256)
    }
}

extension SettingsModelSelection {
    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        try fields.only(["provider", "endpoint", "model"])
        provider = try fields.required(SettingsProviderKind.self, "provider")
        endpoint = try fields.required(SettingsModelEndpoint.self, "endpoint")
        model = try fields.required(String.self, "model")
        try validate()
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        try fields.encode(provider, forKey: SettingsWireKey("provider"))
        try fields.encode(endpoint, forKey: SettingsWireKey("endpoint"))
        try fields.encode(model, forKey: SettingsWireKey("model"))
    }
}

enum ToolSelectionPreference: SettingsWireData {
    case local
    case systemOne(endpoint: String, model: String)

    func validate() throws {
        if case .systemOne(let endpoint, let model) = self {
            try SettingsWire.text(endpoint, maximum: 4096)
            try SettingsWire.text(model, maximum: 256)
        }
    }

    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        switch try fields.required(String.self, "kind") {
        case "local":
            try fields.only(["kind"])
            self = .local
        case "system_one":
            try fields.only(["kind", "endpoint", "model"])
            self = .systemOne(endpoint: try fields.required(String.self, "endpoint"), model: try fields.required(String.self, "model"))
        default: throw SettingsWireError.invalid
        }
        try validate()
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        switch self {
        case .local:
            try fields.encode("local", forKey: SettingsWireKey("kind"))
        case .systemOne(let endpoint, let model):
            try fields.encode("system_one", forKey: SettingsWireKey("kind"))
            try fields.encode(endpoint, forKey: SettingsWireKey("endpoint"))
            try fields.encode(model, forKey: SettingsWireKey("model"))
        }
    }
}

enum SettingValue: SettingsWireData {
    case model(selection: SettingsModelSelection?)
    case toolSelection(selection: ToolSelectionPreference)
    case boolean(value: Bool)
    case dimensions(value: UInt32?)
    case embeddingModel(model: String?)
    case notificationAuthorization(state: SettingsNotificationAuthorization)

    func validate(for key: SettingKey) throws {
        try validate()
        switch (key, self) {
        case (.modelSelection, .model), (.toolSelection, .toolSelection),
             (.embeddingsEnabled, .boolean), (.notificationsCompletion, .boolean),
             (.notificationsAttention, .boolean), (.embeddingsDimensions, .dimensions),
             (.embeddingsModel, .embeddingModel), (.notificationAuthorization, .notificationAuthorization): break
        default: throw SettingsWireError.invalid
        }
    }

    func validate() throws {
        switch self {
        case .model(let selection): try selection?.validate()
        case .toolSelection(let selection): try selection.validate()
        case .dimensions(let value):
            if let value, !(1...65_536).contains(value) { throw SettingsWireError.invalid }
        case .embeddingModel(let model):
            if let model { try SettingsWire.text(model, maximum: 256) }
        default: break
        }
    }

    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        switch try fields.required(String.self, "kind") {
        case "model":
            try fields.only(["kind", "selection"])
            self = .model(selection: try fields.optional(SettingsModelSelection.self, "selection"))
        case "tool_selection":
            try fields.only(["kind", "selection"])
            self = .toolSelection(selection: try fields.required(ToolSelectionPreference.self, "selection"))
        case "boolean":
            try fields.only(["kind", "value"])
            self = .boolean(value: try fields.required(Bool.self, "value"))
        case "dimensions":
            try fields.only(["kind", "value"])
            self = .dimensions(value: try fields.optional(UInt32.self, "value"))
        case "embedding_model":
            try fields.only(["kind", "model"])
            self = .embeddingModel(model: try fields.optional(String.self, "model"))
        case "notification_authorization":
            try fields.only(["kind", "state"])
            self = .notificationAuthorization(state: try fields.required(SettingsNotificationAuthorization.self, "state"))
        default: throw SettingsWireError.invalid
        }
        try validate()
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        switch self {
        case .model(let selection):
            try fields.encode("model", forKey: SettingsWireKey("kind"))
            try fields.encode(selection, forKey: SettingsWireKey("selection"))
        case .toolSelection(let selection):
            try fields.encode("tool_selection", forKey: SettingsWireKey("kind"))
            try fields.encode(selection, forKey: SettingsWireKey("selection"))
        case .boolean(let value):
            try fields.encode("boolean", forKey: SettingsWireKey("kind"))
            try fields.encode(value, forKey: SettingsWireKey("value"))
        case .dimensions(let value):
            try fields.encode("dimensions", forKey: SettingsWireKey("kind"))
            try fields.encode(value, forKey: SettingsWireKey("value"))
        case .embeddingModel(let model):
            try fields.encode("embedding_model", forKey: SettingsWireKey("kind"))
            try fields.encode(model, forKey: SettingsWireKey("model"))
        case .notificationAuthorization(let state):
            try fields.encode("notification_authorization", forKey: SettingsWireKey("kind"))
            try fields.encode(state, forKey: SettingsWireKey("state"))
        }
    }
}

enum SettingConstraints: SettingsWireData {
    case model(maxModelBytes: UInt32)
    case toolSelection(maxEndpointBytes: UInt32, maxModelBytes: UInt32)
    case boolean
    case optionalDimensions(choices: [UInt32]?)
    case embeddingModel
    case notificationAuthorization

    func validate() throws {
        if case .optionalDimensions(let choices) = self, let choices {
            guard !choices.isEmpty, choices.count <= 256,
                  choices.allSatisfy({ (1...65_536).contains($0) }),
                  zip(choices, choices.dropFirst()).allSatisfy({ $0 < $1 }) else { throw SettingsWireError.invalid }
        }
    }

    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        switch try fields.required(String.self, "kind") {
        case "model":
            try fields.only(["kind", "max_model_bytes"])
            self = .model(maxModelBytes: try fields.required(UInt32.self, "max_model_bytes"))
        case "tool_selection":
            try fields.only(["kind", "max_endpoint_bytes", "max_model_bytes"])
            self = .toolSelection(maxEndpointBytes: try fields.required(UInt32.self, "max_endpoint_bytes"), maxModelBytes: try fields.required(UInt32.self, "max_model_bytes"))
        case "boolean":
            try fields.only(["kind"])
            self = .boolean
        case "optional_dimensions":
            try fields.only(["kind", "choices"])
            self = .optionalDimensions(choices: try fields.optional([UInt32].self, "choices"))
        case "embedding_model":
            try fields.only(["kind"])
            self = .embeddingModel
        case "notification_authorization":
            try fields.only(["kind"])
            self = .notificationAuthorization
        default: throw SettingsWireError.invalid
        }
        try validate()
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        switch self {
        case .model(let maxModelBytes):
            try fields.encode("model", forKey: SettingsWireKey("kind"))
            try fields.encode(maxModelBytes, forKey: SettingsWireKey("max_model_bytes"))
        case .toolSelection(let maxEndpointBytes, let maxModelBytes):
            try fields.encode("tool_selection", forKey: SettingsWireKey("kind"))
            try fields.encode(maxEndpointBytes, forKey: SettingsWireKey("max_endpoint_bytes"))
            try fields.encode(maxModelBytes, forKey: SettingsWireKey("max_model_bytes"))
        case .boolean:
            try fields.encode("boolean", forKey: SettingsWireKey("kind"))
        case .optionalDimensions(let choices):
            try fields.encode("optional_dimensions", forKey: SettingsWireKey("kind"))
            try fields.encode(choices, forKey: SettingsWireKey("choices"))
        case .embeddingModel:
            try fields.encode("embedding_model", forKey: SettingsWireKey("kind"))
        case .notificationAuthorization:
            try fields.encode("notification_authorization", forKey: SettingsWireKey("kind"))
        }
    }
}

struct SettingDescriptor: SettingsWireData {
    let key: SettingKey
    let scope: SettingScopeKind
    let constraints: SettingConstraints
    let defaultValue: SettingValue
    let sensitivity: SettingSensitivity
    let agentMutability: AgentMutability
    let applyBehavior: SettingApplyBehavior

    func validate() throws {
        try constraints.validate()
        try defaultValue.validate(for: key)
        guard scope == key.scope, agentMutability == key.agentMutability else { throw SettingsWireError.invalid }
        let expected: (SettingConstraints, SettingValue, SettingSensitivity, SettingApplyBehavior)
        switch key {
        case .modelSelection:
            expected = (.model(maxModelBytes: 256), .model(selection: nil), .recipientMetadata, .subsequentRun)
        case .toolSelection:
            expected = (.toolSelection(maxEndpointBytes: 4096, maxModelBytes: 256), .toolSelection(selection: .local), .recipientMetadata, .subsequentSelection)
        case .embeddingsEnabled:
            expected = (.boolean, .boolean(value: false), .nonSecretPreference, .stopNewWorkThenApply)
        case .embeddingsDimensions:
            guard case .optionalDimensions = constraints else { throw SettingsWireError.invalid }
            expected = (constraints, .dimensions(value: nil), .nonSecretPreference, .rebuildRequired)
        case .embeddingsModel:
            expected = (.embeddingModel, .embeddingModel(model: nil), .nonSecretPreference, .readOnly)
        case .notificationsCompletion, .notificationsAttention:
            expected = (.boolean, .boolean(value: false), .nonSecretPreference, .thisDevice)
        case .notificationAuthorization:
            expected = (.notificationAuthorization, .notificationAuthorization(state: .unavailable), .observedDeviceState, .readOnly)
        }
        guard constraints == expected.0, defaultValue == expected.1,
              sensitivity == expected.2, applyBehavior == expected.3 else { throw SettingsWireError.invalid }
    }
}

extension SettingDescriptor {
    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        try fields.only(["key", "scope", "constraints", "default", "sensitivity", "agent_mutability", "apply_behavior"])
        key = try fields.required(SettingKey.self, "key")
        scope = try fields.required(SettingScopeKind.self, "scope")
        constraints = try fields.required(SettingConstraints.self, "constraints")
        defaultValue = try fields.required(SettingValue.self, "default")
        sensitivity = try fields.required(SettingSensitivity.self, "sensitivity")
        agentMutability = try fields.required(AgentMutability.self, "agent_mutability")
        applyBehavior = try fields.required(SettingApplyBehavior.self, "apply_behavior")
        try validate()
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        try fields.encode(key, forKey: SettingsWireKey("key"))
        try fields.encode(scope, forKey: SettingsWireKey("scope"))
        try fields.encode(constraints, forKey: SettingsWireKey("constraints"))
        try fields.encode(defaultValue, forKey: SettingsWireKey("default"))
        try fields.encode(sensitivity, forKey: SettingsWireKey("sensitivity"))
        try fields.encode(agentMutability, forKey: SettingsWireKey("agent_mutability"))
        try fields.encode(applyBehavior, forKey: SettingsWireKey("apply_behavior"))
    }
}

struct SettingRegistry: SettingsWireData {
    let contractVersion: UInt16
    let descriptors: [SettingDescriptor]

    func validate() throws {
        try SettingsWire.version(contractVersion)
        guard descriptors.map(\.key) == SettingKey.allCases else { throw SettingsWireError.invalid }
        for descriptor in descriptors { try descriptor.validate() }
        try SettingsWire.bound(self, SettingsWire.snapshotLimit)
    }
}

extension SettingRegistry {
    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        try fields.only(["contract_version", "descriptors"])
        contractVersion = try fields.required(UInt16.self, "contract_version")
        descriptors = try fields.required([SettingDescriptor].self, "descriptors")
        try validate()
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        try fields.encode(contractVersion, forKey: SettingsWireKey("contract_version"))
        try fields.encode(descriptors, forKey: SettingsWireKey("descriptors"))
    }
}

struct SettingsConfirmationReference: SettingsWireData {
    let id: UUID
    let expiresAt: UInt64

    func validate() throws {
        try SettingsWire.identifier(id)
        try SettingsWire.timestamp(expiresAt)
    }
}

extension SettingsConfirmationReference {
    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        try fields.only(["id", "expires_at"])
        id = try fields.required(UUID.self, "id")
        expiresAt = try fields.required(UInt64.self, "expires_at")
        try validate()
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        try fields.encode(id, forKey: SettingsWireKey("id"))
        try fields.encode(expiresAt, forKey: SettingsWireKey("expires_at"))
    }
}

struct SettingsActionReference: SettingsWireData {
    let id: UUID
    let expiresAt: UInt64

    func validate() throws {
        try SettingsWire.identifier(id)
        try SettingsWire.timestamp(expiresAt)
    }
}

extension SettingsActionReference {
    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        try fields.only(["id", "expires_at"])
        id = try fields.required(UUID.self, "id")
        expiresAt = try fields.required(UInt64.self, "expires_at")
        try validate()
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        try fields.encode(id, forKey: SettingsWireKey("id"))
        try fields.encode(expiresAt, forKey: SettingsWireKey("expires_at"))
    }
}

struct SettingChange: SettingsWireData {
    let key: SettingKey
    let value: SettingValue

    func validate() throws {
        try value.validate(for: key)
    }
}

extension SettingChange {
    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        try fields.only(["key", "value"])
        key = try fields.required(SettingKey.self, "key")
        value = try fields.required(SettingValue.self, "value")
        try validate()
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        try fields.encode(key, forKey: SettingsWireKey("key"))
        try fields.encode(value, forKey: SettingsWireKey("value"))
    }
}

struct SettingsPatch: SettingsWireData {
    let contractVersion: UInt16
    let requestID: UUID
    let scope: SettingsScope
    let expectedRevision: UInt64
    let changes: [SettingChange]
    let confirmation: SettingsConfirmationReference?

    func validate() throws {
        try SettingsWire.version(contractVersion)
        try SettingsWire.identifier(requestID)
        try scope.validate()
        try SettingsWire.revision(expectedRevision)
        guard !changes.isEmpty, changes.count <= SettingKey.allCases.count,
              Set(changes.map(\.key)).count == changes.count else { throw SettingsWireError.invalid }
        for change in changes {
            guard change.key.scope == scope.kind, change.key.agentMutability != .readOnly else { throw SettingsWireError.invalid }
            try change.validate()
        }
        try confirmation?.validate()
        try SettingsWire.bound(self, SettingsWire.patchLimit)
    }
}

extension SettingsPatch {
    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        try fields.only(["contract_version", "request_id", "scope", "expected_revision", "changes", "confirmation"])
        contractVersion = try fields.required(UInt16.self, "contract_version")
        requestID = try fields.required(UUID.self, "request_id")
        scope = try fields.required(SettingsScope.self, "scope")
        expectedRevision = try fields.required(UInt64.self, "expected_revision")
        changes = try fields.required([SettingChange].self, "changes")
        confirmation = try fields.optional(SettingsConfirmationReference.self, "confirmation")
        try validate()
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        try fields.encode(contractVersion, forKey: SettingsWireKey("contract_version"))
        try fields.encode(requestID, forKey: SettingsWireKey("request_id"))
        try fields.encode(scope, forKey: SettingsWireKey("scope"))
        try fields.encode(expectedRevision, forKey: SettingsWireKey("expected_revision"))
        try fields.encode(changes, forKey: SettingsWireKey("changes"))
        try fields.encode(confirmation, forKey: SettingsWireKey("confirmation"))
    }
}

struct SettingEntry: SettingsWireData {
    let key: SettingKey
    let desired: SettingValue?
    let effective: SettingValue?
    let state: SettingState
    let unavailableReason: SettingsUnavailableReason?

    func validate() throws {
        try desired?.validate(for: key)
        try effective?.validate(for: key)
        guard !(key.agentMutability == .readOnly && desired != nil),
              (state == .unavailable) == (unavailableReason != nil) else { throw SettingsWireError.invalid }
        if state == .effective {
            guard effective != nil, desired == nil || desired == effective else { throw SettingsWireError.invalid }
        }
    }
}

extension SettingEntry {
    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        try fields.only(["key", "desired", "effective", "state", "unavailable_reason"])
        key = try fields.required(SettingKey.self, "key")
        desired = try fields.optional(SettingValue.self, "desired")
        effective = try fields.optional(SettingValue.self, "effective")
        state = try fields.required(SettingState.self, "state")
        unavailableReason = try fields.optional(SettingsUnavailableReason.self, "unavailable_reason")
        try validate()
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        try fields.encode(key, forKey: SettingsWireKey("key"))
        try fields.encode(desired, forKey: SettingsWireKey("desired"))
        try fields.encode(effective, forKey: SettingsWireKey("effective"))
        try fields.encode(state, forKey: SettingsWireKey("state"))
        try fields.encode(unavailableReason, forKey: SettingsWireKey("unavailable_reason"))
    }
}

struct SettingsSnapshot: SettingsWireData {
    let contractVersion: UInt16
    let scope: SettingsScope
    let revision: UInt64
    let observedAt: UInt64
    let stale: Bool
    let entries: [SettingEntry]

    func validate() throws {
        try SettingsWire.version(contractVersion)
        try scope.validate()
        try SettingsWire.revision(revision)
        try SettingsWire.timestamp(observedAt)
        guard entries.count <= SettingKey.allCases.count,
              Set(entries.map(\.key)).count == entries.count else { throw SettingsWireError.invalid }
        for entry in entries {
            guard entry.key.scope == scope.kind else { throw SettingsWireError.invalid }
            try entry.validate()
        }
        try SettingsWire.bound(self, SettingsWire.snapshotLimit)
    }
}

extension SettingsSnapshot {
    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        try fields.only(["contract_version", "scope", "revision", "observed_at", "stale", "entries"])
        contractVersion = try fields.required(UInt16.self, "contract_version")
        scope = try fields.required(SettingsScope.self, "scope")
        revision = try fields.required(UInt64.self, "revision")
        observedAt = try fields.required(UInt64.self, "observed_at")
        stale = try fields.required(Bool.self, "stale")
        entries = try fields.required([SettingEntry].self, "entries")
        try validate()
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        try fields.encode(contractVersion, forKey: SettingsWireKey("contract_version"))
        try fields.encode(scope, forKey: SettingsWireKey("scope"))
        try fields.encode(revision, forKey: SettingsWireKey("revision"))
        try fields.encode(observedAt, forKey: SettingsWireKey("observed_at"))
        try fields.encode(stale, forKey: SettingsWireKey("stale"))
        try fields.encode(entries, forKey: SettingsWireKey("entries"))
    }
}

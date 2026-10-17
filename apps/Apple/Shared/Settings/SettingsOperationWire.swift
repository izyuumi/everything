import Foundation

enum SettingsCredentialStorage: String, Codable, CaseIterable, Sendable {
    case session
    case native
}

enum SettingsSection: String, Codable, CaseIterable, Sendable {
    case general
    case models
    case memory
    case toolSelection = "tool_selection"
    case notifications
    case devices
    case ownerSecurity = "owner_security"
}

struct SettingsModelCredentialBinding: SettingsWireData {
    let provider: SettingsProviderKind
    let endpoint: SettingsModelEndpoint
    let reference: UUID
    let storage: SettingsCredentialStorage

    func validate() throws {
        try endpoint.validate()
        try SettingsWire.identifier(reference)
    }
}

extension SettingsModelCredentialBinding {
    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        try fields.only(["provider", "endpoint", "reference", "storage"])
        provider = try fields.required(SettingsProviderKind.self, "provider")
        endpoint = try fields.required(SettingsModelEndpoint.self, "endpoint")
        reference = try fields.required(UUID.self, "reference")
        storage = try fields.required(SettingsCredentialStorage.self, "storage")
        try validate()
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        try fields.encode(provider, forKey: SettingsWireKey("provider"))
        try fields.encode(endpoint, forKey: SettingsWireKey("endpoint"))
        try fields.encode(reference, forKey: SettingsWireKey("reference"))
        try fields.encode(storage, forKey: SettingsWireKey("storage"))
    }
}

enum SettingsCredentialBinding: SettingsWireData {
    case model(binding: SettingsModelCredentialBinding)
    case systemOne(endpoint: String, reference: UUID)

    func validate() throws {
        switch self {
        case .model(let binding): try binding.validate()
        case .systemOne(let endpoint, let reference):
            try SettingsWire.text(endpoint, maximum: 4096)
            try SettingsWire.identifier(reference)
        }
    }

    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        switch try fields.required(String.self, "kind") {
        case "model":
            try fields.only(["kind", "binding"])
            self = .model(binding: try fields.required(SettingsModelCredentialBinding.self, "binding"))
        case "system_one":
            try fields.only(["kind", "endpoint", "reference"])
            self = .systemOne(endpoint: try fields.required(String.self, "endpoint"), reference: try fields.required(UUID.self, "reference"))
        default: throw SettingsWireError.invalid
        }
        try validate()
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        switch self {
        case .model(let binding):
            try fields.encode("model", forKey: SettingsWireKey("kind"))
            try fields.encode(binding, forKey: SettingsWireKey("binding"))
        case .systemOne(let endpoint, let reference):
            try fields.encode("system_one", forKey: SettingsWireKey("kind"))
            try fields.encode(endpoint, forKey: SettingsWireKey("endpoint"))
            try fields.encode(reference, forKey: SettingsWireKey("reference"))
        }
    }
}

enum NativeSettingsAction: SettingsWireData {
    case credentialEntry(binding: SettingsCredentialBinding)
    case notificationAuthorization
    case openSection(section: SettingsSection)

    func validate() throws {
        if case .credentialEntry(let binding) = self { try binding.validate() }
    }

    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        switch try fields.required(String.self, "kind") {
        case "credential_entry":
            try fields.only(["kind", "binding"])
            self = .credentialEntry(binding: try fields.required(SettingsCredentialBinding.self, "binding"))
        case "notification_authorization":
            try fields.only(["kind"])
            self = .notificationAuthorization
        case "open_section":
            try fields.only(["kind", "section"])
            self = .openSection(section: try fields.required(SettingsSection.self, "section"))
        default: throw SettingsWireError.invalid
        }
        try validate()
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        switch self {
        case .credentialEntry(let binding):
            try fields.encode("credential_entry", forKey: SettingsWireKey("kind"))
            try fields.encode(binding, forKey: SettingsWireKey("binding"))
        case .notificationAuthorization:
            try fields.encode("notification_authorization", forKey: SettingsWireKey("kind"))
        case .openSection(let section):
            try fields.encode("open_section", forKey: SettingsWireKey("kind"))
            try fields.encode(section, forKey: SettingsWireKey("section"))
        }
    }
}

struct SettingsActionRequest: SettingsWireData {
    let contractVersion: UInt16
    let requestID: UUID
    let scope: SettingsScope
    let expectedRevision: UInt64
    let action: NativeSettingsAction

    func validate() throws {
        try SettingsWire.version(contractVersion)
        try SettingsWire.identifier(requestID)
        try scope.validate()
        try SettingsWire.revision(expectedRevision)
        try action.validate()
        switch action {
        case .credentialEntry:
            guard scope.kind == .host else { throw SettingsWireError.invalid }
        case .notificationAuthorization:
            guard scope.kind == .devicePreferences else { throw SettingsWireError.invalid }
        case .openSection: break
        }
        try SettingsWire.bound(self, SettingsWire.patchLimit)
    }
}

extension SettingsActionRequest {
    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        try fields.only(["contract_version", "request_id", "scope", "expected_revision", "action"])
        contractVersion = try fields.required(UInt16.self, "contract_version")
        requestID = try fields.required(UUID.self, "request_id")
        scope = try fields.required(SettingsScope.self, "scope")
        expectedRevision = try fields.required(UInt64.self, "expected_revision")
        action = try fields.required(NativeSettingsAction.self, "action")
        try validate()
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        try fields.encode(contractVersion, forKey: SettingsWireKey("contract_version"))
        try fields.encode(requestID, forKey: SettingsWireKey("request_id"))
        try fields.encode(scope, forKey: SettingsWireKey("scope"))
        try fields.encode(expectedRevision, forKey: SettingsWireKey("expected_revision"))
        try fields.encode(action, forKey: SettingsWireKey("action"))
    }
}

extension DevicePublicKeys: Codable {
    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        try fields.only(["device_id", "signing_key", "encryption_key"])
        let deviceID = try fields.required(UUID.self, "device_id")
        let signing = try fields.required(String.self, "signing_key")
        let encryption = try fields.required(String.self, "encryption_key")
        guard signing.utf8.count == 44, encryption.utf8.count == 44,
              let signingKey = Data(base64Encoded: signing), let encryptionKey = Data(base64Encoded: encryption),
              signingKey.count == 32, encryptionKey.count == 32,
              signingKey.base64EncodedString() == signing,
              encryptionKey.base64EncodedString() == encryption else { throw SettingsWireError.invalid }
        try self.init(deviceID: deviceID, signingKey: signingKey, encryptionKey: encryptionKey)
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        try fields.encode(deviceID, forKey: SettingsWireKey("device_id"))
        try fields.encode(signingKey.base64EncodedString(), forKey: SettingsWireKey("signing_key"))
        try fields.encode(encryptionKey.base64EncodedString(), forKey: SettingsWireKey("encryption_key"))
    }
}
enum SettingsDeviceAction: SettingsWireData {
    case pair
    case revoke(deviceID: UUID, publicKeys: DevicePublicKeys)

    func validate() throws {
        if case .revoke(let deviceID, let publicKeys) = self {
            try SettingsWire.identifier(deviceID)
            guard deviceID == publicKeys.deviceID else { throw SettingsWireError.invalid }
        }
    }

    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        switch try fields.required(String.self, "kind") {
        case "pair":
            try fields.only(["kind"])
            self = .pair
        case "revoke":
            try fields.only(["kind", "device_id", "public_keys"])
            self = .revoke(deviceID: try fields.required(UUID.self, "device_id"), publicKeys: try fields.required(DevicePublicKeys.self, "public_keys"))
        default: throw SettingsWireError.invalid
        }
        try validate()
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        switch self {
        case .pair:
            try fields.encode("pair", forKey: SettingsWireKey("kind"))
        case .revoke(let deviceID, let publicKeys):
            try fields.encode("revoke", forKey: SettingsWireKey("kind"))
            try fields.encode(deviceID, forKey: SettingsWireKey("device_id"))
            try fields.encode(publicKeys, forKey: SettingsWireKey("public_keys"))
        }
    }
}

struct SettingsDeviceRequest: SettingsWireData {
    let contractVersion: UInt16
    let requestID: UUID
    let scope: SettingsScope
    let action: SettingsDeviceAction

    func validate() throws {
        try SettingsWire.version(contractVersion)
        try SettingsWire.identifier(requestID)
        try scope.validate()
        guard scope.kind == .host else { throw SettingsWireError.invalid }
        try action.validate()
        try SettingsWire.bound(self, SettingsWire.patchLimit)
    }
}

extension SettingsDeviceRequest {
    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        try fields.only(["contract_version", "request_id", "scope", "action"])
        contractVersion = try fields.required(UInt16.self, "contract_version")
        requestID = try fields.required(UUID.self, "request_id")
        scope = try fields.required(SettingsScope.self, "scope")
        action = try fields.required(SettingsDeviceAction.self, "action")
        try validate()
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        try fields.encode(contractVersion, forKey: SettingsWireKey("contract_version"))
        try fields.encode(requestID, forKey: SettingsWireKey("request_id"))
        try fields.encode(scope, forKey: SettingsWireKey("scope"))
        try fields.encode(action, forKey: SettingsWireKey("action"))
    }
}

struct SettingsDeviceSummary: SettingsWireData {
    let deviceID: UUID
    let label: String
    let publicKeys: DevicePublicKeys
    let revoked: Bool
    let thisDevice: Bool

    func validate() throws {
        try SettingsWire.identifier(deviceID)
        try SettingsWire.text(label, maximum: 256)
        guard deviceID == publicKeys.deviceID else { throw SettingsWireError.invalid }
    }
}

extension SettingsDeviceSummary {
    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        try fields.only(["device_id", "label", "public_keys", "revoked", "this_device"])
        deviceID = try fields.required(UUID.self, "device_id")
        label = try fields.required(String.self, "label")
        publicKeys = try fields.required(DevicePublicKeys.self, "public_keys")
        revoked = try fields.required(Bool.self, "revoked")
        thisDevice = try fields.required(Bool.self, "this_device")
        try validate()
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        try fields.encode(deviceID, forKey: SettingsWireKey("device_id"))
        try fields.encode(label, forKey: SettingsWireKey("label"))
        try fields.encode(publicKeys, forKey: SettingsWireKey("public_keys"))
        try fields.encode(revoked, forKey: SettingsWireKey("revoked"))
        try fields.encode(thisDevice, forKey: SettingsWireKey("this_device"))
    }
}

struct SettingsDevicePage: SettingsWireData {
    let contractVersion: UInt16
    let hostID: UUID
    let observedAt: UInt64
    let devices: [SettingsDeviceSummary]
    let nextAfter: UUID?

    func validate() throws {
        try SettingsWire.version(contractVersion)
        try SettingsWire.identifier(hostID)
        try SettingsWire.timestamp(observedAt)
        guard devices.count <= 100,
              zip(devices, devices.dropFirst()).allSatisfy({ $0.deviceID.uuidString < $1.deviceID.uuidString }),
              nextAfter == nil || devices.last?.deviceID == nextAfter else { throw SettingsWireError.invalid }
        for device in devices { try device.validate() }
        try SettingsWire.bound(self, SettingsWire.snapshotLimit)
    }
}

extension SettingsDevicePage {
    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        try fields.only(["contract_version", "host_id", "observed_at", "devices", "next_after"])
        contractVersion = try fields.required(UInt16.self, "contract_version")
        hostID = try fields.required(UUID.self, "host_id")
        observedAt = try fields.required(UInt64.self, "observed_at")
        devices = try fields.required([SettingsDeviceSummary].self, "devices")
        nextAfter = try fields.optional(UUID.self, "next_after")
        try validate()
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        try fields.encode(contractVersion, forKey: SettingsWireKey("contract_version"))
        try fields.encode(hostID, forKey: SettingsWireKey("host_id"))
        try fields.encode(observedAt, forKey: SettingsWireKey("observed_at"))
        try fields.encode(devices, forKey: SettingsWireKey("devices"))
        try fields.encode(nextAfter, forKey: SettingsWireKey("next_after"))
    }
}

struct SettingsOwnerReplacementIdentity: SettingsWireData {
    let requestID: UUID
    let scope: SettingsScope
    let expectedRevision: UInt64
    let exactSettingsSHA256: String
    let changedKeys: [SettingKey]
    let restartRequired: Bool

    func validate() throws {
        try SettingsWire.identifier(requestID)
        try scope.validate()
        try SettingsWire.revision(expectedRevision)
        guard scope.kind == .host, expectedRevision < UInt64(Int64.max) else { throw SettingsWireError.invalid }
        try SettingsWire.digest(exactSettingsSHA256)
        try SettingsWire.orderedKeys(changedKeys, scope: .host)
    }
}

extension SettingsOwnerReplacementIdentity {
    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        try fields.only(["request_id", "scope", "expected_revision", "exact_settings_sha256", "changed_keys", "restart_required"])
        requestID = try fields.required(UUID.self, "request_id")
        scope = try fields.required(SettingsScope.self, "scope")
        expectedRevision = try fields.required(UInt64.self, "expected_revision")
        exactSettingsSHA256 = try fields.required(String.self, "exact_settings_sha256")
        changedKeys = try fields.required([SettingKey].self, "changed_keys")
        restartRequired = try fields.required(Bool.self, "restart_required")
        try validate()
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        try fields.encode(requestID, forKey: SettingsWireKey("request_id"))
        try fields.encode(scope, forKey: SettingsWireKey("scope"))
        try fields.encode(expectedRevision, forKey: SettingsWireKey("expected_revision"))
        try fields.encode(exactSettingsSHA256, forKey: SettingsWireKey("exact_settings_sha256"))
        try fields.encode(changedKeys, forKey: SettingsWireKey("changed_keys"))
        try fields.encode(restartRequired, forKey: SettingsWireKey("restart_required"))
    }
}

enum SettingsActionTarget: SettingsWireData {
    case patch(request: SettingsPatch)
    case native(request: SettingsActionRequest)
    case device(request: SettingsDeviceRequest)
    case ownerReplacement(request: SettingsOwnerReplacementIdentity)

    var requestID: UUID {
        switch self {
        case .patch(let request): request.requestID
        case .native(let request): request.requestID
        case .device(let request): request.requestID
        case .ownerReplacement(let request): request.requestID
        }
    }

    var scope: SettingsScope {
        switch self {
        case .patch(let request): request.scope
        case .native(let request): request.scope
        case .device(let request): request.scope
        case .ownerReplacement(let request): request.scope
        }
    }

    var expectedRevision: UInt64? {
        switch self {
        case .patch(let request): request.expectedRevision
        case .native(let request): request.expectedRevision
        case .device: nil
        case .ownerReplacement(let request): request.expectedRevision
        }
    }

    func validate() throws {
        switch self {
        case .patch(let request):
            try request.validate()
            guard request.confirmation == nil else { throw SettingsWireError.invalid }
        case .native(let request): try request.validate()
        case .device(let request): try request.validate()
        case .ownerReplacement(let request): try request.validate()
        }
    }

    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        switch try fields.required(String.self, "kind") {
        case "patch":
            try fields.only(["kind", "request"])
            self = .patch(request: try fields.required(SettingsPatch.self, "request"))
        case "native":
            try fields.only(["kind", "request"])
            self = .native(request: try fields.required(SettingsActionRequest.self, "request"))
        case "device":
            try fields.only(["kind", "request"])
            self = .device(request: try fields.required(SettingsDeviceRequest.self, "request"))
        case "owner_replacement":
            try fields.only(["kind", "request"])
            self = .ownerReplacement(request: try fields.required(SettingsOwnerReplacementIdentity.self, "request"))
        default: throw SettingsWireError.invalid
        }
        try validate()
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        switch self {
        case .patch(let request):
            try fields.encode("patch", forKey: SettingsWireKey("kind"))
            try fields.encode(request, forKey: SettingsWireKey("request"))
        case .native(let request):
            try fields.encode("native", forKey: SettingsWireKey("kind"))
            try fields.encode(request, forKey: SettingsWireKey("request"))
        case .device(let request):
            try fields.encode("device", forKey: SettingsWireKey("kind"))
            try fields.encode(request, forKey: SettingsWireKey("request"))
        case .ownerReplacement(let request):
            try fields.encode("owner_replacement", forKey: SettingsWireKey("kind"))
            try fields.encode(request, forKey: SettingsWireKey("request"))
        }
    }
}

enum NativeActionReference: SettingsWireData {
    case confirmation(reference: SettingsConfirmationReference)
    case action(reference: SettingsActionReference)

    var id: UUID {
        switch self {
        case .confirmation(let reference): reference.id
        case .action(let reference): reference.id
        }
    }

    var expiresAt: UInt64 {
        switch self {
        case .confirmation(let reference): reference.expiresAt
        case .action(let reference): reference.expiresAt
        }
    }

    func validate() throws {
        switch self {
        case .confirmation(let reference): try reference.validate()
        case .action(let reference): try reference.validate()
        }
    }

    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        switch try fields.required(String.self, "kind") {
        case "confirmation":
            try fields.only(["kind", "reference"])
            self = .confirmation(reference: try fields.required(SettingsConfirmationReference.self, "reference"))
        case "action":
            try fields.only(["kind", "reference"])
            self = .action(reference: try fields.required(SettingsActionReference.self, "reference"))
        default: throw SettingsWireError.invalid
        }
        try validate()
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        switch self {
        case .confirmation(let reference):
            try fields.encode("confirmation", forKey: SettingsWireKey("kind"))
            try fields.encode(reference, forKey: SettingsWireKey("reference"))
        case .action(let reference):
            try fields.encode("action", forKey: SettingsWireKey("kind"))
            try fields.encode(reference, forKey: SettingsWireKey("reference"))
        }
    }
}

struct NativeActionInvocation: SettingsWireData {
    let operationID: UUID
    let nativeRequestID: UUID
    let reference: NativeActionReference

    func validate() throws {
        try SettingsWire.identifier(operationID)
        try SettingsWire.identifier(nativeRequestID)
        try reference.validate()
    }
}

extension NativeActionInvocation {
    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        try fields.only(["operation_id", "native_request_id", "reference"])
        operationID = try fields.required(UUID.self, "operation_id")
        nativeRequestID = try fields.required(UUID.self, "native_request_id")
        reference = try fields.required(NativeActionReference.self, "reference")
        try validate()
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        try fields.encode(operationID, forKey: SettingsWireKey("operation_id"))
        try fields.encode(nativeRequestID, forKey: SettingsWireKey("native_request_id"))
        try fields.encode(reference, forKey: SettingsWireKey("reference"))
    }
}

enum NativeActionObservation: SettingsWireData {
    case dismissed
    case notificationAuthorization(state: SettingsNotificationAuthorization)
    case sectionOpened(section: SettingsSection)

    func validate() throws {
    }

    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        switch try fields.required(String.self, "kind") {
        case "dismissed":
            try fields.only(["kind"])
            self = .dismissed
        case "notification_authorization":
            try fields.only(["kind", "state"])
            self = .notificationAuthorization(state: try fields.required(SettingsNotificationAuthorization.self, "state"))
        case "section_opened":
            try fields.only(["kind", "section"])
            self = .sectionOpened(section: try fields.required(SettingsSection.self, "section"))
        default: throw SettingsWireError.invalid
        }
        try validate()
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        switch self {
        case .dismissed:
            try fields.encode("dismissed", forKey: SettingsWireKey("kind"))
        case .notificationAuthorization(let state):
            try fields.encode("notification_authorization", forKey: SettingsWireKey("kind"))
            try fields.encode(state, forKey: SettingsWireKey("state"))
        case .sectionOpened(let section):
            try fields.encode("section_opened", forKey: SettingsWireKey("kind"))
            try fields.encode(section, forKey: SettingsWireKey("section"))
        }
    }
}

struct SettingsRequestBinding: SettingsWireData {
    let requestID: UUID
    let principalID: UUID
    let clientSessionID: UUID
    let scope: SettingsScope
    let expectedRevision: UInt64?
    let exactRequestSHA256: String
    let issuedAt: UInt64
    let expiresAt: UInt64

    func validate() throws {
        try SettingsWire.identifier(requestID)
        try SettingsWire.identifier(principalID)
        try SettingsWire.identifier(clientSessionID)
        try scope.validate()
        if let expectedRevision { try SettingsWire.revision(expectedRevision) }
        try SettingsWire.digest(exactRequestSHA256)
        try SettingsWire.timestamp(issuedAt)
        try SettingsWire.timestamp(expiresAt)
        guard expiresAt > issuedAt, expiresAt - issuedAt <= 300_000 else { throw SettingsWireError.invalid }
    }
}

extension SettingsRequestBinding {
    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        try fields.only(["request_id", "principal_id", "client_session_id", "scope", "expected_revision", "exact_request_sha256", "issued_at", "expires_at"])
        requestID = try fields.required(UUID.self, "request_id")
        principalID = try fields.required(UUID.self, "principal_id")
        clientSessionID = try fields.required(UUID.self, "client_session_id")
        scope = try fields.required(SettingsScope.self, "scope")
        expectedRevision = try fields.optional(UInt64.self, "expected_revision")
        exactRequestSHA256 = try fields.required(String.self, "exact_request_sha256")
        issuedAt = try fields.required(UInt64.self, "issued_at")
        expiresAt = try fields.required(UInt64.self, "expires_at")
        try validate()
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        try fields.encode(requestID, forKey: SettingsWireKey("request_id"))
        try fields.encode(principalID, forKey: SettingsWireKey("principal_id"))
        try fields.encode(clientSessionID, forKey: SettingsWireKey("client_session_id"))
        try fields.encode(scope, forKey: SettingsWireKey("scope"))
        try fields.encode(expectedRevision, forKey: SettingsWireKey("expected_revision"))
        try fields.encode(exactRequestSHA256, forKey: SettingsWireKey("exact_request_sha256"))
        try fields.encode(issuedAt, forKey: SettingsWireKey("issued_at"))
        try fields.encode(expiresAt, forKey: SettingsWireKey("expires_at"))
    }
}

enum SettingsOperationOrigin: SettingsWireData {
    case localOwner(requestID: UUID)
    case nativeUI(sessionID: UUID)
    case settingsAgent(sessionID: UUID, nativeSessionID: UUID)
    case pairedDevice(deviceID: UUID)

    func validate() throws {
        switch self {
        case .localOwner(let requestID): try SettingsWire.identifier(requestID)
        case .nativeUI(let sessionID): try SettingsWire.identifier(sessionID)
        case .settingsAgent(let sessionID, let nativeSessionID):
            try SettingsWire.identifier(sessionID)
            try SettingsWire.identifier(nativeSessionID)
        case .pairedDevice(let deviceID): try SettingsWire.identifier(deviceID)
        }
    }

    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        switch try fields.required(String.self, "kind") {
        case "local_owner":
            try fields.only(["kind", "request_id"])
            self = .localOwner(requestID: try fields.required(UUID.self, "request_id"))
        case "native_ui":
            try fields.only(["kind", "session_id"])
            self = .nativeUI(sessionID: try fields.required(UUID.self, "session_id"))
        case "settings_agent":
            try fields.only(["kind", "session_id", "native_session_id"])
            self = .settingsAgent(sessionID: try fields.required(UUID.self, "session_id"), nativeSessionID: try fields.required(UUID.self, "native_session_id"))
        case "paired_device":
            try fields.only(["kind", "device_id"])
            self = .pairedDevice(deviceID: try fields.required(UUID.self, "device_id"))
        default: throw SettingsWireError.invalid
        }
        try validate()
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        switch self {
        case .localOwner(let requestID):
            try fields.encode("local_owner", forKey: SettingsWireKey("kind"))
            try fields.encode(requestID, forKey: SettingsWireKey("request_id"))
        case .nativeUI(let sessionID):
            try fields.encode("native_ui", forKey: SettingsWireKey("kind"))
            try fields.encode(sessionID, forKey: SettingsWireKey("session_id"))
        case .settingsAgent(let sessionID, let nativeSessionID):
            try fields.encode("settings_agent", forKey: SettingsWireKey("kind"))
            try fields.encode(sessionID, forKey: SettingsWireKey("session_id"))
            try fields.encode(nativeSessionID, forKey: SettingsWireKey("native_session_id"))
        case .pairedDevice(let deviceID):
            try fields.encode("paired_device", forKey: SettingsWireKey("kind"))
            try fields.encode(deviceID, forKey: SettingsWireKey("device_id"))
        }
    }
}

enum SettingsOperationState: SettingsWireData {
    case applied(revision: UInt64)
    case completed(receiptID: UUID)
    case pendingApplication(revision: UInt64)
    case requiresConfirmation(confirmation: SettingsConfirmationReference)
    case requiresUserAction(action: SettingsActionReference)
    case hostActionRequired
    case restartRequired(revision: UInt64)
    case conflict(observedRevision: UInt64)
    case unavailable(reason: SettingsUnavailableReason)
    case failed(reason: SettingsFailureReason)
    case unknown
    case cancelled

    func validate() throws {
        switch self {
        case .applied(let revision), .pendingApplication(let revision), .restartRequired(let revision), .conflict(let revision):
            try SettingsWire.revision(revision)
        case .completed(let receiptID): try SettingsWire.identifier(receiptID)
        case .requiresConfirmation(let reference): try reference.validate()
        case .requiresUserAction(let reference): try reference.validate()
        default: break
        }
    }

    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        switch try fields.required(String.self, "kind") {
        case "applied":
            try fields.only(["kind", "revision"])
            self = .applied(revision: try fields.required(UInt64.self, "revision"))
        case "completed":
            try fields.only(["kind", "receipt_id"])
            self = .completed(receiptID: try fields.required(UUID.self, "receipt_id"))
        case "pending_application":
            try fields.only(["kind", "revision"])
            self = .pendingApplication(revision: try fields.required(UInt64.self, "revision"))
        case "requires_confirmation":
            try fields.only(["kind", "confirmation"])
            self = .requiresConfirmation(confirmation: try fields.required(SettingsConfirmationReference.self, "confirmation"))
        case "requires_user_action":
            try fields.only(["kind", "action"])
            self = .requiresUserAction(action: try fields.required(SettingsActionReference.self, "action"))
        case "host_action_required":
            try fields.only(["kind"])
            self = .hostActionRequired
        case "restart_required":
            try fields.only(["kind", "revision"])
            self = .restartRequired(revision: try fields.required(UInt64.self, "revision"))
        case "conflict":
            try fields.only(["kind", "observed_revision"])
            self = .conflict(observedRevision: try fields.required(UInt64.self, "observed_revision"))
        case "unavailable":
            try fields.only(["kind", "reason"])
            self = .unavailable(reason: try fields.required(SettingsUnavailableReason.self, "reason"))
        case "failed":
            try fields.only(["kind", "reason"])
            self = .failed(reason: try fields.required(SettingsFailureReason.self, "reason"))
        case "unknown":
            try fields.only(["kind"])
            self = .unknown
        case "cancelled":
            try fields.only(["kind"])
            self = .cancelled
        default: throw SettingsWireError.invalid
        }
        try validate()
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        switch self {
        case .applied(let revision):
            try fields.encode("applied", forKey: SettingsWireKey("kind"))
            try fields.encode(revision, forKey: SettingsWireKey("revision"))
        case .completed(let receiptID):
            try fields.encode("completed", forKey: SettingsWireKey("kind"))
            try fields.encode(receiptID, forKey: SettingsWireKey("receipt_id"))
        case .pendingApplication(let revision):
            try fields.encode("pending_application", forKey: SettingsWireKey("kind"))
            try fields.encode(revision, forKey: SettingsWireKey("revision"))
        case .requiresConfirmation(let confirmation):
            try fields.encode("requires_confirmation", forKey: SettingsWireKey("kind"))
            try fields.encode(confirmation, forKey: SettingsWireKey("confirmation"))
        case .requiresUserAction(let action):
            try fields.encode("requires_user_action", forKey: SettingsWireKey("kind"))
            try fields.encode(action, forKey: SettingsWireKey("action"))
        case .hostActionRequired:
            try fields.encode("host_action_required", forKey: SettingsWireKey("kind"))
        case .restartRequired(let revision):
            try fields.encode("restart_required", forKey: SettingsWireKey("kind"))
            try fields.encode(revision, forKey: SettingsWireKey("revision"))
        case .conflict(let observedRevision):
            try fields.encode("conflict", forKey: SettingsWireKey("kind"))
            try fields.encode(observedRevision, forKey: SettingsWireKey("observed_revision"))
        case .unavailable(let reason):
            try fields.encode("unavailable", forKey: SettingsWireKey("kind"))
            try fields.encode(reason, forKey: SettingsWireKey("reason"))
        case .failed(let reason):
            try fields.encode("failed", forKey: SettingsWireKey("kind"))
            try fields.encode(reason, forKey: SettingsWireKey("reason"))
        case .unknown:
            try fields.encode("unknown", forKey: SettingsWireKey("kind"))
        case .cancelled:
            try fields.encode("cancelled", forKey: SettingsWireKey("kind"))
        }
    }
}

struct SettingsOperationResult: SettingsWireData {
    let contractVersion: UInt16
    let requestID: UUID
    let scope: SettingsScope
    let state: SettingsOperationState

    func validate() throws {
        try SettingsWire.version(contractVersion)
        try SettingsWire.identifier(requestID)
        try scope.validate()
        try state.validate()
        try SettingsWire.bound(self, SettingsWire.patchLimit)
    }
}

extension SettingsOperationResult {
    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        try fields.only(["contract_version", "request_id", "scope", "state"])
        contractVersion = try fields.required(UInt16.self, "contract_version")
        requestID = try fields.required(UUID.self, "request_id")
        scope = try fields.required(SettingsScope.self, "scope")
        state = try fields.required(SettingsOperationState.self, "state")
        try validate()
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        try fields.encode(contractVersion, forKey: SettingsWireKey("contract_version"))
        try fields.encode(requestID, forKey: SettingsWireKey("request_id"))
        try fields.encode(scope, forKey: SettingsWireKey("scope"))
        try fields.encode(state, forKey: SettingsWireKey("state"))
    }
}

enum SettingsApplicationState: SettingsWireData {
    case applied
    case restartRequired
    case unavailable(reason: SettingsUnavailableReason)
    case failed(reason: SettingsFailureReason)

    func validate() throws {
    }

    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        switch try fields.required(String.self, "state") {
        case "applied":
            try fields.only(["state"])
            self = .applied
        case "restart_required":
            try fields.only(["state"])
            self = .restartRequired
        case "unavailable":
            try fields.only(["state", "reason"])
            self = .unavailable(reason: try fields.required(SettingsUnavailableReason.self, "reason"))
        case "failed":
            try fields.only(["state", "reason"])
            self = .failed(reason: try fields.required(SettingsFailureReason.self, "reason"))
        default: throw SettingsWireError.invalid
        }
        try validate()
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        switch self {
        case .applied:
            try fields.encode("applied", forKey: SettingsWireKey("state"))
        case .restartRequired:
            try fields.encode("restart_required", forKey: SettingsWireKey("state"))
        case .unavailable(let reason):
            try fields.encode("unavailable", forKey: SettingsWireKey("state"))
            try fields.encode(reason, forKey: SettingsWireKey("reason"))
        case .failed(let reason):
            try fields.encode("failed", forKey: SettingsWireKey("state"))
            try fields.encode(reason, forKey: SettingsWireKey("reason"))
        }
    }
}

enum SettingsWriteResolution: String, Codable, CaseIterable, Sendable {
    case applied
    case notApplied = "not_applied"
}

struct SettingsWriteInspection: SettingsWireData {
    let expectedRevision: UInt64
    let revision: UInt64
    let changedKeys: [SettingKey]
    let restartRequired: Bool
    let resolution: SettingsWriteResolution?

    func validate() throws {
        try SettingsWire.revision(expectedRevision)
        guard expectedRevision < UInt64(Int64.max), revision == expectedRevision + 1 else { throw SettingsWireError.invalid }
        try SettingsWire.orderedKeys(changedKeys, scope: .host)
    }
}

extension SettingsWriteInspection {
    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        try fields.only(["expected_revision", "revision", "changed_keys", "restart_required", "resolution"])
        expectedRevision = try fields.required(UInt64.self, "expected_revision")
        revision = try fields.required(UInt64.self, "revision")
        changedKeys = try fields.required([SettingKey].self, "changed_keys")
        restartRequired = try fields.required(Bool.self, "restart_required")
        resolution = try fields.optional(SettingsWriteResolution.self, "resolution")
        try validate()
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        try fields.encode(expectedRevision, forKey: SettingsWireKey("expected_revision"))
        try fields.encode(revision, forKey: SettingsWireKey("revision"))
        try fields.encode(changedKeys, forKey: SettingsWireKey("changed_keys"))
        try fields.encode(restartRequired, forKey: SettingsWireKey("restart_required"))
        try fields.encode(resolution, forKey: SettingsWireKey("resolution"))
    }
}

enum SettingsOperationPhase: SettingsWireData {
    case awaitingConfirmation(reference: SettingsConfirmationReference)
    case awaitingAction(reference: SettingsActionReference)
    case inFlight(nativeRequestID: UUID)
    case applying(expectedRevision: UInt64, revision: UInt64)
    case finished(result: SettingsOperationResult)

    func validate() throws {
        switch self {
        case .awaitingConfirmation(let reference): try reference.validate()
        case .awaitingAction(let reference): try reference.validate()
        case .inFlight(let id): try SettingsWire.identifier(id)
        case .applying(let expected, let revision):
            try SettingsWire.revision(expected)
            guard expected < UInt64(Int64.max), revision == expected + 1 else { throw SettingsWireError.invalid }
        case .finished(let result): try result.validate()
        }
    }

    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        switch try fields.required(String.self, "phase") {
        case "awaiting_confirmation":
            try fields.only(["phase", "reference"])
            self = .awaitingConfirmation(reference: try fields.required(SettingsConfirmationReference.self, "reference"))
        case "awaiting_action":
            try fields.only(["phase", "reference"])
            self = .awaitingAction(reference: try fields.required(SettingsActionReference.self, "reference"))
        case "in_flight":
            try fields.only(["phase", "native_request_id"])
            self = .inFlight(nativeRequestID: try fields.required(UUID.self, "native_request_id"))
        case "applying":
            try fields.only(["phase", "expected_revision", "revision"])
            self = .applying(expectedRevision: try fields.required(UInt64.self, "expected_revision"), revision: try fields.required(UInt64.self, "revision"))
        case "finished":
            try fields.only(["phase", "result"])
            self = .finished(result: try fields.required(SettingsOperationResult.self, "result"))
        default: throw SettingsWireError.invalid
        }
        try validate()
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        switch self {
        case .awaitingConfirmation(let reference):
            try fields.encode("awaiting_confirmation", forKey: SettingsWireKey("phase"))
            try fields.encode(reference, forKey: SettingsWireKey("reference"))
        case .awaitingAction(let reference):
            try fields.encode("awaiting_action", forKey: SettingsWireKey("phase"))
            try fields.encode(reference, forKey: SettingsWireKey("reference"))
        case .inFlight(let nativeRequestID):
            try fields.encode("in_flight", forKey: SettingsWireKey("phase"))
            try fields.encode(nativeRequestID, forKey: SettingsWireKey("native_request_id"))
        case .applying(let expectedRevision, let revision):
            try fields.encode("applying", forKey: SettingsWireKey("phase"))
            try fields.encode(expectedRevision, forKey: SettingsWireKey("expected_revision"))
            try fields.encode(revision, forKey: SettingsWireKey("revision"))
        case .finished(let result):
            try fields.encode("finished", forKey: SettingsWireKey("phase"))
            try fields.encode(result, forKey: SettingsWireKey("result"))
        }
    }
}

struct SettingsOperationInspection: SettingsWireData {
    let binding: SettingsRequestBinding
    let origin: SettingsOperationOrigin
    let phase: SettingsOperationPhase
    let updatedAt: UInt64
    let write: SettingsWriteInspection?
    let application: SettingsApplicationState?

    func validate() throws {
        try binding.validate()
        try origin.validate()
        try phase.validate()
        try SettingsWire.timestamp(updatedAt)
        guard updatedAt >= binding.issuedAt else { throw SettingsWireError.invalid }
        switch origin {
        case .localOwner(let requestID):
            guard requestID == binding.requestID, requestID == binding.clientSessionID else { throw SettingsWireError.invalid }
        case .nativeUI(let sessionID):
            guard sessionID == binding.clientSessionID else { throw SettingsWireError.invalid }
        case .settingsAgent(_, let nativeSessionID):
            guard nativeSessionID == binding.clientSessionID else { throw SettingsWireError.invalid }
        case .pairedDevice(let deviceID):
            guard deviceID == binding.clientSessionID else { throw SettingsWireError.invalid }
        }
        switch phase {
        case .awaitingConfirmation(let reference):
            guard reference.expiresAt == binding.expiresAt, binding.expectedRevision != nil else { throw SettingsWireError.invalid }
        case .awaitingAction(let reference):
            guard reference.expiresAt == binding.expiresAt else { throw SettingsWireError.invalid }
        case .applying(let expected, let revision):
            guard let write, write.resolution == nil,
                  write.expectedRevision == expected, write.revision == revision else { throw SettingsWireError.invalid }
        case .finished(let result):
            guard result.requestID == binding.requestID, result.scope == binding.scope else { throw SettingsWireError.invalid }
            switch result.state {
            case .requiresConfirmation, .requiresUserAction: throw SettingsWireError.invalid
            default: break
            }
        case .inFlight: break
        }
        if let write {
            try write.validate()
            guard binding.scope.kind == .host, binding.expectedRevision == write.expectedRevision else { throw SettingsWireError.invalid }
            switch (phase, write.resolution) {
            case (.applying, nil): break
            case (.finished(let result), nil):
                guard result.state == .unknown else { throw SettingsWireError.invalid }
            case (.finished(let result), .applied):
                switch result.state {
                case .applied(let revision), .pendingApplication(let revision), .restartRequired(let revision):
                    guard revision == write.revision else { throw SettingsWireError.invalid }
                default: throw SettingsWireError.invalid
                }
            case (.finished(let result), .notApplied):
                switch result.state {
                case .failed, .unavailable, .cancelled, .conflict: break
                default: throw SettingsWireError.invalid
                }
            default: throw SettingsWireError.invalid
            }
        }
        if let application {
            try application.validate()
            guard case .finished(let result) = phase,
                  case .pendingApplication = result.state,
                  write?.resolution == .applied else { throw SettingsWireError.invalid }
        }
        try SettingsWire.bound(self, SettingsWire.snapshotLimit)
    }
}

extension SettingsOperationInspection {
    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        try fields.only(["binding", "origin", "phase", "updated_at", "write", "application"])
        binding = try fields.required(SettingsRequestBinding.self, "binding")
        origin = try fields.required(SettingsOperationOrigin.self, "origin")
        phase = try fields.required(SettingsOperationPhase.self, "phase")
        updatedAt = try fields.required(UInt64.self, "updated_at")
        write = try fields.optional(SettingsWriteInspection.self, "write")
        application = try fields.optional(SettingsApplicationState.self, "application")
        try validate()
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        try fields.encode(binding, forKey: SettingsWireKey("binding"))
        try fields.encode(origin, forKey: SettingsWireKey("origin"))
        try fields.encode(phase, forKey: SettingsWireKey("phase"))
        try fields.encode(updatedAt, forKey: SettingsWireKey("updated_at"))
        try fields.encodeIfPresent(write, forKey: SettingsWireKey("write"))
        try fields.encodeIfPresent(application, forKey: SettingsWireKey("application"))
    }
}

enum SettingsChangeState: String, Codable, CaseIterable, Sendable {
    case keysChanged = "keys_changed"
    case awaitingConfirmation = "awaiting_confirmation"
    case awaitingAction = "awaiting_action"
    case inFlight = "in_flight"
    case applying
    case applied
    case completed
    case pendingApplication = "pending_application"
    case hostActionRequired = "host_action_required"
    case restartRequired = "restart_required"
    case conflict
    case unavailable
    case failed
    case unknown
    case cancelled
}

struct SettingsChangeHint: SettingsWireData {
    let sequence: UInt64
    let scope: SettingsScope
    let requestID: UUID?
    let revision: UInt64?
    let changedKeys: [SettingKey]
    let state: SettingsChangeState
    let application: SettingsApplicationState?

    func validate() throws {
        try SettingsWire.revision(sequence)
        try scope.validate()
        if let requestID { try SettingsWire.identifier(requestID) }
        if let revision { try SettingsWire.revision(revision) }
        try SettingsWire.orderedKeys(changedKeys, scope: scope.kind)
        try application?.validate()
        try SettingsWire.bound(self, 1024)
    }
}

extension SettingsChangeHint {
    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        try fields.only(["sequence", "scope", "request_id", "revision", "changed_keys", "state", "application"])
        sequence = try fields.required(UInt64.self, "sequence")
        scope = try fields.required(SettingsScope.self, "scope")
        requestID = try fields.optional(UUID.self, "request_id")
        revision = try fields.optional(UInt64.self, "revision")
        changedKeys = try fields.required([SettingKey].self, "changed_keys")
        state = try fields.required(SettingsChangeState.self, "state")
        application = try fields.optional(SettingsApplicationState.self, "application")
        try validate()
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        try fields.encode(sequence, forKey: SettingsWireKey("sequence"))
        try fields.encode(scope, forKey: SettingsWireKey("scope"))
        try fields.encode(requestID, forKey: SettingsWireKey("request_id"))
        try fields.encode(revision, forKey: SettingsWireKey("revision"))
        try fields.encode(changedKeys, forKey: SettingsWireKey("changed_keys"))
        try fields.encode(state, forKey: SettingsWireKey("state"))
        try fields.encode(application, forKey: SettingsWireKey("application"))
    }
}

struct SettingsChangePage: SettingsWireData {
    let contractVersion: UInt16
    let observedAt: UInt64
    let changes: [SettingsChangeHint]
    let nextSequence: UInt64?

    func validate(afterSequence: UInt64?, limit: Int) throws {
        try validate()
        guard (1...128).contains(limit), changes.count <= limit else { throw SettingsWireError.invalid }
        if let afterSequence { try SettingsWire.timestamp(afterSequence) }
        let after = afterSequence ?? 0
        guard changes.allSatisfy({ $0.sequence > after }),
              nextSequence.map({ $0 > after }) ?? true else { throw SettingsWireError.invalid }
    }

    func validate() throws {
        try SettingsWire.version(contractVersion)
        try SettingsWire.timestamp(observedAt)
        guard changes.count <= 128,
              zip(changes, changes.dropFirst()).allSatisfy({ $0.sequence < $1.sequence }),
              changes.isEmpty || nextSequence != nil else { throw SettingsWireError.invalid }
        for change in changes { try change.validate() }
        if let nextSequence {
            try SettingsWire.revision(nextSequence)
            guard changes.last.map({ $0.sequence <= nextSequence }) ?? true else { throw SettingsWireError.invalid }
        }
        try SettingsWire.bound(self, SettingsWire.snapshotLimit)
    }
}

extension SettingsChangePage {
    init(from decoder: any Decoder) throws {
        let fields = try SettingsWireFields(decoder)
        try fields.only(["contract_version", "observed_at", "changes", "next_sequence"])
        contractVersion = try fields.required(UInt16.self, "contract_version")
        observedAt = try fields.required(UInt64.self, "observed_at")
        changes = try fields.required([SettingsChangeHint].self, "changes")
        nextSequence = try fields.optional(UInt64.self, "next_sequence")
        try validate()
    }

    func encode(to encoder: any Encoder) throws {
        var fields = encoder.container(keyedBy: SettingsWireKey.self)
        try fields.encode(contractVersion, forKey: SettingsWireKey("contract_version"))
        try fields.encode(observedAt, forKey: SettingsWireKey("observed_at"))
        try fields.encode(changes, forKey: SettingsWireKey("changes"))
        try fields.encode(nextSequence, forKey: SettingsWireKey("next_sequence"))
    }
}

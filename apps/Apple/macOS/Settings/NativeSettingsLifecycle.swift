import AppKit
import Foundation

enum NativeSettingsSessionError: Error, Equatable, LocalizedError, Sendable {
    case invalidRequest
    case invalidConnection
    case inactive
    case expired
    case closed
    case capacity
    case requestReused
    case openUncertain
    case operationUncertain
    case invalidResponse
    case systemOneProviderEndpoint

    var errorDescription: String? {
        switch self {
        case .invalidRequest: "The native Settings request is invalid."
        case .invalidConnection: "The native Settings connection is invalid."
        case .inactive: "This Settings action requires its original active Settings surface."
        case .expired: "The original native Settings action or session has expired."
        case .closed: "This native Settings session is closed."
        case .capacity: "Native Settings is at its operation capacity."
        case .requestReused: "This Settings identity already belongs to a different request."
        case .openUncertain: "The native Settings session could not be opened reliably. It will not be opened again automatically."
        case .operationUncertain: "The original Settings operation must be inspected before another effect is attempted."
        case .invalidResponse: "The Host returned an invalid native Settings response."
        case .systemOneProviderEndpoint: "The Host rejected this change: System One provider endpoint. Check the provider's base URL, inspect this operation, then save again."
        }
    }
}

enum NativeSettingsIntent: Equatable, Sendable {
    case open
    case update(SettingsPatch)
    case requestAction(SettingsActionRequest)
    case pairDevice(SettingsDeviceRequest)
    case revokeDevice(SettingsDeviceRequest)
    case confirmPatch(NativeActionInvocation)
    case confirmDevice(NativeActionInvocation)
    case present(NativeActionInvocation)
    case submitCredential(NativeActionInvocation)

    var operationID: UUID? {
        switch self {
        case .open: nil
        case .update(let request): request.requestID
        case .requestAction(let request): request.requestID
        case .pairDevice(let request), .revokeDevice(let request): request.requestID
        case .confirmPatch(let invocation), .confirmDevice(let invocation), .present(let invocation), .submitCredential(let invocation): invocation.operationID
        }
    }

    fileprivate var execution: Bool {
        switch self {
        case .confirmPatch, .confirmDevice, .present, .submitCredential: true
        default: false
        }
    }

    func validate() throws {
        switch self {
        case .open: break
        case .update(let request):
            try request.validate()
            guard request.confirmation == nil else { throw NativeSettingsSessionError.invalidRequest }
        case .requestAction(let request): try request.validate()
        case .pairDevice(let request):
            try request.validate()
            guard request.action == .pair else { throw NativeSettingsSessionError.invalidRequest }
        case .revokeDevice(let request):
            try request.validate()
            guard case .revoke = request.action else { throw NativeSettingsSessionError.invalidRequest }
        case .confirmPatch(let invocation), .confirmDevice(let invocation), .present(let invocation), .submitCredential(let invocation):
            try invocation.validate()
        }
    }
}

final class NativeSettingsGate: @unchecked Sendable {
    struct Capture: Sendable {
        let generation: UInt64
        let expiresAt: UInt64
        let deadline: ContinuousClock.Instant
    }

    let connectionKey: UUID
    private let lock = NSLock()
    private var generation: UInt64 = 1
    private var visible = false
    private var active = false
    private var closed = false

    init(connectionKey: UUID) { self.connectionKey = connectionKey }

    func update(visible: Bool? = nil, active: Bool? = nil) {
        lock.withLock {
            guard !closed else { return }
            let nextVisible = visible ?? self.visible
            let nextActive = active ?? self.active
            if nextVisible != self.visible || nextActive != self.active {
                if generation == UInt64.max { closed = true }
                else { generation += 1 }
            }
            self.visible = nextVisible
            self.active = nextActive
        }
    }

    func invalidate() {
        lock.withLock {
            closed = true
            active = false
            visible = false
        }
    }

    func capture(expiresAt: UInt64) throws -> Capture {
        try lock.withLock {
            try requireActive()
            let now = try Self.wallTime()
            try SettingsWire.timestamp(expiresAt)
            guard expiresAt > now else { throw NativeSettingsSessionError.expired }
            return Capture(generation: generation, expiresAt: expiresAt,
                           deadline: ContinuousClock.now.advanced(by: .milliseconds(Int64(min(expiresAt - now, 300_000)))))
        }
    }

    func validate(_ capture: Capture) throws {
        try lock.withLock {
            try requireActive()
            guard capture.generation == generation else { throw NativeSettingsSessionError.inactive }
            guard try Self.wallTime() < capture.expiresAt, ContinuousClock.now < capture.deadline else {
                throw NativeSettingsSessionError.expired
            }
        }
    }

    func requireCurrentActivity() throws {
        try lock.withLock { try requireActive() }
    }

    private func requireActive() throws {
        guard !closed else { throw NativeSettingsSessionError.closed }
        guard active, visible else { throw NativeSettingsSessionError.inactive }
    }

    static func wallTime() throws -> UInt64 {
        let milliseconds = Date().timeIntervalSince1970 * 1000
        guard milliseconds.isFinite, milliseconds >= 0, milliseconds < Double(Int64.max) else {
            throw NativeSettingsSessionError.expired
        }
        return UInt64(milliseconds)
    }
}

final class NativeSettingsAdmission: @unchecked Sendable {
    let intent: NativeSettingsIntent
    let expiresAt: UInt64
    private let owner: NativeSettingsGate
    private let initial: Result<NativeSettingsGate.Capture, NativeSettingsSessionError>
    private let lock = NSLock()
    private var cancelled = false

    fileprivate init(intent: NativeSettingsIntent, expiresAt: UInt64, owner: NativeSettingsGate,
                     initial: Result<NativeSettingsGate.Capture, NativeSettingsSessionError>) {
        self.intent = intent
        self.expiresAt = expiresAt
        self.owner = owner
        self.initial = initial
    }

    func cancel() { lock.withLock { cancelled = true } }

    func validateOwner(_ gate: NativeSettingsGate) throws {
        guard gate === owner else { throw NativeSettingsSessionError.invalidRequest }
    }

    func validate(for gate: NativeSettingsGate) throws {
        try validateOwner(gate)
        guard !lock.withLock({ cancelled }) else { throw NativeSettingsSessionError.inactive }
        try gate.validate(initial.get())
    }

    @MainActor
    func validatePresentation(for gate: NativeSettingsGate) throws {
        guard NSApplication.shared.isActive else { throw NativeSettingsSessionError.inactive }
        try validate(for: gate)
    }
}

final class NativeSettingsAssistantOpenAdmission: @unchecked Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    let requestID = UUID()
    let capture: NativeSettingsGate.Capture
    private let owner: NativeSettingsGate

    var description: String { "NativeSettingsAssistantOpenAdmission([private])" }
    var debugDescription: String { description }

    fileprivate init(owner: NativeSettingsGate, capture: NativeSettingsGate.Capture) {
        self.owner = owner
        self.capture = capture
    }

    func validateOwner(_ gate: NativeSettingsGate) throws {
        guard owner === gate else { throw NativeSettingsSessionError.invalidRequest }
    }

    @MainActor
    func validatePresentation(_ gate: NativeSettingsGate) throws {
        try validateOwner(gate)
        guard NSApplication.shared.isActive else { throw NativeSettingsSessionError.inactive }
        try gate.validate(capture)
    }
}

final class NativeSettingsAssistantConsent: @unchecked Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    private let chat: NativeSettingsSession.AssistantHandle
    private let owner: NativeSettingsGate
    private let capture: NativeSettingsGate.Capture

    var description: String { "NativeSettingsAssistantConsent([private])" }
    var debugDescription: String { description }

    fileprivate init(chat: NativeSettingsSession.AssistantHandle, owner: NativeSettingsGate, capture: NativeSettingsGate.Capture) {
        self.chat = chat
        self.owner = owner
        self.capture = capture
    }

    func validate(_ chat: NativeSettingsSession.AssistantHandle, gate: NativeSettingsGate) throws {
        guard self.chat === chat, owner === gate else { throw NativeSettingsSessionError.invalidRequest }
        try gate.validate(capture)
    }
}

final class NativeSettingsAssistantSendAdmission: @unchecked Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    let requestID = UUID()
    let turnID = UUID()
    let chat: NativeSettingsSession.AssistantHandle
    let capture: NativeSettingsGate.Capture
    let started: ContinuousClock.Instant
    private let consent: NativeSettingsAssistantConsent
    private let owner: NativeSettingsGate
    private let lock = NSLock()
    private var payload: String?
    private var cancelled = false

    var description: String { "NativeSettingsAssistantSendAdmission([private])" }
    var debugDescription: String { description }

    fileprivate init(chat: NativeSettingsSession.AssistantHandle, consent: NativeSettingsAssistantConsent,
                     owner: NativeSettingsGate, capture: NativeSettingsGate.Capture, started: ContinuousClock.Instant, text: String) {
        self.started = started
        self.chat = chat
        self.consent = consent
        self.owner = owner
        self.capture = capture
        payload = text
    }

    func validateOwner(_ gate: NativeSettingsGate) throws {
        guard owner === gate else { throw NativeSettingsSessionError.invalidRequest }
        try chat.validateOwner(gate)
    }

    @MainActor
    func validatePresentation(_ gate: NativeSettingsGate) throws {
        try validateOwner(gate)
        try chat.validateLifetime()
        guard NSApplication.shared.isActive else { throw NativeSettingsSessionError.inactive }
        try consent.validate(chat, gate: gate)
        try gate.validate(capture)
        guard !lock.withLock({ cancelled }) else { throw NativeSettingsSessionError.inactive }
    }

    func takeText(_ gate: NativeSettingsGate) throws -> String {
        try validateOwner(gate)
        try consent.validate(chat, gate: gate)
        try gate.validate(capture)
        return try lock.withLock {
            guard !cancelled, let value = payload else { throw NativeSettingsSessionError.inactive }
            payload = nil
            return value
        }
    }

    func cancel() {
        lock.withLock {
            cancelled = true
            payload = nil
        }
    }
}

@MainActor
final class NativeSettingsLifecycle {
    let session: NativeSettingsSession
    let connectionKey: UUID
    private let gate: NativeSettingsGate
    private var opening: NativeSettingsAdmission?
    private var admissions: [UUID: [NativeSettingsAdmission]] = [:]
    private var drainTask: Task<NativeSettingsSession.DrainReport, Never>?
    private var assistantOpens: [UUID: NativeSettingsAssistantOpenAdmission] = [:]
    private var assistantSends: [UUID: NativeSettingsAssistantSendAdmission] = [:]

    init(client: OwnerIPCClient, connection: OwnerConnection, connectionKey: UUID) throws {
        do {
            try connection.validate()
            try SettingsWire.identifier(connectionKey)
        } catch { throw NativeSettingsSessionError.invalidConnection }
        let gate = NativeSettingsGate(connectionKey: connectionKey)
        self.gate = gate
        self.connectionKey = connectionKey
        session = NativeSettingsSession(client: client, connection: connection, connectionKey: connectionKey, gate: gate)
    }

    func setSurfaceVisible(_ visible: Bool) {
        gate.update(visible: visible)
        if !visible { for admission in assistantSends.values { admission.cancel() } }
    }

    func setApplicationActive(_ active: Bool) {
        let effective = active && NSApplication.shared.isActive
        gate.update(active: effective)
        if !effective { for admission in assistantSends.values { admission.cancel() } }
    }

    func prepareOpen() throws -> NativeSettingsAdmission {
        if let opening { return opening }
        let now = try NativeSettingsGate.wallTime()
        guard now <= UInt64(Int64.max) - 300_000 else { throw NativeSettingsSessionError.expired }
        let admission = makeAdmission(intent: .open, expiresAt: now + 300_000)
        opening = admission
        return admission
    }

    func prepare(_ intent: NativeSettingsIntent, expiresAt: UInt64) throws -> NativeSettingsAdmission {
        do { try intent.validate() }
        catch { throw NativeSettingsSessionError.invalidRequest }
        guard let id = intent.operationID else { throw NativeSettingsSessionError.invalidRequest }
        if let original = admissions[id]?.first(where: { $0.intent.execution == intent.execution }) {
            guard original.intent == intent, original.expiresAt == expiresAt else { throw NativeSettingsSessionError.requestReused }
            return original
        }
        guard admissions[id] != nil || admissions.count < 64 else { throw NativeSettingsSessionError.capacity }
        let admission = makeAdmission(intent: intent, expiresAt: expiresAt)
        admissions[id, default: []].append(admission)
        return admission
    }

    func prepareAssistantAction(_ intent: NativeSettingsIntent,
                                receipt: NativeSettingsSession.AssistantDetailsReceipt) throws -> NativeSettingsAdmission {
        try intent.validate()
        let invocation: NativeActionInvocation
        switch (intent, receipt.details.target) {
        case (.confirmDevice(let value), .device): invocation = value
        case (.present(let value), .native(let request)):
            guard case .openSection = request.action else { throw NativeSettingsSessionError.invalidRequest }
            invocation = value
        default: throw NativeSettingsSessionError.invalidRequest
        }
        guard invocation.operationID == receipt.original.operationID,
              case .awaitingAction(let reference) = receipt.details.operation.phase,
              invocation.reference == .action(reference: reference) else { throw NativeSettingsSessionError.invalidRequest }
        let captured = try receipt.capturePresentation(for: gate)
        if let original = admissions[invocation.operationID]?.first(where: { $0.intent.execution }) {
            guard original.intent == intent, original.expiresAt == reference.expiresAt else { throw NativeSettingsSessionError.requestReused }
            return original
        }
        guard drainTask == nil, admissions[invocation.operationID] != nil || admissions.count < 64 else {
            throw NativeSettingsSessionError.capacity
        }
        let admission = NativeSettingsAdmission(intent: intent, expiresAt: reference.expiresAt, owner: gate, initial: .success(captured))
        admissions[invocation.operationID, default: []].append(admission)
        return admission
    }

    func prepareAssistantOpen() throws -> NativeSettingsAssistantOpenAdmission {
        guard drainTask == nil, assistantOpens.count + admissions.count < 64,
              let opening else { throw NativeSettingsSessionError.capacity }
        guard NSApplication.shared.isActive else { throw NativeSettingsSessionError.inactive }
        let admission = NativeSettingsAssistantOpenAdmission(owner: gate, capture: try gate.capture(expiresAt: opening.expiresAt))
        guard assistantOpens[admission.requestID] == nil, admissions[admission.requestID] == nil else {
            throw NativeSettingsSessionError.requestReused
        }
        assistantOpens[admission.requestID] = admission
        return admission
    }

    func acceptAssistantDisclosure(_ chat: NativeSettingsSession.AssistantHandle) throws -> NativeSettingsAssistantConsent {
        guard drainTask == nil, assistantOpens[chat.openRequestID] != nil else { throw NativeSettingsSessionError.closed }
        try chat.validateOwner(gate)
        guard NSApplication.shared.isActive else { throw NativeSettingsSessionError.inactive }
        try chat.validateLifetime()
        return NativeSettingsAssistantConsent(chat: chat, owner: gate, capture: try gate.capture(expiresAt: chat.opened.expiresAt))
    }

    func prepareAssistantSend(_ chat: NativeSettingsSession.AssistantHandle,
                              consent: NativeSettingsAssistantConsent, text: String) throws -> NativeSettingsAssistantSendAdmission {
        guard drainTask == nil, assistantOpens[chat.openRequestID] != nil,
              assistantSends.count < 64 else { throw NativeSettingsSessionError.capacity }
        try chat.validateOwner(gate)
        try chat.validateLifetime()
        guard NSApplication.shared.isActive else { throw NativeSettingsSessionError.inactive }
        try consent.validate(chat, gate: gate)
        guard text.utf8.count <= 8192, !text.contains("\0"),
              !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { throw NativeSettingsSessionError.invalidRequest }
        let started = ContinuousClock.now
        let admission = NativeSettingsAssistantSendAdmission(chat: chat, consent: consent, owner: gate,
            capture: try gate.capture(expiresAt: chat.opened.expiresAt), started: started, text: text)
        guard assistantSends[admission.turnID] == nil, assistantOpens[admission.turnID] == nil,
              admissions[admission.turnID] == nil else { throw NativeSettingsSessionError.requestReused }
        assistantSends[admission.turnID] = admission
        return admission
    }

    func retireAssistant(_ original: NativeSettingsAssistantOpenAdmission) {
        guard assistantOpens[original.requestID] === original else { return }
        for admission in assistantSends.values where admission.chat.openRequestID == original.requestID { admission.cancel() }
    }

    func invalidate() {
        gate.invalidate()
        guard drainTask == nil else { return }
        for admission in assistantSends.values { admission.cancel() }
        let session = session
        let originals = Array(assistantOpens.values)
        drainTask = Task { await session.drain(assistantOpens: originals) }
    }

    func drain() async -> NativeSettingsSession.DrainReport {
        invalidate()
        guard let drainTask else { return NativeSettingsSession.DrainReport(failures: [.closed], unresolvedOperationIDs: []) }
        return await drainTask.value
    }

    private func makeAdmission(intent: NativeSettingsIntent, expiresAt: UInt64) -> NativeSettingsAdmission {
        let initial: Result<NativeSettingsGate.Capture, NativeSettingsSessionError>
        do {
            guard NSApplication.shared.isActive else { throw NativeSettingsSessionError.inactive }
            initial = .success(try gate.capture(expiresAt: expiresAt))
        } catch let error as NativeSettingsSessionError { initial = .failure(error) }
        catch { initial = .failure(.invalidRequest) }
        return NativeSettingsAdmission(intent: intent, expiresAt: expiresAt, owner: gate, initial: initial)
    }
}

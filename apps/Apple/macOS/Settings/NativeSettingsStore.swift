import AppKit
import Foundation
import Observation

@MainActor
@Observable
final class NativeSettingsStore {
    @MainActor
    final class CredentialHandle: Identifiable {
        let id: UUID
        fileprivate let ownerID: UUID

        fileprivate init(id: UUID, ownerID: UUID) {
            self.id = id
            self.ownerID = ownerID
        }
    }

    @MainActor
    final class CredentialPresentation: Identifiable {
        let id: UUID
        let request: SettingsActionRequest
        let reference: SettingsActionReference
        fileprivate let handle: CredentialHandle
        fileprivate let deadline: ContinuousClock.Instant
        fileprivate weak var field: NativeCredentialField?

        fileprivate init(handle: CredentialHandle, request: SettingsActionRequest, reference: SettingsActionReference, deadline: ContinuousClock.Instant) {
            id = handle.id
            self.handle = handle
            self.request = request
            self.reference = reference
            self.deadline = deadline
        }
    }

    @MainActor
    final class DeviceHandle: Identifiable {
        let id: UUID
        fileprivate let ownerID: UUID

        fileprivate init(id: UUID, ownerID: UUID) {
            self.id = id
            self.ownerID = ownerID
        }
    }

    struct DeviceOperation: Identifiable {
        let id: UUID
        let handle: DeviceHandle
        let request: SettingsDeviceRequest
        let operation: NativeSettingsOperation
        let retired: Bool
        let approvalID: UUID?
        let approval: NativeSettingsSession.PairingApprovalStatus?
        let approvalWorking: Bool
        let pairingLifecycle: NativeSettingsWire.PairingLifecycle?
        let issue: NativeSettingsSessionError?
        let cleanupIssue: NativeSettingsSessionError?
    }

    private enum DeviceCleanupKind: Equatable { case action, pairing }

    @MainActor
    private final class DeviceContext {
        let handle: DeviceHandle
        let request: SettingsDeviceRequest
        let capture: CredentialCapture
        let ceremony: NativeDeviceCeremony
        var details: NativeSettingsSession.Details?
        var confirmation: NativeSettingsAdmission?
        var approval: NativePairingApprovalAdmission?
        var approvalStatus: NativeSettingsSession.PairingApprovalStatus?
        var approvalIssue: NativeSettingsSessionError?
        var approvalWorking = false
        var invitationPresented = false
        var retired = false
        var pairingLifecycle: NativeSettingsWire.PairingLifecycle?
        var issue: NativeSettingsSessionError?
        var cleanupIssue: NativeSettingsSessionError?
        var cleanupKind: DeviceCleanupKind?

        init(handle: DeviceHandle, request: SettingsDeviceRequest, capture: CredentialCapture, ceremony: NativeDeviceCeremony) {
            self.handle = handle
            self.request = request
            self.capture = capture
            self.ceremony = ceremony
        }
    }

    private struct CredentialCapture {
        let generation: UUID
        let started: ContinuousClock.Instant
        let expiresAt: UInt64
        let deadline: ContinuousClock.Instant
    }

    @MainActor
    private final class CredentialContext {
        let handle: CredentialHandle
        let request: SettingsActionRequest
        let metadata: OwnerSettingsDocument
        let initial: Bool
        let capture: CredentialCapture
        var details: NativeSettingsSession.Details?
        var presentation: CredentialPresentation?
        var retired = false
        var submitted = false
        var issue: NativeCredentialIssue?
        var link: CredentialLink?

        init(handle: CredentialHandle, request: SettingsActionRequest, metadata: OwnerSettingsDocument,
             initial: Bool, capture: CredentialCapture) {
            self.handle = handle
            self.request = request
            self.metadata = metadata
            self.initial = initial
            self.capture = capture
        }
    }

    @MainActor
    private final class CredentialLink {
        let plan: OwnerSettingsPreparedLink
        let capture: CredentialCapture
        var attempted = false
        var state = NativeCredentialLinkState.notSubmitted
        var receipt: OwnerSettingsReceipt?
        var issue: NativeCredentialIssue?

        init(plan: OwnerSettingsPreparedLink, capture: CredentialCapture) {
            self.plan = plan
            self.capture = capture
        }
    }

    struct Action: Sendable {
        let operationID: UUID
        let snapshot: SettingsSnapshot
        let registry: SettingRegistry
        fileprivate let ownerID: UUID
        fileprivate let admission: NativeSettingsAdmission
    }

    @MainActor
    final class AssistantOpenAction: CustomStringConvertible, CustomDebugStringConvertible {
        let requestID: UUID
        fileprivate let ownerID: UUID
        fileprivate let admission: NativeSettingsAssistantOpenAdmission

        nonisolated var description: String { "NativeSettingsAssistantOpenAction([private])" }
        nonisolated var debugDescription: String { description }

        fileprivate init(ownerID: UUID, admission: NativeSettingsAssistantOpenAdmission) {
            self.ownerID = ownerID
            self.admission = admission
            requestID = admission.requestID
        }
    }

    @MainActor
    final class AssistantSendAction: CustomStringConvertible, CustomDebugStringConvertible {
        let turnID: UUID
        fileprivate let ownerID: UUID
        fileprivate let admission: NativeSettingsAssistantSendAdmission

        nonisolated var description: String { "NativeSettingsAssistantSendAction([private])" }
        nonisolated var debugDescription: String { description }

        fileprivate init(ownerID: UUID, admission: NativeSettingsAssistantSendAdmission) {
            self.ownerID = ownerID
            self.admission = admission
            turnID = admission.turnID
        }
    }

    private enum AssistantReadIdentity: Hashable {
        case open(UUID)
        case closeOpen(UUID)
        case poll(UUID)
        case close(UUID)
        case inspect(UUID, UUID, UUID)
        case details(UUID, UUID, UUID, UUID, UInt64, Bool)
    }

    private struct AssistantSendReservation {
        let openRequestID: UUID
        let requestID: UUID
        let turnID: UUID
        var operationIDs: Set<UUID> = []
        var materialized: Set<UUID> = []
        var pending = true
    }

    struct Cleanup: Identifiable, Sendable {
        let id: UUID
        let connectionKey: UUID
        let report: NativeSettingsSession.DrainReport?
        let unlinkedCredentialOperationIDs: [UUID]

        init(id: UUID, connectionKey: UUID, report: NativeSettingsSession.DrainReport?, unlinkedCredentialOperationIDs: [UUID] = []) {
            self.id = id
            self.connectionKey = connectionKey
            self.report = report
            self.unlinkedCredentialOperationIDs = unlinkedCredentialOperationIDs
        }

        var hasIssues: Bool {
            guard let report else { return false }
            return !report.failures.isEmpty || !report.unresolvedOperationIDs.isEmpty || !report.unresolvedAssistantOpenRequestIDs.isEmpty
        }

        var requiresRetention: Bool { hasIssues || !unlinkedCredentialOperationIDs.isEmpty }
    }

    @MainActor
    private final class Owner {
        let id = UUID()
        let connectionKey: UUID
        let connection: OwnerConnection
        let lifecycle: NativeSettingsLifecycle
        var information: NativeSettingsSession.Information?
        var work: Task<Void, Never>?
        var observer: Task<Void, Never>?
        var observerID: UUID?
        var cursor: UInt64?
        var actions: [UUID: Action] = [:]
        var operations: [UUID: NativeSettingsOperation] = [:]
        var submittedDrafts: [UUID: NativeSettingsDraft] = [:]
        var effects: [UUID: Task<Void, Never>] = [:]
        var credentials: [UUID: CredentialContext] = [:]
        var devices: [UUID: DeviceContext] = [:]
        var assistantOpens: [UUID: AssistantOpenAction] = [:]
        var assistantChats: [UUID: NativeSettingsSession.AssistantHandle] = [:]
        var assistantSends: [UUID: AssistantSendReservation] = [:]
        var assistantReads: [AssistantReadIdentity: UUID] = [:]
        var retiredAssistantOpens: Set<UUID> = []
        var devicePage: SettingsDevicePage?
        var devicesReadID: UUID?
        var reloadAfterRead = false
        var refreshRequested = false
        var retirement: Task<Cleanup, Never>?
        var retirementID: UUID?
        var recoveryRetirementPending = false
        var report: NativeSettingsSession.DrainReport?
        var nativeReport: NativeSettingsSession.DrainReport?

        init(lifecycle: NativeSettingsLifecycle, connection: OwnerConnection, connectionKey: UUID) {
            self.lifecycle = lifecycle
            self.connection = connection
            self.connectionKey = connectionKey
        }
    }

    private(set) var information: NativeSettingsSession.Information?
    private(set) var registry: SettingRegistry?
    private(set) var snapshot: SettingsSnapshot?
    private(set) var metadata: OwnerSettingsDocument?
    private(set) var metadataUnavailable = false
    private(set) var draft: NativeSettingsDraft?
    private(set) var draftConflict = false
    private(set) var draftMessage: String?
    private(set) var operations: [NativeSettingsOperation] = []
    private(set) var devicePage: SettingsDevicePage?
    private(set) var devicesLoading = false
    private(set) var deviceIssue: NativeSettingsSessionError?
    private(set) var deviceOperations: [DeviceOperation] = []
    private(set) var credentialPresentation: CredentialPresentation?
    private(set) var credentialOperations: [NativeCredentialOperation] = []
    private(set) var credentialIssue: NativeCredentialIssue?
    private(set) var credentialPresentationIssue: NativeCredentialIssue?
    private(set) var issue: NativeSettingsSessionError?
    private(set) var observedAt: Date?
    private(set) var observationIsStale = true
    private(set) var sessionPresent = false
    private(set) var isOpening = false
    private(set) var isRefreshing = false
    private(set) var isClosed = false
    private(set) var retainedOwnerCount = 0
    private(set) var cleanup: [Cleanup] = []
    private var surfaceVisible = false
    private var applicationActive = false
    @ObservationIgnored private let client: OwnerIPCClient
    @ObservationIgnored private var connection: OwnerConnection
    @ObservationIgnored private var connectionKey: UUID
    @ObservationIgnored private var currentID: UUID?
    @ObservationIgnored private var owners: [UUID: Owner] = [:]
    @ObservationIgnored private var observationGeneration = UUID()
    @ObservationIgnored private var credentialGeneration = UUID()
    @ObservationIgnored private var deviceGeneration = UUID()
    @ObservationIgnored private var devicePageOwnerID: UUID?
    @ObservationIgnored private var devicePageGeneration: UUID?
    @ObservationIgnored private var drainTask: Task<[Cleanup], Never>?
    private static let maximumOwners = 8

    init(client: OwnerIPCClient, connection: OwnerConnection, connectionKey: UUID) {
        self.client = client
        self.connection = connection
        self.connectionKey = connectionKey
    }

    var isForeground: Bool {
        !isClosed && surfaceVisible && applicationActive && NSApplication.shared.isActive
    }

    var canOpen: Bool {
        isForeground && !sessionPresent && retainedOwnerCount < Self.maximumOwners
    }

    var canRefresh: Bool {
        isForeground && sessionPresent && information != nil && !isOpening && !isRefreshing
    }

    var snapshotIsStale: Bool {
        observationIsStale || !isForeground || !sessionPresent || issue != nil || snapshot?.stale == true
    }

    var capacityReached: Bool { retainedOwnerCount >= Self.maximumOwners }
    var actionCapacityReached: Bool { current.map { identityCount($0) >= 64 } ?? false }

    var canEdit: Bool {
        !isClosed && !isOpening && !isRefreshing && draft != nil && !operations.contains(where: \.blocksNewChanges)
            && current.map({ !credentialLinksNeedRecovery($0) && !deviceApprovalsNeedRecovery($0) }) == true
    }

    var canSave: Bool {
        canEdit && !snapshotIsStale && !draftConflict && draft?.isDirty == true
            && draft?.validationMessage == nil && snapshot?.revision == draft?.baseline.revision
            && registry == draft?.registry
            && snapshot?.revision != UInt64(Int64.max) && current.map({ identityCount($0) < 64 }) == true
    }

    var canAct: Bool {
        canRefresh && !snapshotIsStale && current?.effects.isEmpty == true && !hasDeviceCeremony
            && current.map { !deviceApprovalsNeedRecovery($0) } == true
    }

    var canEnterCredential: Bool {
        canAct && !actionCapacityReached && draft?.isDirty == false && metadata != nil
            && metadata?.revision == snapshot?.revision && credentialPresentation == nil
            && !operations.contains(where: \.blocksNewChanges) && current.map({ !credentialLinksNeedRecovery($0) && !deviceApprovalsNeedRecovery($0) }) == true
    }

    var status: String {
        if isClosed { return "App closing" }
        if isOpening { return "Opening the original session" }
        guard sessionPresent else { return "Closed" }
        guard information != nil else { return "Open is not confirmed" }
        if issue == .expired { return "Expired; close before another Open" }
        return "Original Open acknowledged"
    }

    func setSurfaceVisible(_ visible: Bool) {
        guard !isClosed else { return }
        if surfaceVisible != visible { invalidateObservation() }
        if surfaceVisible != visible { retireCredentialPresentations(); retireDeviceCeremonies() }
        surfaceVisible = visible
        current?.lifecycle.setSurfaceVisible(visible)
        if isForeground && observationIsStale { requestRefresh() }
        updateObserver()
    }

    func setApplicationActive(_ active: Bool) {
        guard !isClosed else { return }
        if applicationActive != active { invalidateObservation() }
        if applicationActive != active { retireCredentialPresentations(); retireDeviceCeremonies() }
        applicationActive = active
        current?.lifecycle.setApplicationActive(active)
        if isForeground && observationIsStale { requestRefresh() }
        updateObserver()
    }

    func replaceConnection(_ connection: OwnerConnection, connectionKey: UUID) {
        guard !isClosed else { return }
        retireCurrent()
        self.connection = connection
        self.connectionKey = connectionKey
        clearPresentation()
    }

    func open() {
        guard !isClosed else { issue = .closed; return }
        guard current == nil else { return }
        guard isForeground else { issue = .inactive; return }
        guard owners.count < Self.maximumOwners else { issue = .capacity; return }
        do {
            let lifecycle = try NativeSettingsLifecycle(client: client, connection: connection, connectionKey: connectionKey)
            lifecycle.setApplicationActive(applicationActive)
            lifecycle.setSurfaceVisible(surfaceVisible)
            let admission = try lifecycle.prepareOpen()
            let owner = Owner(lifecycle: lifecycle, connection: connection, connectionKey: connectionKey)
            clearPresentation(preservingDraft: true)
            owners[owner.id] = owner
            currentID = owner.id
            sessionPresent = true
            isOpening = true
            refreshCleanup()
            let generation = observationGeneration
            owner.work = Task { [self, owner, admission] in
                await read(owner, opening: admission, generation: generation)
            }
        } catch { issue = sanitize(error) }
    }

    func refresh() {
        guard canRefresh, let owner = current, owner.work == nil else { return }
        issue = nil
        observationIsStale = true
        isRefreshing = true
        let generation = observationGeneration
        owner.work = Task { [self, owner] in
            await read(owner, opening: nil, generation: generation)
        }
    }

    func edit(_ change: (inout NativeSettingsDraft) -> Void) {
        guard canEdit, var draft else { return }
        change(&draft)
        self.draft = draft
        draftMessage = nil
    }

    func reloadDraft() {
        guard canRefresh, let owner = current, owner.effects.isEmpty else { return }
        owner.reloadAfterRead = true
        if owner.work == nil { refresh() }
    }

    func prepareAssistantOpen() throws -> AssistantOpenAction {
        guard !isClosed, let owner = current, isCurrent(owner), isForeground,
              owner.information != nil, owner.retirement == nil else { throw NativeSettingsSessionError.inactive }
        guard identityCount(owner) < 64, owner.effects.count < 4 else { throw NativeSettingsSessionError.capacity }
        let admission = try owner.lifecycle.prepareAssistantOpen()
        let conflict = identityReserved(owner, id: admission.requestID)
        let action = AssistantOpenAction(ownerID: owner.id, admission: admission)
        owner.assistantOpens[action.requestID] = action
        guard !conflict else { throw NativeSettingsSessionError.requestReused }
        return action
    }

    func acceptAssistantDisclosure(_ chat: NativeSettingsSession.AssistantHandle) throws -> NativeSettingsAssistantConsent {
        let owner = try assistantOwner(chat, live: true)
        return try owner.lifecycle.acceptAssistantDisclosure(chat)
    }

    func prepareAssistantSend(_ chat: NativeSettingsSession.AssistantHandle,
                              consent: NativeSettingsAssistantConsent, text: String) throws -> AssistantSendAction {
        let owner = try assistantOwner(chat, live: true)
        guard identityCount(owner) + 9 <= 64, owner.effects.count < 4 else { throw NativeSettingsSessionError.capacity }
        guard !owner.assistantSends.values.contains(where: { $0.openRequestID == chat.openRequestID && $0.pending }) else {
            throw NativeSettingsSessionError.operationUncertain
        }
        let admission = try owner.lifecycle.prepareAssistantSend(chat, consent: consent, text: text)
        let conflict = identityReserved(owner, id: admission.turnID) || identityReserved(owner, id: admission.requestID)
            || admission.turnID == admission.requestID
        owner.assistantSends[admission.turnID] = AssistantSendReservation(openRequestID: chat.openRequestID,
            requestID: admission.requestID, turnID: admission.turnID)
        guard !conflict else { admission.cancel(); throw NativeSettingsSessionError.requestReused }
        return AssistantSendAction(ownerID: owner.id, admission: admission)
    }

    func openAssistant(_ action: AssistantOpenAction,
                       completion: @escaping @MainActor @Sendable (Result<NativeSettingsSession.AssistantOpenOutcome, NativeSettingsSessionError>) -> Void) throws {
        let owner = try assistantOwner(action, live: true)
        try startAssistantEffect(owner, id: action.requestID, work: {
            try await owner.lifecycle.session.openAssistant(action.admission)
        }, observe: { result in
            if case .opened(let chat) = result {
                guard chat.openRequestID == action.requestID else { throw NativeSettingsSessionError.invalidResponse }
                if let original = owner.assistantChats[chat.opened.chatID], original !== chat { throw NativeSettingsSessionError.requestReused }
                owner.assistantChats[chat.opened.chatID] = chat
            } else {
                switch result {
                case .unavailable: owner.retiredAssistantOpens.insert(action.requestID)
                case .state(let state): self.observeAssistantOpen(owner, action: action, state: state)
                case .opened: break
                }
            }
        }, completion: completion)
    }

    func sendAssistant(_ action: AssistantSendAction,
                       completion: @escaping @MainActor @Sendable (Result<NativeSettingsSession.AssistantSendOutcome, NativeSettingsSessionError>) -> Void) throws {
        guard let owner = owners[action.ownerID], owner.assistantSends[action.turnID]?.requestID == action.admission.requestID,
              owner.assistantChats[action.admission.chat.opened.chatID] === action.admission.chat else {
            throw NativeSettingsSessionError.invalidRequest
        }
        guard isCurrent(owner), isForeground else { throw NativeSettingsSessionError.inactive }
        try startAssistantEffect(owner, id: action.turnID, work: {
            try await owner.lifecycle.session.sendAssistant(action.admission)
        }, observe: { outcome in
            if case .unavailable = outcome {
                owner.assistantSends[action.turnID]?.pending = false
                owner.retiredAssistantOpens.insert(action.admission.chat.openRequestID)
            }
        }, completion: completion)
    }

    func assistantOpenStatus(_ action: AssistantOpenAction,
                             completion: @escaping @MainActor @Sendable (Result<NativeSettingsAssistantWire.OpenState, NativeSettingsSessionError>) -> Void) throws {
        let owner = try assistantOwner(action, live: false)
        try startAssistantEffect(owner, id: UUID(), read: .open(action.requestID), work: { () async throws -> (NativeSettingsAssistantWire.OpenState, NativeSettingsSession.AssistantHandle?) in
            let state = try await owner.lifecycle.session.assistantOpenStatus(action.admission)
            if case .ready = state {
                let recovered = try await owner.lifecycle.session.openAssistant(action.admission)
                if case .opened(let chat) = recovered { return (state, chat) }
            }
            return (state, Optional<NativeSettingsSession.AssistantHandle>.none)
        }, observe: { value in
            if let chat = value.1 { owner.assistantChats[chat.opened.chatID] = chat }
            self.observeAssistantOpen(owner, action: action, state: value.0)
        }, completion: { value in completion(value.map { $0.0 }) })
    }

    func closeAssistantOpen(_ action: AssistantOpenAction,
                            completion: @escaping @MainActor @Sendable (Result<NativeSettingsAssistantWire.OpenState, NativeSettingsSessionError>) -> Void) throws {
        let owner = try assistantOwner(action, live: false)
        owner.lifecycle.retireAssistant(action.admission)
        owner.retiredAssistantOpens.insert(action.requestID)
        try startAssistantEffect(owner, id: UUID(), read: .closeOpen(action.requestID), work: {
            try await owner.lifecycle.session.closeAssistantOpen(action.admission)
        }, observe: { state in self.observeAssistantOpen(owner, action: action, state: state) }, completion: completion)
    }

    func pollAssistant(_ chat: NativeSettingsSession.AssistantHandle,
                       completion: @escaping @MainActor @Sendable (Result<NativeSettingsSession.AssistantObservation, NativeSettingsSessionError>) -> Void) throws {
        let owner = try assistantOwner(chat, live: false)
        try startAssistantEffect(owner, id: UUID(), read: .poll(chat.opened.chatID), work: {
            try await owner.lifecycle.session.pollAssistant(chat)
        }, observe: { value in try self.observeAssistant(owner, observation: value) }, completion: completion)
    }

    func closeAssistant(_ chat: NativeSettingsSession.AssistantHandle,
                        completion: @escaping @MainActor @Sendable (Result<NativeSettingsSession.AssistantObservation, NativeSettingsSessionError>) -> Void) throws {
        let owner = try assistantOwner(chat, live: false)
        guard let original = owner.assistantOpens[chat.openRequestID] else { throw NativeSettingsSessionError.invalidRequest }
        owner.lifecycle.retireAssistant(original.admission)
        owner.retiredAssistantOpens.insert(chat.openRequestID)
        try startAssistantEffect(owner, id: UUID(), read: .close(chat.opened.chatID), work: {
            try await owner.lifecycle.session.closeAssistant(chat)
        }, observe: { value in try self.observeAssistant(owner, observation: value) }, completion: completion)
    }

    func inspectAssistantOperation(_ original: NativeSettingsSession.AssistantOperationHandle,
                                   completion: @escaping @MainActor @Sendable (Result<NativeSettingsAssistantWire.OperationLookup, NativeSettingsSessionError>) -> Void) throws {
        let owner = try assistantOwner(original.chat, live: false)
        try requireAssistantMembership(owner, original: original)
        try startAssistantEffect(owner, id: UUID(), read: .inspect(original.chat.opened.chatID, original.turnID, original.operationID), work: {
            try await owner.lifecycle.session.inspectAssistantOperation(original)
        }, observe: { value in
            if case .inspected(let operation) = value {
                if owner.operations[original.operationID] != nil {
                    try owner.operations[original.operationID]?.adopt(operation)
                    self.publishOperations(owner)
                }
                if self.isCurrent(owner) { self.invalidateObservation(); self.requestRefresh() }
            }
        }, completion: completion)
    }

    func assistantActionDetails(_ original: NativeSettingsSession.AssistantOperationHandle, reference: NativeActionReference,
                                completion: @escaping @MainActor @Sendable (Result<NativeSettingsSession.AssistantDetailsReceipt, NativeSettingsSessionError>) -> Void) throws {
        let owner = try assistantOwner(original.chat, live: true)
        try requireAssistantMembership(owner, original: original)
        let confirmation: Bool
        switch reference {
        case .confirmation: confirmation = true
        case .action: confirmation = false
        }
        try startAssistantEffect(owner, id: UUID(), read: .details(original.chat.opened.chatID, original.turnID, original.operationID, reference.id, reference.expiresAt, confirmation), work: {
            try await owner.lifecycle.session.assistantActionDetails(original, reference: reference)
        }, observe: { value in try self.adoptAssistantDetails(owner, receipt: value) }, completion: completion)
    }

    private func assistantOwner(_ action: AssistantOpenAction, live: Bool) throws -> Owner {
        guard let owner = owners[action.ownerID], owner.assistantOpens[action.requestID] === action,
              owner.retirement == nil else { throw NativeSettingsSessionError.closed }
        if live, !isCurrent(owner) || !isForeground { throw NativeSettingsSessionError.inactive }
        return owner
    }

    private func assistantOwner(_ chat: NativeSettingsSession.AssistantHandle, live: Bool) throws -> Owner {
        guard let owner = owners.values.first(where: { $0.assistantChats[chat.opened.chatID] === chat }),
              owner.assistantOpens[chat.openRequestID] != nil, owner.retirement == nil else { throw NativeSettingsSessionError.closed }
        if live {
            guard isCurrent(owner), isForeground else { throw NativeSettingsSessionError.inactive }
            guard !owner.retiredAssistantOpens.contains(chat.openRequestID) else { throw NativeSettingsSessionError.closed }
        }
        return owner
    }

    private func requireAssistantMembership(_ owner: Owner, original: NativeSettingsSession.AssistantOperationHandle) throws {
        guard let turn = owner.assistantSends[original.turnID], turn.openRequestID == original.chat.openRequestID,
              turn.operationIDs.contains(original.operationID) else { throw NativeSettingsSessionError.invalidRequest }
    }

    private func observeAssistantOpen(_ owner: Owner, action: AssistantOpenAction, state: NativeSettingsAssistantWire.OpenState) {
        switch state {
        case .closing, .closed, .unavailable, .unknown: owner.retiredAssistantOpens.insert(action.requestID)
        case .ready, .notAccepted: break
        }
    }

    private func observeAssistant(_ owner: Owner, observation: NativeSettingsSession.AssistantObservation) throws {
        let status = observation.status
        guard owner.assistantChats[status.chatID] != nil else { throw NativeSettingsSessionError.invalidResponse }
        if let turnID = status.turnID {
            guard let turn = owner.assistantSends[turnID], owner.assistantChats[status.chatID]?.openRequestID == turn.openRequestID else {
                throw NativeSettingsSessionError.invalidResponse
            }
            let members = turn.operationIDs.union(status.operationIDs)
            guard members.count <= 8 else { throw NativeSettingsSessionError.capacity }
            for id in members where !turn.operationIDs.contains(id) {
                guard !identityReserved(owner, id: id) else { throw NativeSettingsSessionError.requestReused }
            }
            owner.assistantSends[turnID]?.operationIDs = members
            switch status.phase {
            case .completed:
                owner.assistantSends[turnID]?.pending = false
                if turn.pending, isCurrent(owner) { invalidateObservation(); requestRefresh() }
            case .closed, .unavailable, .stoppedUncertain, .stoppedUnusable:
                owner.assistantSends[turnID]?.pending = false
                owner.retiredAssistantOpens.insert(turn.openRequestID)
            case .closing, .unknown: owner.retiredAssistantOpens.insert(turn.openRequestID)
            case .ready, .running: break
            }
        }
    }

    private func adoptAssistantDetails(_ owner: Owner, receipt: NativeSettingsSession.AssistantDetailsReceipt) throws {
        let original = receipt.original
        guard isCurrent(owner), isForeground, owner.assistantChats[original.chat.opened.chatID] === original.chat else {
            throw NativeSettingsSessionError.inactive
        }
        try requireAssistantMembership(owner, original: original)
        let id = original.operationID
        let details = receipt.details
        if owner.assistantSends[original.turnID]?.materialized.contains(id) == true {
            guard owner.operations[id]?.target == details.target else { throw NativeSettingsSessionError.requestReused }
            try owner.operations[id]?.adopt(details.operation)
            publishOperations(owner)
            return
        }
        guard owner.operations[id] == nil, let snapshot, let registry, !snapshotIsStale,
              snapshot.scope == details.target.scope,
              details.target.expectedRevision == nil || details.target.expectedRevision == snapshot.revision else {
            invalidateObservation()
            requestRefresh()
            throw NativeSettingsSessionError.operationUncertain
        }
        var operation = NativeSettingsOperation(id: id, target: details.target, originalSnapshot: snapshot, originalRegistry: registry)
        try operation.adopt(details.operation)
        operation.wasSubmitted = true
        owner.operations[id] = operation
        owner.assistantSends[original.turnID]?.materialized.insert(id)
        publishOperations(owner)
    }

    private func startAssistantEffect<Value: Sendable>(_ owner: Owner, id: UUID, read: AssistantReadIdentity? = nil,
        work: @escaping @MainActor @Sendable () async throws -> Value,
        observe: @escaping @MainActor @Sendable (Value) throws -> Void,
        completion: @escaping @MainActor @Sendable (Result<Value, NativeSettingsSessionError>) -> Void) throws {
        guard !isClosed, owner.retirement == nil, owner.effects.count < 4, owner.effects[id] == nil else {
            throw NativeSettingsSessionError.capacity
        }
        if let read {
            guard owner.assistantReads[read] == nil else { throw NativeSettingsSessionError.capacity }
            guard !identityReserved(owner, id: id) else { throw NativeSettingsSessionError.requestReused }
            owner.assistantReads[read] = id
        }
        owner.effects[id] = Task { [self, owner] in
            let result: Result<Value, NativeSettingsSessionError>
            do {
                let value = try await work()
                try observe(value)
                result = .success(value)
            } catch { result = .failure(sanitize(error)) }
            owner.effects[id] = nil
            if let read { owner.assistantReads.removeValue(forKey: read) }
            completion(result)
        }
    }

    func save() {
        guard canSave, let draft else { return }
        do {
            let patch = try draft.patch(requestID: UUID())
            let action = try prepareAction(.update(patch))
            perform(action)
        } catch { draftMessage = sanitize(error).errorDescription }
    }

    func prepareAction(_ intent: NativeSettingsIntent) throws -> Action {
        guard !isClosed else { throw NativeSettingsSessionError.closed }
        guard canAct, let owner = current, let information = owner.information,
              let snapshot, let registry else { throw NativeSettingsSessionError.inactive }
        guard identityCount(owner) < 64 else { throw NativeSettingsSessionError.capacity }
        guard !owner.operations.values.contains(where: \.blocksNewChanges), !credentialLinksNeedRecovery(owner), !deviceApprovalsNeedRecovery(owner) else {
            throw NativeSettingsSessionError.operationUncertain
        }
        let target: SettingsActionTarget
        switch intent {
        case .update(let request):
            guard snapshot.revision < UInt64(Int64.max) else { throw NativeSettingsSessionError.invalidRequest }
            for change in request.changes {
                guard let descriptor = registry.descriptors.first(where: { $0.key == change.key }) else { throw NativeSettingsSessionError.invalidRequest }
                if case .optionalDimensions(let choices) = descriptor.constraints {
                    guard case .dimensions(let value) = change.value, let choices,
                          value == nil || choices.contains(where: { $0 == value }) else { throw NativeSettingsSessionError.invalidRequest }
                }
            }
            target = .patch(request: request)
        case .requestAction(let request): target = .native(request: request)
        case .pairDevice(let request), .revokeDevice(let request): target = .device(request: request)
        default: throw NativeSettingsSessionError.invalidRequest
        }
        try target.validate()
        guard target.scope == snapshot.scope,
              target.expectedRevision == nil || target.expectedRevision == snapshot.revision,
              !identityReserved(owner, id: target.requestID),
              !owner.devices.values.contains(where: { $0.approval?.review.operationID == target.requestID }) else {
            throw NativeSettingsSessionError.invalidRequest
        }
        var submittedDraft: NativeSettingsDraft?
        if case .update(let patch) = intent, let draft, draft.isDirty, draft.validationMessage == nil,
           try draft.patch(requestID: patch.requestID) == patch { submittedDraft = draft }
        let admission = try owner.lifecycle.prepare(intent, expiresAt: information.expiresAt)
        let action = Action(operationID: target.requestID, snapshot: snapshot, registry: registry, ownerID: owner.id, admission: admission)
        owner.actions[target.requestID] = action
        owner.operations[target.requestID] = NativeSettingsOperation(id: target.requestID, target: target,
            originalSnapshot: snapshot, originalRegistry: registry)
        owner.submittedDrafts[target.requestID] = submittedDraft
        publishOperations(owner)
        return action
    }

    func perform(_ action: Action) {
        guard let owner = current, owner.id == action.ownerID,
              let original = owner.actions[action.operationID], original.admission === action.admission,
              owner.effects[action.operationID] == nil, owner.effects.count < 4,
              owner.operations[action.operationID]?.wasSubmitted == false,
              owner.operations[action.operationID]?.cancelledBeforeSubmission == false else { return }
        owner.operations[action.operationID]?.wasSubmitted = true
        startEffect(owner, id: action.operationID) {
            try await owner.lifecycle.session.perform(action.admission)
        }
    }

    func confirm(_ id: UUID) {
        guard canAct, let owner = current, let operation = owner.operations[id],
              operation.issue == nil, operation.target.expectedRevision == snapshot?.revision,
              let reference = operation.confirmation, case .patch = operation.target else { return }
        do {
            let invocation = NativeActionInvocation(operationID: id, nativeRequestID: UUID(), reference: .confirmation(reference: reference))
            let admission = try owner.lifecycle.prepare(.confirmPatch(invocation), expiresAt: reference.expiresAt)
            owner.operations[id]?.executionPrepared = true
            startEffect(owner, id: id) { try await owner.lifecycle.session.perform(admission) }
        } catch { setOperationIssue(sanitize(error), owner: owner, id: id) }
    }

    func inspect(_ id: UUID) { recover(id, reconciliation: false, cancellation: false) }
    func reconcile(_ id: UUID) { recover(id, reconciliation: true, cancellation: false) }
    func cancel(_ id: UUID) { recover(id, reconciliation: false, cancellation: true) }

    func beginCredentialEntry(_ choice: NativeCredentialChoice) {
        guard canEnterCredential, let owner = current, let metadata, let snapshot else { return }
        do {
            let capture = try captureCredential(owner)
            let binding: SettingsCredentialBinding
            let initial: Bool
            switch choice {
            case .selectedModel(let storage):
                guard let model = metadata.defaultModel,
                      metadata.modelCredentials.count < 16,
                      !metadata.modelCredentials.contains(where: { $0.provider == model.provider && $0.endpoint == model.endpoint }) else {
                    throw OwnerSettingsMetadataError.bindingConflict
                }
                binding = .model(binding: SettingsModelCredentialBinding(provider: model.provider, endpoint: model.endpoint, reference: UUID(), storage: storage))
                initial = true
            case .configuredModel(let configured):
                guard metadata.modelCredentials.contains(configured) else { throw OwnerSettingsMetadataError.bindingConflict }
                binding = .model(binding: configured)
                initial = false
            case .systemOne:
                guard let configured = metadata.systemOne else { throw OwnerSettingsMetadataError.bindingConflict }
                binding = .systemOne(endpoint: configured.endpoint, reference: configured.reference ?? UUID())
                initial = configured.reference == nil
            }
            guard !initial || metadata.revision < UInt64(Int64.max) else { throw OwnerSettingsMetadataError.revisionExhausted }
            guard identityCount(owner) + (initial ? 2 : 1) <= 64 else { throw NativeSettingsSessionError.capacity }
            let request = SettingsActionRequest(contractVersion: 1, requestID: UUID(), scope: snapshot.scope,
                expectedRevision: snapshot.revision, action: .credentialEntry(binding: binding))
            let action = try prepareAction(.requestAction(request))
            let handle = CredentialHandle(id: request.requestID, ownerID: owner.id)
            owner.credentials[request.requestID] = CredentialContext(handle: handle, request: request,
                metadata: metadata, initial: initial, capture: capture)
            credentialIssue = nil
            credentialPresentationIssue = nil
            perform(action)
            publishCredentials()
        } catch { credentialIssue = credentialError(error) }
    }

    func attachCredentialField(_ field: NativeCredentialField, to presentation: CredentialPresentation) {
        guard let (owner, context) = credential(presentation.handle), context.presentation === presentation,
              !context.retired, !context.submitted else { field.clear(); return }
        do {
            try validateCredential(context.capture, owner: owner)
            try validateCredentialReference(presentation)
            presentation.field = field
        } catch { field.clear(); dismissCredential(presentation) }
    }

    func submitCredential(_ presentation: CredentialPresentation, secret: String) {
        presentation.field?.clear()
        guard let (owner, context) = credential(presentation.handle), context.presentation === presentation,
              !context.retired, !context.submitted, owner.effects[context.handle.id] == nil,
              owner.effects.count < 4 else { return }
        guard !snapshotIsStale, snapshot?.scope == context.request.scope, snapshot?.revision == context.request.expectedRevision else {
            context.issue = .staleContext
            credentialPresentationIssue = .staleContext
            publishCredentials()
            return
        }
        do {
            try validateCredential(context.capture, owner: owner)
            try validateCredentialReference(presentation)
            try SettingsWire.text(secret, maximum: 8192)
            guard let details = context.details, details.target == .native(request: context.request),
                  details.operation.phase == .awaitingAction(reference: presentation.reference),
                  owner.operations[context.handle.id]?.inspection?.phase == details.operation.phase else {
                throw NativeSettingsSessionError.invalidResponse
            }
            let invocation = NativeActionInvocation(operationID: context.handle.id, nativeRequestID: UUID(), reference: .action(reference: presentation.reference))
            let admission = try owner.lifecycle.prepare(.submitCredential(invocation), expiresAt: presentation.reference.expiresAt)
            context.issue = nil
            credentialPresentationIssue = nil
            context.submitted = true
            owner.operations[context.handle.id]?.executionPrepared = true
            credentialPresentation = nil
            context.presentation = nil
            startEffect(owner, id: context.handle.id) {
                try await owner.lifecycle.session.submitCredential(admission, secret: secret)
            }
        } catch is SettingsWireError {
            context.issue = .invalidSecret
            credentialPresentationIssue = .invalidSecret
            publishCredentials()
        } catch {
            context.issue = credentialError(error)
            credentialPresentationIssue = context.issue
            publishCredentials()
        }
    }

    func dismissCredential(_ presentation: CredentialPresentation) {
        presentation.field?.clear()
        guard let (owner, context) = credential(presentation.handle), context.presentation === presentation else { return }
        context.retired = true
        context.presentation = nil
        if credentialPresentation === presentation { credentialPresentation = nil }
        if !context.submitted, owner.effects[context.handle.id] == nil, isCurrent(owner) {
            recover(context.handle.id, reconciliation: false, cancellation: true)
        }
        publishCredentials()
    }

    func expireCredentialPresentation(_ presentation: CredentialPresentation) {
        guard let (owner, context) = credential(presentation.handle) else { presentation.field?.clear(); return }
        do { try validateCredential(context.capture, owner: owner); try validateCredentialReference(presentation) }
        catch { dismissCredential(presentation) }
    }

    func canLinkCredential(_ handle: CredentialHandle) -> Bool {
        guard canAct, let (owner, context) = credential(handle), isCurrent(owner), context.initial,
              context.link == nil, identityCount(owner) <= 64,
              !owner.operations.values.contains(where: \.blocksNewChanges), !credentialLinksNeedRecovery(owner),
              let completed = owner.operations[handle.id]?.inspection,
              case .finished(let result) = completed.phase, case .completed = result.state,
              snapshot?.revision == context.metadata.revision else { return false }
        return true
    }

    func linkCredentialMetadata(_ handle: CredentialHandle) {
        guard canLinkCredential(handle), let (owner, context) = credential(handle),
              let details = context.details, let completed = owner.operations[handle.id]?.inspection,
              case .awaitingAction(let reference) = details.operation.phase else { return }
        do {
            let capture = try captureCredential(owner, expiry: context.capture.expiresAt, deadline: context.capture.deadline)
            try validateCredential(capture, owner: owner)
            let requestID = OwnerID()
            guard !identityReserved(owner, id: requestID.value),
                  !owner.credentials.values.contains(where: { $0.link?.plan.ownerRequest.id == requestID }) else {
                throw NativeSettingsSessionError.requestReused
            }
            let plan = try context.metadata.credentialMetadataPlan(request: context.request, admittedTarget: details.target,
                admittedOperation: details.operation, originalReference: reference, completed: completed,
                nativeSessionID: details.operation.binding.clientSessionID, ownerRequestID: requestID)
            guard case .initialEnrollment(let prepared) = plan else { throw OwnerSettingsMetadataError.invalidEnrollment }
            let link = CredentialLink(plan: prepared, capture: capture)
            context.link = link
            runCredentialLink(owner, context: context, link: link)
        } catch { context.issue = credentialError(error); publishCredentials() }
    }

    func inspectCredentialLink(_ handle: CredentialHandle) { recoverCredentialLink(handle, reconcile: false) }
    func reconcileCredentialLink(_ handle: CredentialHandle) { recoverCredentialLink(handle, reconcile: true) }

    func canRecoverCredentialLink(_ handle: CredentialHandle) -> Bool {
        guard !isClosed, let (owner, context) = credential(handle), let link = context.link else { return false }
        return link.attempted && !owner.recoveryRetirementPending
            && owner.effects.count < 4 && owner.effects[link.plan.ownerRequest.id.value] == nil
    }

    func hostAvailabilityChanged(_ available: Bool) {
        guard !isClosed else { return }
        invalidateObservation()
        stopObserver()
        if !available { retireCredentialPresentations(); retireDeviceCeremonies() }
        if available { requestRefresh() }
    }

    func close() {
        retireCurrent()
    }

    func invalidate() {
        guard !isClosed else { return }
        isClosed = true
        surfaceVisible = false
        applicationActive = false
        retireCurrent()
        for owner in owners.values { owner.lifecycle.invalidate() }
    }

    func drain() async -> [Cleanup] {
        invalidate()
        if let drainTask { return await drainTask.value }
        let retained = Array(owners.values)
        let tasks = retained.compactMap(\.retirement)
        let effects = retained.flatMap { $0.effects.values }
        let task = Task { [self] in
            for task in tasks { _ = await task.value }
            for task in effects { await task.value }
            return retained.map { owner in
                Cleanup(id: owner.id, connectionKey: owner.connectionKey,
                        report: owner.nativeReport.map { credentialCleanup(owner, native: $0) },
                        unlinkedCredentialOperationIDs: unlinkedCredentialIDs(owner))
            }
        }
        drainTask = task
        return await task.value
    }

    var devicesAreStale: Bool {
        snapshotIsStale || deviceIssue != nil || devicesLoading || devicePageOwnerID != currentID || devicePageGeneration != observationGeneration
    }

    var canRefreshDevices: Bool {
        canRefresh && current.map { $0.effects.count < 4 && $0.devicesReadID == nil } == true
    }

    var canBeginDevicePairing: Bool {
        canBeginDeviceAction && current.map { identityCount($0) + 2 <= 64 && $0.devices.values.filter { $0.request.action == .pair }.count < 8 } == true
    }

    func refreshDevices(first: Bool = true) {
        guard canRefreshDevices, let owner = current else { return }
        let after: UUID?
        if first { after = nil }
        else {
            guard !devicesAreStale, let next = owner.devicePage?.nextAfter else { return }
            after = next
        }
        let id = UUID()
        guard owner.effects[id] == nil, !identityReserved(owner, id: id),
              !owner.devices.values.contains(where: { $0.approval?.review.operationID == id }),
              !owner.credentials.values.contains(where: { $0.link?.plan.ownerRequest.id.value == id }) else {
            deviceIssue = .requestReused
            return
        }
        let generation = observationGeneration
        owner.devicesReadID = id
        devicesLoading = true
        deviceIssue = nil
        owner.effects[id] = Task { [self, owner] in
            do {
                let page = try await owner.lifecycle.session.devices(after: after, limit: 25)
                guard page.hostID == owner.information?.hostID, page.devices.count <= 25 else { throw NativeSettingsSessionError.invalidResponse }
                if isCurrent(owner), generation == observationGeneration, isForeground {
                    owner.devicePage = page
                    devicePage = page
                    devicePageOwnerID = owner.id
                    devicePageGeneration = generation
                }
            } catch { if isCurrent(owner) { deviceIssue = sanitize(error) } }
            owner.effects[id] = nil
            if owner.devicesReadID == id { owner.devicesReadID = nil }
            if isCurrent(owner) { devicesLoading = false }
        }
    }

    func beginDevicePairing(parent: NSWindow?) {
        guard canBeginDevicePairing else { return }
        beginDeviceAction(.pair, parent: parent)
    }

    func canRevokeDevice(_ row: SettingsDeviceSummary) -> Bool {
        canBeginDeviceAction && !devicesAreStale && devicePage?.devices.contains(row) == true
            && current.map { identityCount($0) < 64 } == true
    }

    func requestDeviceRevocation(_ row: SettingsDeviceSummary, parent: NSWindow?) {
        guard canRevokeDevice(row) else { return }
        beginDeviceAction(.revoke(deviceID: row.deviceID, publicKeys: row.publicKeys), parent: parent)
    }

    func confirmDeviceAction(_ handle: DeviceHandle, ceremony: NativeDeviceCeremony) {
        guard let (owner, context) = device(handle), context.ceremony === ceremony,
              canContinueDevice(owner, context: context), context.confirmation == nil,
              owner.effects[handle.id] == nil, let details = context.details,
              case .awaitingAction(let reference) = details.operation.phase,
              owner.operations[handle.id]?.inspection?.phase == details.operation.phase,
              context.request.action == .pair || ceremony.compared else { return }
        do {
            try validateDevice(context.capture, owner: owner)
            try ceremony.validate()
            let invocation = NativeActionInvocation(operationID: handle.id, nativeRequestID: UUID(), reference: .action(reference: reference))
            let admission = try owner.lifecycle.prepare(.confirmDevice(invocation), expiresAt: reference.expiresAt)
            context.confirmation = admission
            ceremony.confirming()
            owner.operations[handle.id]?.executionPrepared = true
            owner.operations[handle.id]?.isWorking = true
            context.issue = nil
            publishDevices()
            owner.effects[handle.id] = Task { [self, owner, context, admission] in
                do {
                    let result = try await owner.lifecycle.session.perform(admission)
                    switch result {
                    case .prepared: throw NativeSettingsSessionError.invalidResponse
                    case .inspected(let inspection): try owner.operations[handle.id]?.adopt(inspection)
                    case .pairing(let status):
                        context.pairingLifecycle = status.lifecycle
                        try owner.operations[handle.id]?.adopt(status.operation)
                        guard context.request.action == .pair, status.invitationAvailable else { throw NativeSettingsSessionError.expired }
                        context.invitationPresented = true
                        publishOperations(owner)
                        let final = try await owner.lifecycle.session.showInvitation(admission) { [context] presentation in
                            try await context.ceremony.showInvitation(presentation)
                        }
                        try adoptPairingRetirement(final, owner: owner, context: context)
                    }
                    if !context.invitationPresented {
                        guard case .finished = owner.operations[handle.id]?.inspection?.phase else { throw NativeSettingsSessionError.operationUncertain }
                        context.ceremony.completed()
                    }
                } catch {
                    let original = sanitize(error)
                    context.issue = original
                    context.ceremony.failure(original)
                    if context.request.action == .pair {
                        context.cleanupKind = .pairing
                        context.retired = true
                        context.ceremony.retire()
                        context.approval?.cancel()
                        admission.cancel()
                        publishDevices()
                        await context.ceremony.waitForRetirement()
                        do {
                            let cancellation = try await owner.lifecycle.session.cancelPairing(operationID: handle.id)
                            try adoptPairingRetirement(cancellation, owner: owner, context: context)
                        } catch { context.cleanupIssue = sanitize(error) }
                    }
                    owner.operations[handle.id]?.issue = original
                }
                owner.operations[handle.id]?.isWorking = false
                owner.effects[handle.id] = nil
                publishOperations(owner)
                if isCurrent(owner) { invalidateObservation(); requestRefresh() }
            }
        } catch { context.issue = sanitize(error); context.ceremony.failure(sanitize(error)); publishDevices() }
    }

    func approveDevicePairing(_ handle: DeviceHandle, ceremony: NativeDeviceCeremony) {
        guard let (owner, context) = device(handle), context.ceremony === ceremony,
              context.request.action == .pair, context.invitationPresented,
              context.approval == nil, canContinueDevice(owner, context: context),
              owner.effects[handle.id] != nil else { return }
        do {
            try validateDevice(context.capture, owner: owner)
            let choice = try ceremony.takeChoice()
            let admission: NativePairingApprovalAdmission
            do {
                admission = try NativePairingApprovalAdmission.capture(session: owner.lifecycle.session,
                    creationOperationID: handle.id, candidate: choice.candidate, label: choice.label,
                    expectedFingerprint: choice.fingerprint, access: choice.access)
            } catch { choice.candidate.discard(); throw error }
            let id = admission.review.operationID
            guard !identityReserved(owner, id: id), owner.effects[id] == nil,
                  !owner.devices.values.contains(where: { $0.approval?.review.operationID == id }),
                  !owner.credentials.values.contains(where: { $0.link?.plan.ownerRequest.id.value == id }) else {
                admission.cancel()
                throw NativeSettingsSessionError.requestReused
            }
            context.approval = admission
            context.approvalIssue = nil
            runPairingApproval(owner, context: context, inspecting: false)
        } catch { context.approvalIssue = sanitize(error); ceremony.failure(sanitize(error)); publishDevices() }
    }

    func inspectPairingApproval(_ handle: DeviceHandle) {
        guard let (owner, context) = device(handle), isCurrent(owner), owner.retirement == nil,
              let admission = context.approval, owner.effects[admission.review.operationID] == nil,
              owner.effects.count < 4 else { return }
        runPairingApproval(owner, context: context, inspecting: true)
    }

    func inspectDeviceOperation(_ handle: DeviceHandle) {
        guard let (owner, context) = device(handle), isCurrent(owner), owner.retirement == nil else { return }
        if context.cleanupIssue != nil, context.cleanupKind == .pairing {
            startEffect(owner, id: handle.id) { .pairing(try await owner.lifecycle.session.pairing(operationID: handle.id)) }
        } else { recover(handle.id, reconciliation: false, cancellation: false) }
    }

    func canInspectDeviceOperation(_ handle: DeviceHandle) -> Bool {
        guard let (owner, _) = device(handle), isCurrent(owner), owner.retirement == nil else { return false }
        return owner.effects.count < 4 && owner.effects[handle.id] == nil
    }

    func canInspectPairingApproval(_ handle: DeviceHandle) -> Bool {
        guard let (owner, context) = device(handle), isCurrent(owner), owner.retirement == nil,
              let admission = context.approval else { return false }
        return owner.effects.count < 4 && owner.effects[admission.review.operationID] == nil
    }

    func devicesSurfaceHidden() { retireDeviceCeremonies() }

    func dismissDeviceCeremony(_ handle: DeviceHandle) {
        guard let (owner, context) = device(handle) else { return }
        guard !context.retired else { context.ceremony.retire(); return }
        context.retired = true
        context.ceremony.retire()
        context.approval?.cancel()
        if !context.invitationPresented { context.confirmation?.cancel() }
        if context.confirmation == nil, owner.effects[handle.id] == nil, isCurrent(owner) {
            recover(handle.id, reconciliation: false, cancellation: true)
        }
        publishDevices()
    }

    private var hasDeviceCeremony: Bool {
        owners.values.contains { $0.devices.values.contains { !$0.ceremony.uiRetired } }
    }

    private var canBeginDeviceAction: Bool {
        canAct && draft?.isDirty == false && credentialPresentation == nil
            && !operations.contains(where: \.blocksNewChanges)
            && current.map { !credentialLinksNeedRecovery($0) && !deviceApprovalsNeedRecovery($0) } == true
    }

    private func beginDeviceAction(_ action: SettingsDeviceAction, parent: NSWindow?) {
        guard canBeginDeviceAction, let owner = current, let snapshot else { return }
        do {
            let parent = try NativeDeviceCeremony.requireParent(parent)
            let capture = try captureDevice(owner)
            let request = SettingsDeviceRequest(contractVersion: 1, requestID: UUID(), scope: snapshot.scope, action: action)
            let prepared = try prepareAction(action == .pair ? .pairDevice(request) : .revokeDevice(request))
            let handle = DeviceHandle(id: request.requestID, ownerID: owner.id)
            let ceremony = NativeDeviceCeremony(store: self, handle: handle, request: request, parent: parent,
                                                expiresAt: capture.expiresAt, deadline: capture.deadline)
            let context = DeviceContext(handle: handle, request: request, capture: capture, ceremony: ceremony)
            owner.devices[handle.id] = context
            do { try ceremony.present() }
            catch {
                prepared.admission.cancel()
                owner.operations[handle.id]?.cancelledBeforeSubmission = true
                context.retired = true
                ceremony.retire()
                throw error
            }
            deviceIssue = nil
            perform(prepared)
            publishDevices()
        } catch { deviceIssue = sanitize(error); publishDevices() }
    }

    private func device(_ handle: DeviceHandle) -> (Owner, DeviceContext)? {
        guard let owner = owners[handle.ownerID], let context = owner.devices[handle.id], context.handle === handle else { return nil }
        return (owner, context)
    }

    private func captureDevice(_ owner: Owner) throws -> CredentialCapture {
        guard isCurrent(owner), isForeground, let information = owner.information else { throw NativeSettingsSessionError.inactive }
        let now = try NativeSettingsGate.wallTime()
        guard information.expiresAt > now else { throw NativeSettingsSessionError.expired }
        let started = ContinuousClock.now
        return CredentialCapture(generation: deviceGeneration, started: started, expiresAt: information.expiresAt,
            deadline: started.advanced(by: .milliseconds(Int64(min(information.expiresAt - now, 300_000)))))
    }

    private func validateDevice(_ capture: CredentialCapture, owner: Owner) throws {
        guard isCurrent(owner), isForeground, capture.generation == deviceGeneration else { throw NativeSettingsSessionError.inactive }
        guard try NativeSettingsGate.wallTime() < capture.expiresAt, ContinuousClock.now < capture.deadline else {
            throw NativeSettingsSessionError.expired
        }
    }

    private func canContinueDevice(_ owner: Owner, context: DeviceContext) -> Bool {
        isCurrent(owner) && isForeground && !context.retired && owner.effects.count < 4
            && !credentialLinksNeedRecovery(owner)
            && !owner.devices.values.contains(where: { $0 !== context && deviceApprovalNeedsRecovery($0) })
            && !owner.operations.contains { $0.key != context.handle.id && $0.value.blocksNewChanges }
            && owner.effects.keys.allSatisfy { $0 == context.handle.id }
    }

    private func prepareDeviceConfirmation(_ owner: Owner, context: DeviceContext) async throws {
        guard let inspection = owner.operations[context.handle.id]?.inspection,
              case .awaitingAction(let reference) = inspection.phase else { return }
        let details = try await owner.lifecycle.session.details(operationID: context.handle.id, reference: .action(reference: reference))
        guard details.target == .device(request: context.request), details.operation.binding == inspection.binding,
              details.operation.origin == inspection.origin, details.operation.phase == .awaitingAction(reference: reference),
              case .nativeUI(let sessionID) = details.operation.origin,
              sessionID == details.operation.binding.clientSessionID else { throw NativeSettingsSessionError.invalidResponse }
        context.details = details
        do {
            try validateDevice(context.capture, owner: owner)
            guard !context.retired else { throw NativeSettingsSessionError.inactive }
            let lifetime = reference.expiresAt - details.operation.binding.issuedAt
            let deadline = min(context.capture.deadline, context.capture.started.advanced(by: .milliseconds(Int64(min(lifetime, 300_000)))))
            try context.ceremony.confirmation(expiresAt: reference.expiresAt, deadline: deadline)
        } catch {
            let original = error
            context.retired = true
            context.issue = sanitize(original)
            context.cleanupKind = .action
            context.ceremony.retire()
            do {
                let cancelled = try await owner.lifecycle.session.cancel(operationID: context.handle.id)
                try owner.operations[context.handle.id]?.adopt(cancelled)
                if case .finished(let result) = cancelled.phase, result.state != .unknown { context.cleanupIssue = nil }
                else { context.cleanupIssue = .operationUncertain }
            } catch { context.cleanupIssue = sanitize(error) }
            throw original
        }
    }

    private func adoptPairingRetirement(_ status: NativeSettingsSession.PairingStatus, owner: Owner, context: DeviceContext) throws {
        context.pairingLifecycle = status.lifecycle
        try owner.operations[context.handle.id]?.adopt(status.operation)
        switch status.lifecycle {
        case .cancelled, .expired, .noLongerPending: context.cleanupIssue = nil
        case .retained, .cancellationPending, .unavailable: throw NativeSettingsSessionError.operationUncertain
        }
    }

    private func runPairingApproval(_ owner: Owner, context: DeviceContext, inspecting: Bool) {
        guard let admission = context.approval else { return }
        let id = admission.review.operationID
        guard isCurrent(owner), owner.retirement == nil, owner.effects[id] == nil, owner.effects.count < 4 else { return }
        context.approvalWorking = true
        publishDevices()
        owner.effects[id] = Task { [self, owner, context, admission] in
            do {
                let status: NativeSettingsSession.PairingApprovalStatus
                if inspecting { status = try await owner.lifecycle.session.pairingApproval(admission) }
                else { status = try await owner.lifecycle.session.approvePairing(admission) }
                context.approvalStatus = status
                context.approvalIssue = nil
                publishDevices()
                if status.acceptanceAvailable, !context.retired {
                    _ = try await owner.lifecycle.session.showPairingAcceptance(admission) { [context, admission] presentation in
                        guard presentation.operationID == admission.review.operationID else { throw NativeSettingsSessionError.invalidResponse }
                        try await context.ceremony.showAcceptance(presentation)
                    }
                } else if !context.retired { context.ceremony.failure(.operationUncertain) }
            } catch {
                context.approvalIssue = sanitize(error)
                context.ceremony.failure(sanitize(error))
            }
            context.approvalWorking = false
            owner.effects[id] = nil
            publishDevices()
        }
    }

    private func deviceApprovalsNeedRecovery(_ owner: Owner) -> Bool {
        owner.devices.values.contains { $0.cleanupIssue != nil || deviceApprovalNeedsRecovery($0) }
    }

    private func deviceApprovalNeedsRecovery(_ context: DeviceContext) -> Bool {
        guard context.approval != nil else { return false }
        guard !context.approvalWorking, context.approvalIssue == nil, let status = context.approvalStatus,
              case .finished(let result) = status.operation.phase else { return true }
        return result.state == .unknown
    }

    private func retireDeviceCeremonies() {
        deviceGeneration = UUID()
        for owner in owners.values {
            for context in owner.devices.values { dismissDeviceCeremony(context.handle) }
        }
    }

    private func publishDevices() {
        deviceOperations = owners.values.flatMap { owner in
            owner.devices.values.compactMap { context in
                guard let operation = owner.operations[context.handle.id] else { return nil }
                return DeviceOperation(id: context.handle.id, handle: context.handle, request: context.request,
                    operation: operation, retired: context.retired, approvalID: context.approval?.review.operationID,
                    approval: context.approvalStatus, approvalWorking: context.approvalWorking,
                    pairingLifecycle: context.pairingLifecycle, issue: context.approvalIssue ?? context.issue,
                    cleanupIssue: context.cleanupIssue)
            }
        }.sorted { $0.id.uuidString < $1.id.uuidString }
    }

    private var current: Owner? { currentID.flatMap { owners[$0] } }

    private func read(_ owner: Owner, opening: NativeSettingsAdmission?, generation: UUID) async {
        do {
            if let opening {
                let information = try await owner.lifecycle.session.open(opening)
                guard information.connectionKey == owner.connectionKey else { throw NativeSettingsSessionError.invalidResponse }
                owner.information = information
                if isCurrent(owner) { self.information = information }
            }
            guard let information = owner.information else { throw NativeSettingsSessionError.closed }
            let registry = try await owner.lifecycle.session.describe()
            let snapshot = try await owner.lifecycle.session.snapshot(.host(hostID: information.hostID))
            var metadata: OwnerSettingsDocument?
            do {
                metadata = try OwnerSettingsDocument(settingsResponse: await client.request(.query("settings"), connection: owner.connection), hostID: information.hostID)
                guard metadata?.revision == snapshot.revision else { throw OwnerSettingsMetadataError.bindingConflict }
            } catch { metadata = nil }
            guard isCurrent(owner) else { owner.work = nil; return }
            if let previous = self.snapshot, previous.scope == snapshot.scope, snapshot.revision < previous.revision {
                throw NativeSettingsSessionError.invalidResponse
            }
            self.registry = registry
            self.snapshot = snapshot
            self.metadata = metadata
            metadataUnavailable = metadata == nil
            if let presentation = credentialPresentation,
               presentation.request.scope != snapshot.scope || presentation.request.expectedRevision != snapshot.revision {
                dismissCredential(presentation)
            }
            if owner.reloadAfterRead || draft == nil || draft?.isDirty == false || draftWasSaved(owner, snapshot: snapshot) {
                draft = try NativeSettingsDraft(snapshot: snapshot, registry: registry, systemOne: metadata?.systemOne)
                draftConflict = false
                draftMessage = nil
            } else {
                draftConflict = draft?.baseline.revision != snapshot.revision || draft?.baseline.scope != snapshot.scope || draft?.registry != registry
            }
            owner.reloadAfterRead = false
            observedAt = Date()
            observationIsStale = generation != observationGeneration || !isForeground || snapshot.stale
            issue = nil
        } catch {
            owner.reloadAfterRead = false
            if isCurrent(owner) {
                issue = sanitize(error)
                observationIsStale = true
                retireCredentialPresentations()
                retireDeviceCeremonies()
            }
        }
        owner.work = nil
        if isCurrent(owner) {
            isOpening = false
            isRefreshing = false
            if owner.refreshRequested && issue == nil {
                owner.refreshRequested = false
                requestRefresh()
            } else { updateObserver() }
        }
    }

    private func isCurrent(_ owner: Owner) -> Bool {
        !isClosed && currentID == owner.id && connectionKey == owner.connectionKey
    }

    private func retireCurrent() {
        guard let owner = current else { return }
        retireCredentialPresentations()
        retireDeviceCeremonies()
        owner.lifecycle.invalidate()
        owner.observer?.cancel()
        currentID = nil
        sessionPresent = false
        isOpening = false
        isRefreshing = false
        operations = []
        invalidateObservation()
        let work = owner.work
        let observer = owner.observer
        let effects = Array(owner.effects.values)
        let ceremonies = owner.devices.values.map(\.ceremony)
        let retirementID = UUID()
        owner.retirementID = retirementID
        owner.retirement = Task { [self, owner, work, observer, effects, ceremonies] in
            if let work { await work.value }
            if let observer { await observer.value }
            for ceremony in ceremonies { await ceremony.waitForRetirement() }
            for effect in effects { await effect.value }
            let native = await owner.lifecycle.drain()
            owner.nativeReport = native
            let report = credentialCleanup(owner, native: native)
            let result = Cleanup(id: owner.id, connectionKey: owner.connectionKey, report: report,
                                 unlinkedCredentialOperationIDs: unlinkedCredentialIDs(owner))
            owner.report = report
            if owner.retirementID == retirementID, owner.effects.isEmpty, !result.requiresRetention {
                owners.removeValue(forKey: owner.id)
            }
            refreshCleanup()
            publishCredentials()
            publishDevices()
            return result
        }
        refreshCleanup()
    }

    private func invalidateObservation() {
        observationGeneration = UUID()
        observationIsStale = true
    }

    private func clearPresentation(preservingDraft: Bool = false) {
        information = nil
        if !preservingDraft {
            registry = nil
            snapshot = nil
            metadata = nil
            metadataUnavailable = false
            draft = nil
            draftConflict = false
            observedAt = nil
        }
        draftMessage = nil
        operations = []
        credentialPresentation?.field?.clear()
        credentialPresentation = nil
        credentialIssue = nil
        credentialPresentationIssue = nil
        issue = nil
        devicePage = nil
        devicePageOwnerID = nil
        devicePageGeneration = nil
        devicesLoading = false
        deviceIssue = nil
        invalidateObservation()
    }

    private func refreshCleanup() {
        retainedOwnerCount = owners.count
        cleanup = owners.values.filter { $0.id != currentID }.map {
            Cleanup(id: $0.id, connectionKey: $0.connectionKey, report: $0.report,
                    unlinkedCredentialOperationIDs: unlinkedCredentialIDs($0))
        }.sorted { $0.id.uuidString < $1.id.uuidString }
    }

    private func sanitize(_ error: any Error) -> NativeSettingsSessionError {
        error as? NativeSettingsSessionError ?? .operationUncertain
    }

    private func startEffect(_ owner: Owner, id: UUID,
                             work: @escaping @MainActor @Sendable () async throws -> NativeSettingsSession.Outcome) {
        guard isCurrent(owner), owner.effects[id] == nil, owner.effects.count < 4,
              owner.operations[id] != nil else { return }
        owner.operations[id]?.isWorking = true
        owner.operations[id]?.issue = nil
        publishOperations(owner)
        owner.effects[id] = Task { [self, owner] in
            do {
                let outcome = try await work()
                switch outcome {
                case .prepared(let result):
                    guard result.requestID == id, result.scope == owner.operations[id]?.target.scope else {
                        throw NativeSettingsSessionError.invalidResponse
                    }
                    owner.operations[id]?.prepared = result
                    let inspection = try await owner.lifecycle.session.inspect(operationID: id)
                    try owner.operations[id]?.adopt(inspection)
                case .inspected(let inspection):
                    try owner.operations[id]?.adopt(inspection)
                    if let context = owner.devices[id], context.cleanupKind == .action,
                       case .finished(let result) = inspection.phase, result.state != .unknown { context.cleanupIssue = nil }
                case .pairing(let status):
                    try owner.operations[id]?.adopt(status.operation)
                    if let context = owner.devices[id] {
                        context.pairingLifecycle = status.lifecycle
                        if context.cleanupKind == .pairing {
                            switch status.lifecycle {
                            case .cancelled, .expired, .noLongerPending: context.cleanupIssue = nil
                            case .retained, .cancellationPending, .unavailable: context.cleanupIssue = .operationUncertain
                            }
                        }
                    }
                }
                if let context = owner.credentials[id], context.details == nil {
                    if context.retired, case .awaitingAction = owner.operations[id]?.inspection?.phase {
                        let cancelled = try await owner.lifecycle.session.cancel(operationID: id)
                        try owner.operations[id]?.adopt(cancelled)
                    } else if !context.retired { try await prepareCredentialPresentation(owner, context: context) }
                }
                if let context = owner.devices[id], context.details == nil {
                    if context.retired, case .awaitingAction = owner.operations[id]?.inspection?.phase {
                        let cancelled = try await owner.lifecycle.session.cancel(operationID: id)
                        try owner.operations[id]?.adopt(cancelled)
                    } else if !context.retired { try await prepareDeviceConfirmation(owner, context: context) }
                }
                if let context = owner.devices[id], !context.retired { context.issue = nil }
            } catch {
                let failure = sanitize(error)
                owner.operations[id]?.issue = failure
                if let context = owner.devices[id] {
                    if context.retired { context.issue = context.issue ?? failure }
                    else { context.issue = failure }
                    context.ceremony.failure(context.issue ?? failure)
                }
            }
            owner.operations[id]?.isWorking = false
            owner.effects[id] = nil
            if isCurrent(owner) {
                publishOperations(owner)
                invalidateObservation()
                requestRefresh()
            }
        }
    }

    private func recover(_ id: UUID, reconciliation: Bool, cancellation: Bool) {
        guard let owner = current, let operation = owner.operations[id], owner.effects[id] == nil else { return }
        if !operation.wasSubmitted {
            if cancellation {
                owner.actions[id]?.admission.cancel()
                owner.operations[id]?.cancelledBeforeSubmission = true
                publishOperations(owner)
            }
            return
        }
        startEffect(owner, id: id) {
            let inspection: SettingsOperationInspection
            if reconciliation { inspection = try await owner.lifecycle.session.reconcile(operationID: id) }
            else if cancellation {
                owner.actions[id]?.admission.cancel()
                inspection = try await owner.lifecycle.session.cancel(operationID: id)
            } else { inspection = try await owner.lifecycle.session.inspect(operationID: id) }
            return .inspected(inspection)
        }
    }

    private func setOperationIssue(_ issue: NativeSettingsSessionError, owner: Owner, id: UUID) {
        owner.operations[id]?.issue = issue
        publishOperations(owner)
    }

    private func draftWasSaved(_ owner: Owner, snapshot: SettingsSnapshot) -> Bool {
        guard let draft else { return false }
        return owner.submittedDrafts.contains { id, submitted in
            guard submitted == draft, let operation = owner.operations[id],
                  case .patch(let patch) = operation.target, patch.expectedRevision < UInt64(Int64.max),
                  snapshot.revision == patch.expectedRevision + 1,
                  patch.changes.allSatisfy({ snapshot.entry($0.key)?.desired == $0.value }) else { return false }
            if operation.inspection?.write?.resolution == .applied { return true }
            switch operation.result?.state {
            case .applied(let revision), .pendingApplication(let revision), .restartRequired(let revision): return revision == snapshot.revision
            default: return false
            }
        }
    }

    private func publishOperations(_ owner: Owner) {
        if isCurrent(owner) { operations = owner.operations.values.sorted { $0.id.uuidString < $1.id.uuidString } }
        publishCredentials()
        publishDevices()
    }

    private func identityCount(_ owner: Owner) -> Int {
        owner.operations.count + owner.credentials.values.filter { $0.initial || $0.link != nil }.count
            + owner.devices.values.filter { $0.request.action == .pair }.count
            + owner.assistantOpens.count + owner.assistantSends.count
            + owner.assistantSends.values.reduce(0) { $0 + 8 - $1.materialized.count }
    }

    private func identityReserved(_ owner: Owner, id: UUID) -> Bool {
        owner.operations[id] != nil || owner.effects[id] != nil || owner.assistantOpens[id] != nil
            || owner.assistantSends.values.contains { $0.turnID == id || $0.requestID == id || $0.operationIDs.contains(id) }
            || owner.assistantChats.values.contains { $0.opened.chatID == id }
            || owner.credentials.values.contains { $0.link?.plan.ownerRequest.id.value == id }
            || owner.devices.values.contains { $0.approval?.review.operationID == id }
    }

    private func credential(_ handle: CredentialHandle) -> (Owner, CredentialContext)? {
        guard let owner = owners[handle.ownerID], let context = owner.credentials[handle.id], context.handle === handle else { return nil }
        return (owner, context)
    }

    private func captureCredential(_ owner: Owner, expiry: UInt64? = nil,
                                   deadline: ContinuousClock.Instant? = nil) throws -> CredentialCapture {
        guard isCurrent(owner), isForeground, let information = owner.information else { throw NativeSettingsSessionError.inactive }
        let expiresAt = min(expiry ?? information.expiresAt, information.expiresAt)
        let now = try NativeSettingsGate.wallTime()
        guard expiresAt > now else { throw NativeSettingsSessionError.expired }
        let started = ContinuousClock.now
        let captured = started.advanced(by: .milliseconds(Int64(min(expiresAt - now, 300_000))))
        return CredentialCapture(generation: credentialGeneration, started: started, expiresAt: expiresAt, deadline: min(deadline ?? captured, captured))
    }

    private func validateCredential(_ capture: CredentialCapture, owner: Owner) throws {
        guard isCurrent(owner), isForeground, NSApplication.shared.isActive, capture.generation == credentialGeneration else {
            throw NativeSettingsSessionError.inactive
        }
        guard try NativeSettingsGate.wallTime() < capture.expiresAt, ContinuousClock.now < capture.deadline else {
            throw NativeSettingsSessionError.expired
        }
    }

    private func prepareCredentialPresentation(_ owner: Owner, context: CredentialContext) async throws {
        guard let inspection = owner.operations[context.handle.id]?.inspection,
              case .awaitingAction(let reference) = inspection.phase else { return }
        let details = try await owner.lifecycle.session.details(operationID: context.handle.id, reference: .action(reference: reference))
        guard details.target == .native(request: context.request), details.operation.binding == inspection.binding,
              details.operation.origin == inspection.origin, details.operation.phase == .awaitingAction(reference: reference),
              case .nativeUI(let sessionID) = details.operation.origin,
              sessionID == details.operation.binding.clientSessionID else { throw NativeSettingsSessionError.invalidResponse }
        context.details = details
        do {
            try validateCredential(context.capture, owner: owner)
            guard !context.retired, !context.submitted else { throw NativeSettingsSessionError.inactive }
            let lifetime = reference.expiresAt - details.operation.binding.issuedAt
            let deadline = min(context.capture.deadline, context.capture.started.advanced(by: .milliseconds(Int64(min(lifetime, 300_000)))))
            let presentation = CredentialPresentation(handle: context.handle, request: context.request, reference: reference, deadline: deadline)
            try validateCredentialReference(presentation)
            context.presentation = presentation
            credentialPresentationIssue = nil
            credentialPresentation = presentation
        } catch {
            context.retired = true
            context.issue = credentialError(error)
            let cancelled = try await owner.lifecycle.session.cancel(operationID: context.handle.id)
            try owner.operations[context.handle.id]?.adopt(cancelled)
            throw error
        }
    }

    private func validateCredentialReference(_ presentation: CredentialPresentation) throws {
        guard try NativeSettingsGate.wallTime() < presentation.reference.expiresAt, ContinuousClock.now < presentation.deadline else {
            throw NativeSettingsSessionError.expired
        }
    }

    private func retireCredentialPresentations() {
        credentialGeneration = UUID()
        credentialPresentation?.field?.clear()
        credentialPresentation = nil
        for owner in owners.values {
            for context in owner.credentials.values where !context.retired {
                context.retired = true
                context.presentation?.field?.clear()
                context.presentation = nil
                if !context.submitted, owner.effects[context.handle.id] == nil, isCurrent(owner) {
                    recover(context.handle.id, reconciliation: false, cancellation: true)
                }
            }
        }
        publishCredentials()
    }

    private func publishCredentials() {
        credentialOperations = owners.values.flatMap { owner in
            owner.credentials.values.compactMap { context in
                guard let operation = owner.operations[context.handle.id] else { return nil }
                return NativeCredentialOperation(id: context.handle.id, handle: context.handle, request: context.request,
                    operation: operation, initial: context.initial, presentationRetired: context.retired,
                    linkRequestID: context.link?.plan.ownerRequest.id.value, linkState: context.link?.state ?? .notRequested,
                    issue: context.link?.issue ?? context.issue)
            }
        }.sorted { $0.id.uuidString < $1.id.uuidString }
    }

    private func credentialError(_ error: any Error) -> NativeCredentialIssue {
        if let error = error as? NativeSettingsSessionError { return .native(error) }
        if let error = error as? OwnerSettingsMetadataError { return .metadata(error) }
        return .unconfirmed
    }

    private func credentialLinksNeedRecovery(_ owner: Owner) -> Bool {
        owner.credentials.values.contains { context in
            guard let link = context.link else { return false }
            if case .working = link.state { return true }
            guard link.attempted else { return false }
            guard let receipt = link.receipt else { return true }
            return receipt.fileOutcome == .unsettled || link.issue != nil
        }
    }

    private func runCredentialLink(_ owner: Owner, context: CredentialContext, link: CredentialLink) {
        let id = link.plan.ownerRequest.id.value
        guard !isClosed, isCurrent(owner), owner.effects.count < 4, owner.effects[id] == nil, !link.attempted else { return }
        link.state = .working
        publishCredentials()
        owner.effects[id] = Task { [self, owner, context, link] in
            do {
                let completed = try await owner.lifecycle.session.inspect(operationID: context.handle.id)
                guard let details = context.details, completed.binding == details.operation.binding,
                      completed.origin == details.operation.origin, completed.write == nil, completed.application == nil,
                      case .finished(let result) = completed.phase, case .completed = result.state else {
                    throw OwnerSettingsMetadataError.invalidEnrollment
                }
                try owner.operations[context.handle.id]?.adopt(completed)
                let current = try await owner.lifecycle.session.snapshot(context.request.scope)
                guard current.revision == context.metadata.revision, !current.stale else { throw OwnerSettingsMetadataError.bindingConflict }
                try validateCredential(link.capture, owner: owner)
                link.attempted = true
                let response = try await client.request(link.plan.ownerRequest, connection: owner.connection)
                link.receipt = try link.plan.receipt(response)
                if let receipt = link.receipt { link.state = .observed(receipt) }
            } catch {
                link.issue = credentialError(error)
                link.state = link.attempted ? .unconfirmed : .notSubmitted
            }
            owner.effects[id] = nil
            publishOperations(owner)
            if isCurrent(owner) { invalidateObservation(); requestRefresh() }
        }
    }

    private func recoverCredentialLink(_ handle: CredentialHandle, reconcile: Bool) {
        guard !isClosed, let (owner, context) = credential(handle), let link = context.link,
              link.attempted, !owner.recoveryRetirementPending, owner.effects.count < 4 else { return }
        let id = link.plan.ownerRequest.id.value
        guard owner.effects[id] == nil else { return }
        if reconcile, link.receipt?.operation.write == nil || link.receipt?.operation.write?.resolution != nil { return }
        if owner.retirement != nil { owner.recoveryRetirementPending = true }
        link.state = .working
        publishCredentials()
        owner.effects[id] = Task { [self, owner, link] in
            do {
                let request = try reconcile ? link.plan.reconciliationRequest() : link.plan.inspectionRequest()
                let response = try await client.request(request, connection: owner.connection)
                let receipt = try reconcile ? link.plan.reconciled(response, previous: link.receipt) : link.plan.inspected(response, previous: link.receipt)
                try validateCredentialLink(receipt, previous: link.receipt)
                link.receipt = receipt
                link.issue = nil
                link.state = .observed(receipt)
            } catch { link.issue = credentialError(error); link.state = .unconfirmed }
            owner.effects[id] = nil
            if let native = owner.nativeReport { owner.report = credentialCleanup(owner, native: native); refreshCleanup() }
            publishCredentials()
            if isCurrent(owner) { invalidateObservation(); requestRefresh() }
        }
        if owner.retirement != nil, let effect = owner.effects[id] {
            let previous = owner.retirement
            let retirementID = UUID()
            owner.retirementID = retirementID
            owner.retirement = Task { [self, owner, effect, previous] in
                if let previous { _ = await previous.value }
                await effect.value
                let report = owner.nativeReport.map { credentialCleanup(owner, native: $0) }
                let result = Cleanup(id: owner.id, connectionKey: owner.connectionKey, report: report,
                                     unlinkedCredentialOperationIDs: unlinkedCredentialIDs(owner))
                owner.report = report
                if owner.retirementID == retirementID {
                    owner.recoveryRetirementPending = false
                    if owner.effects.isEmpty, !result.requiresRetention { owners.removeValue(forKey: owner.id) }
                }
                refreshCleanup()
                publishCredentials()
                publishDevices()
                return result
            }
        }
    }

    private func credentialCleanup(_ owner: Owner, native: NativeSettingsSession.DrainReport) -> NativeSettingsSession.DrainReport {
        var failures = native.failures
        var unresolved = Set(native.unresolvedOperationIDs)
        for context in owner.credentials.values {
            guard let link = context.link, link.attempted else { continue }
            if link.receipt?.fileOutcome == nil || link.receipt?.fileOutcome == .unsettled {
                unresolved.insert(link.plan.ownerRequest.id.value)
            }
            if link.issue != nil { failures.append(.operationUncertain) }
        }
        for context in owner.devices.values {
            if context.cleanupKind == .pairing, let lifecycle = native.pairingLifecycles[context.handle.id] {
                context.pairingLifecycle = lifecycle
                switch lifecycle {
                case .cancelled, .expired, .noLongerPending: context.cleanupIssue = nil
                case .retained, .cancellationPending, .unavailable: context.cleanupIssue = context.cleanupIssue ?? .operationUncertain
                }
            }
            if let cleanupIssue = context.cleanupIssue {
                if let original = context.issue { failures.append(original) }
                failures.append(cleanupIssue)
                unresolved.insert(context.handle.id)
            }
        }
        return NativeSettingsSession.DrainReport(failures: failures,
            unresolvedOperationIDs: unresolved.sorted { $0.uuidString < $1.uuidString },
            pairingLifecycles: native.pairingLifecycles,
            unresolvedAssistantOpenRequestIDs: native.unresolvedAssistantOpenRequestIDs)
    }

    private func unlinkedCredentialIDs(_ owner: Owner) -> [UUID] {
        owner.credentials.values.compactMap { context in
            guard context.initial, let operation = owner.operations[context.handle.id]?.inspection,
                  case .finished(let result) = operation.phase, case .completed = result.state else { return nil }
            if case .applied = context.link?.receipt?.fileOutcome { return nil }
            return context.handle.id
        }.sorted { $0.uuidString < $1.uuidString }
    }

    private func validateCredentialLink(_ receipt: OwnerSettingsReceipt, previous: OwnerSettingsReceipt?) throws {
        guard let previous, case .finished(let result) = previous.operation.phase else { return }
        if previous.operation.write == nil, receipt.operation.write != nil { throw OwnerSettingsMetadataError.unexpectedReceipt }
        if previous.operation.phase != receipt.operation.phase {
            guard result.state == .unknown, let write = previous.operation.write, write.resolution == nil,
                  case .finished = receipt.operation.phase, receipt.operation.write?.resolution != nil else {
                throw OwnerSettingsMetadataError.unexpectedReceipt
            }
        }
    }

    private func stopObserver() {
        current?.observer?.cancel()
    }

    private func requestRefresh() {
        guard isForeground, let owner = current, owner.information != nil else { return }
        if owner.work != nil { owner.refreshRequested = true }
        else { refresh() }
    }

    private func updateObserver() {
        guard isForeground else { stopObserver(); return }
        guard let owner = current, owner.information != nil, owner.observer == nil,
              issue == nil, !observationIsStale else { return }
        let observerID = UUID()
        owner.observerID = observerID
        owner.observer = Task { [self, owner] in
            do {
                while !Task.isCancelled && isCurrent(owner) && isForeground && issue == nil {
                    try await Task.sleep(for: .seconds(2))
                    try Task.checkCancellation()
                    let generation = observationGeneration
                    let page = try await owner.lifecycle.session.changes(after: owner.cursor, limit: 32)
                    try Task.checkCancellation()
                    guard isCurrent(owner), isForeground, owner.observerID == observerID else { break }
                    owner.cursor = page.nextSequence ?? owner.cursor
                    for id in Set(page.changes.compactMap(\.requestID)) where owner.operations[id] != nil && owner.effects[id] == nil {
                        try Task.checkCancellation()
                        let inspection = try await owner.lifecycle.session.inspect(operationID: id)
                        try owner.operations[id]?.adopt(inspection)
                    }
                    publishOperations(owner)
                    if !page.changes.isEmpty {
                        invalidateObservation()
                        requestRefresh()
                    } else if generation != observationGeneration { break }
                }
            } catch is CancellationError {
            } catch {
                if isCurrent(owner) { issue = sanitize(error); invalidateObservation(); retireCredentialPresentations(); retireDeviceCeremonies() }
            }
            if owner.observerID == observerID {
                owner.observer = nil
                owner.observerID = nil
            }
            if Task.isCancelled && isCurrent(owner) && isForeground && !observationIsStale { updateObserver() }
        }
    }
}

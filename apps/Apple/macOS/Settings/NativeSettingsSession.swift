import AppKit
import Foundation

actor NativeSettingsSession {
    struct Information: Equatable, Sendable {
        let hostID: UUID
        let expiresAt: UInt64
        let connectionKey: UUID
    }

    final class AssistantHandle: @unchecked Sendable, CustomStringConvertible, CustomDebugStringConvertible {
        let opened: NativeSettingsAssistantWire.Opened
        let openRequestID: UUID
        private let issuer: UUID
        private let gate: NativeSettingsGate
        private let parentID: UUID
        private let hostID: UUID
        private let deadline: ContinuousClock.Instant

        var description: String { "NativeSettingsAssistantHandle([private])" }
        var debugDescription: String { description }

        fileprivate init(opened: NativeSettingsAssistantWire.Opened, original: NativeSettingsAssistantOpenAdmission,
                         issuer: UUID, gate: NativeSettingsGate, parentID: UUID, hostID: UUID, deadline: ContinuousClock.Instant) {
            self.opened = opened
            openRequestID = original.requestID
            self.issuer = issuer
            self.gate = gate
            self.parentID = parentID
            self.hostID = hostID
            self.deadline = deadline
        }

        func validateOwner(_ gate: NativeSettingsGate) throws {
            guard self.gate === gate else { throw NativeSettingsSessionError.invalidRequest }
        }

        @MainActor
        fileprivate func validatePresentation(_ gate: NativeSettingsGate) throws {
            try validateOwner(gate)
            guard NSApplication.shared.isActive else { throw NativeSettingsSessionError.inactive }
            try gate.requireCurrentActivity()
            try validateLifetime()
        }

        func validateLifetime() throws {
            guard try NativeSettingsGate.wallTime() < opened.expiresAt, ContinuousClock.now < deadline else {
                throw NativeSettingsSessionError.expired
            }
        }

        fileprivate func validate(issuer: UUID, gate: NativeSettingsGate, parentID: UUID, hostID: UUID) throws {
            try validateOwner(gate)
            guard self.issuer == issuer, self.parentID == parentID, self.hostID == hostID else {
                throw NativeSettingsSessionError.invalidRequest
            }
        }
    }

    final class AssistantOperationHandle: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
        let chat: AssistantHandle
        let turnID: UUID
        let operationID: UUID

        var description: String { "NativeSettingsAssistantOperationHandle([private])" }
        var debugDescription: String { description }

        fileprivate init(chat: AssistantHandle, turnID: UUID, operationID: UUID) {
            self.chat = chat
            self.turnID = turnID
            self.operationID = operationID
        }
    }

    struct AssistantObservation: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
        let status: NativeSettingsAssistantWire.Status
        let operations: [AssistantOperationHandle]

        var description: String { "NativeSettingsAssistantObservation([private])" }
        var debugDescription: String { description }

        fileprivate init(status: NativeSettingsAssistantWire.Status, operations: [AssistantOperationHandle]) {
            self.status = status
            self.operations = operations
        }
    }

    struct AssistantDetailsReceipt: Sendable {
        let original: AssistantOperationHandle
        let details: Details

        fileprivate init(original: AssistantOperationHandle, details: Details) {
            self.original = original
            self.details = details
        }
    }

    enum AssistantOpenOutcome: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
        case opened(AssistantHandle)
        case state(NativeSettingsAssistantWire.OpenState)
        case unavailable

        var description: String { "NativeSettingsAssistantOpenOutcome([private])" }
        var debugDescription: String { description }
    }

    enum AssistantSendOutcome: Sendable {
        case accepted(chatID: UUID, turnID: UUID)
        case unavailable
    }

    private struct AssistantOpenRecord {
        let admission: NativeSettingsAssistantOpenAdmission
        var task: Task<Result<AssistantOpenOutcome, NativeSettingsSessionError>, Never>?
        var handle: AssistantHandle?
        var knownChatID: UUID?
        var state: NativeSettingsAssistantWire.OpenState?
        var status: NativeSettingsAssistantWire.Status?
        var retiring = false
    }

    private struct AssistantSendRecord {
        let admission: NativeSettingsAssistantSendAdmission
        var task: Task<Result<AssistantSendOutcome, NativeSettingsSessionError>, Never>?
        var operations: [UUID: AssistantOperationHandle] = [:]
        var materialized: Set<UUID> = []
        var references: [UUID: NativeActionReference] = [:]
        var modelAttempts: UInt32 = 0
        var chargedTokens: UInt64 = 0
        var reservedOutputTokens: UInt64 = 0
        var pending = true
    }

    private enum AssistantReadKey: Hashable {
        case open(UUID)
        case closeOpen(UUID)
        case poll(UUID)
        case close(UUID)
        case inspect(UUID, UUID, UUID)
        case details(UUID, UUID, UUID, UUID, UInt64, Bool)
    }

    private struct AssistantReadFlight {
        let task: Task<Result<NativeSettingsAssistantWire.Reply, NativeSettingsSessionError>, Never>
    }

    struct Details: Equatable, Sendable {
        let operation: SettingsOperationInspection
        let target: SettingsActionTarget
    }

    struct PairingStatus: Sendable {
        let operation: SettingsOperationInspection
        let lifecycle: NativeSettingsWire.PairingLifecycle
        let invitationAvailable: Bool
    }

    struct PairingApprovalStatus: Sendable {
        let operation: SettingsOperationInspection
        let approval: NativeSettingsWire.PairingApprovalMetadata
        let hostPin: NativeSettingsWire.PairingPin
        let acceptanceAvailable: Bool
    }

    struct PairingAcceptancePresentation: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
        let operationID: UUID
        let pairingID: UUID
        let expiresAt: UInt64
        private let value: Data
        private let admission: NativePairingApprovalAdmission
        private let gate: NativeSettingsGate
        private let deadline: ContinuousClock.Instant

        var description: String { "NativePairingAcceptancePresentation([private])" }
        var debugDescription: String { description }

        fileprivate init(operationID: UUID, pairingID: UUID, expiresAt: UInt64, value: Data,
                         admission: NativePairingApprovalAdmission, gate: NativeSettingsGate, deadline: ContinuousClock.Instant) {
            self.operationID = operationID
            self.pairingID = pairingID
            self.expiresAt = expiresAt
            self.value = value
            self.admission = admission
            self.gate = gate
            self.deadline = deadline
        }

        @MainActor
        fileprivate func validateDelivery() throws {
            try admission.validatePresentation(gate)
            guard try NativeSettingsGate.wallTime() < expiresAt, ContinuousClock.now < deadline else {
                throw NativeSettingsSessionError.expired
            }
        }

        @MainActor
        func transferData() throws -> Data {
            try validateDelivery()
            return value
        }
    }

    enum Outcome: Sendable {
        case prepared(SettingsOperationResult)
        case inspected(SettingsOperationInspection)
        case pairing(PairingStatus)
    }

    struct Presentation: Sendable {
        let invocation: NativeActionInvocation
        let action: NativeSettingsAction
    }

    struct PairingPresentation: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
        let operationID: UUID
        let pairingID: UUID
        let expiresAt: UInt64
        let fingerprint: String
        let invitation: String

        var description: String { "NativeSettingsPairingPresentation([private])" }
        var debugDescription: String { description }
    }

    struct DrainReport: Sendable {
        let failures: [NativeSettingsSessionError]
        let unresolvedOperationIDs: [UUID]
        let pairingLifecycles: [UUID: NativeSettingsWire.PairingLifecycle]
        let unresolvedAssistantOpenRequestIDs: [UUID]

        init(failures: [NativeSettingsSessionError], unresolvedOperationIDs: [UUID],
             pairingLifecycles: [UUID: NativeSettingsWire.PairingLifecycle] = [:],
             unresolvedAssistantOpenRequestIDs: [UUID] = []) {
            self.failures = failures
            self.unresolvedOperationIDs = unresolvedOperationIDs
            self.pairingLifecycles = pairingLifecycles
            self.unresolvedAssistantOpenRequestIDs = unresolvedAssistantOpenRequestIDs
        }
    }

    private struct Record {
        let target: SettingsActionTarget
        let initialIntent: NativeSettingsIntent?
        let initialRPC = UUID()
        let executionRPC = UUID()
        let finalizationRPC = UUID()
        var initialTask: Task<Result<Outcome, NativeSettingsSessionError>, Never>?
        var executionTask: Task<Result<Outcome, NativeSettingsSessionError>, Never>?
        var executionIntent: NativeSettingsIntent?
        var invitationTask: Task<Result<PairingStatus, NativeSettingsSessionError>, Never>?
        var prepared: SettingsOperationResult?
        var inspection: SettingsOperationInspection?
        var pairing: PairingStatus?
        var observation: NativeActionObservation?
        var presentationUncertain = false
        var invitationRetired = false
        var invitationIdentity: InvitationIdentity?
    }

    private struct InvitationIdentity: Sendable {
        let pairingID: UUID
        let expiresAt: UInt64
        let fingerprint: String
        let deadline: ContinuousClock.Instant
    }

    private struct RetainedInvitation: Sendable {
        let value: NativeSettingsWire.Invitation
        let deadline: ContinuousClock.Instant
    }

    private struct PairingApprovalRecord: CustomStringConvertible, CustomDebugStringConvertible {
        let admission: NativePairingApprovalAdmission
        let sessionID: UUID
        let deadline: ContinuousClock.Instant
        var invitation: PairingInvitation?
        var task: Task<Result<PairingApprovalStatus, NativeSettingsSessionError>, Never>?
        var presentation: Task<Result<PairingApprovalStatus, NativeSettingsSessionError>, Never>?
        var metadata: NativeSettingsWire.PairingApprovalMetadata?
        var status: PairingApprovalStatus?

        var description: String { "NativePairingApprovalRecord([private])" }
        var debugDescription: String { description }
    }

    private enum Presented: Sendable {
        case notStarted
        case observed(NativeActionObservation)
        case uncertain
    }

    private let client: OwnerIPCClient
    private let connection: OwnerConnection
    private let connectionKey: UUID
    private let gate: NativeSettingsGate
    private let closeRequestID = UUID()
    private var openingAdmission: NativeSettingsAdmission?
    private var openingTask: Task<Result<Information, NativeSettingsSessionError>, Never>?
    private var closingTask: Task<DrainReport, Never>?
    private var proof: NativeSettingsWire.Proof?
    private var information: Information?
    private var deadline: ContinuousClock.Instant?
    private var closing = false
    private var finalizingDrain = false
    private var closeAcknowledged = false
    private var records: [UUID: Record] = [:]
    private var calls: [UUID: Task<Result<NativeSettingsWire.Response, NativeSettingsSessionError>, Never>] = [:]
    private var invitations: [UUID: RetainedInvitation] = [:]
    private var pairingApprovals: [UUID: PairingApprovalRecord] = [:]
    private let assistantIssuer = UUID()
    private var assistantOpens: [UUID: AssistantOpenRecord] = [:]
    private var assistantSends: [UUID: AssistantSendRecord] = [:]
    private var assistantReads: [AssistantReadKey: AssistantReadFlight] = [:]
    private var assistantDrainFailures: [NativeSettingsSessionError] = []
    private var assistantDrainUnresolved: Set<UUID> = []
    private var retainedOperationCount: Int {
        records.count + pairingApprovals.count + assistantOpens.count + assistantSends.count
            + assistantSends.values.reduce(0) { $0 + 8 - $1.materialized.count }
    }

    init(client: OwnerIPCClient, connection: OwnerConnection, connectionKey: UUID, gate: NativeSettingsGate) {
        self.client = client
        self.connection = connection
        self.connectionKey = connectionKey
        self.gate = gate
    }

    func open(_ admission: NativeSettingsAdmission) async throws -> Information {
        guard admission.intent == .open else { throw NativeSettingsSessionError.invalidRequest }
        if let openingTask {
            guard openingAdmission === admission else { throw NativeSettingsSessionError.requestReused }
            let value = try await openingTask.value.get()
            try requireLive()
            return value
        }
        if Task.isCancelled { admission.cancel() }
        try admission.validate(for: gate)
        guard !closing else { throw NativeSettingsSessionError.closed }
        openingAdmission = admission
        let started = ContinuousClock.now
        let task = Task { await self.finishOpen(admission, started: started) }
        openingTask = task
        let value = try await task.value.get()
        try requireLive()
        return value
    }

    func openAssistant(_ admission: NativeSettingsAssistantOpenAdmission) async throws -> AssistantOpenOutcome {
        try admission.validateOwner(gate)
        if let original = assistantOpens[admission.requestID] {
            guard original.admission === admission else { throw NativeSettingsSessionError.requestReused }
            if original.retiring, let state = original.state { return .state(state) }
            if case .ready = original.state, let handle = original.handle { return .opened(handle) }
            if let task = original.task { return try await task.value.get() }
        }
        guard !closing else { throw NativeSettingsSessionError.closed }
        try requireLive()
        try retainAssistantOpen(admission)
        guard assistantOpens[admission.requestID]?.retiring == false else { throw NativeSettingsSessionError.closed }
        let task = Task { await self.finishAssistantOpen(admission) }
        assistantOpens[admission.requestID]?.task = task
        return try await task.value.get()
    }

    func assistantOpenStatus(_ admission: NativeSettingsAssistantOpenAdmission) async throws -> NativeSettingsAssistantWire.OpenState {
        try requireAssistantRead()
        try retainAssistantOpen(admission)
        guard case .openState(_, let state) = try await assistantRead(.open(admission.requestID),
            operation: .agentOpenStatus(openRequestID: admission.requestID)) else { throw NativeSettingsSessionError.invalidResponse }
        try acceptAssistantOpenState(state, original: admission)
        return state
    }

    func closeAssistantOpen(_ admission: NativeSettingsAssistantOpenAdmission) async throws -> NativeSettingsAssistantWire.OpenState {
        try requireAssistantRead()
        try retainAssistantOpen(admission)
        return try await closeAssistantOriginal(admission)
    }

    func sendAssistant(_ admission: NativeSettingsAssistantSendAdmission) async throws -> AssistantSendOutcome {
        try admission.validateOwner(gate)
        if let original = assistantSends[admission.turnID] {
            guard original.admission === admission, let task = original.task else { throw NativeSettingsSessionError.requestReused }
            return try await task.value.get()
        }
        try requireLive()
        try requireAssistant(admission.chat, live: true)
        guard retainedOperationCount + 9 <= 64 else { throw NativeSettingsSessionError.capacity }
        guard !assistantSends.values.contains(where: { $0.admission.chat === admission.chat && $0.pending }) else {
            throw NativeSettingsSessionError.operationUncertain
        }
        guard !assistantIdentityExists(admission.turnID), !assistantIdentityExists(admission.requestID),
              records[admission.turnID] == nil, pairingApprovals[admission.turnID] == nil,
              records[admission.requestID] == nil, pairingApprovals[admission.requestID] == nil else {
            throw NativeSettingsSessionError.requestReused
        }
        assistantSends[admission.turnID] = AssistantSendRecord(admission: admission)
        let task = Task { await self.finishAssistantSend(admission) }
        assistantSends[admission.turnID]?.task = task
        return try await task.value.get()
    }

    func pollAssistant(_ chat: AssistantHandle) async throws -> AssistantObservation {
        try requireAssistantRead()
        try requireAssistant(chat, live: false)
        if let state = assistantOpens[chat.openRequestID]?.state, assistantOriginalClosed(state) { throw NativeSettingsSessionError.closed }
        guard case .status(let status) = try await assistantRead(.poll(chat.opened.chatID),
            operation: .agentPoll(chatID: chat.opened.chatID)) else { throw NativeSettingsSessionError.invalidResponse }
        return try acceptAssistantStatus(status, chat: chat)
    }

    func closeAssistant(_ chat: AssistantHandle) async throws -> AssistantObservation {
        try requireAssistantRead()
        try requireAssistant(chat, live: false)
        if let state = assistantOpens[chat.openRequestID]?.state, assistantOriginalClosed(state) { throw NativeSettingsSessionError.closed }
        assistantOpens[chat.openRequestID]?.retiring = true
        for original in assistantSends.values where original.admission.chat === chat { original.admission.cancel() }
        for original in assistantSends.values where original.admission.chat === chat {
            if let task = original.task { _ = await task.value }
        }
        guard case .status(let status) = try await assistantRead(.close(chat.opened.chatID),
            operation: .agentClose(chatID: chat.opened.chatID)) else { throw NativeSettingsSessionError.invalidResponse }
        return try acceptAssistantStatus(status, chat: chat)
    }

    func inspectAssistantOperation(_ original: AssistantOperationHandle) async throws -> NativeSettingsAssistantWire.OperationLookup {
        try requireAssistantRead()
        try requireAssistantOperation(original, live: false)
        guard case .operation(_, _, _, let lookup) = try await assistantRead(
            .inspect(original.chat.opened.chatID, original.turnID, original.operationID),
            operation: .agentInspectOperation(chatID: original.chat.opened.chatID, turnID: original.turnID,
                                             operationID: original.operationID)) else { throw NativeSettingsSessionError.invalidResponse }
        switch lookup {
        case .notRecorded: assistantSends[original.turnID]?.references.removeValue(forKey: original.operationID)
        case .inspected(let operation):
            switch operation.phase {
            case .awaitingConfirmation(let reference):
                assistantSends[original.turnID]?.references[original.operationID] = .confirmation(reference: reference)
            case .awaitingAction(let reference):
                assistantSends[original.turnID]?.references[original.operationID] = .action(reference: reference)
            default: assistantSends[original.turnID]?.references.removeValue(forKey: original.operationID)
            }
            if records[original.operationID] != nil { try accept(operation, id: original.operationID) }
        }
        return lookup
    }

    func assistantActionDetails(_ original: AssistantOperationHandle,
                                reference: NativeActionReference) async throws -> AssistantDetailsReceipt {
        try requireAssistantOperation(original, live: true)
        guard assistantSends[original.turnID]?.references[original.operationID] == reference else {
            throw NativeSettingsSessionError.invalidRequest
        }
        try reference.validate()
        guard try NativeSettingsGate.wallTime() < reference.expiresAt else { throw NativeSettingsSessionError.expired }
        let confirmation: Bool
        switch reference {
        case .confirmation: confirmation = true
        case .action: confirmation = false
        }
        guard case .actionDetails(_, _, _, let operation, let target) = try await assistantRead(
            .details(original.chat.opened.chatID, original.turnID, original.operationID, reference.id, reference.expiresAt, confirmation),
            operation: .agentActionDetails(chatID: original.chat.opened.chatID, turnID: original.turnID,
                                          operationID: original.operationID, reference: reference), liveChat: original.chat) else {
            throw NativeSettingsSessionError.invalidResponse
        }
        try requireAssistantOperation(original, live: true)
        guard assistantSends[original.turnID]?.references[original.operationID] == reference else {
            throw NativeSettingsSessionError.invalidResponse
        }
        let id = original.operationID
        if let existing = records[id] {
            guard existing.target == target, assistantSends[original.turnID]?.materialized.contains(id) == true else {
                throw NativeSettingsSessionError.requestReused
            }
        } else {
            guard (assistantSends[original.turnID]?.materialized.count ?? 8) < 8,
                  pairingApprovals[id] == nil else { throw NativeSettingsSessionError.capacity }
            records[id] = Record(target: target, initialIntent: nil)
            assistantSends[original.turnID]?.materialized.insert(id)
        }
        try accept(operation, id: id)
        return AssistantDetailsReceipt(original: original, details: Details(operation: operation, target: target))
    }

    private func retainAssistantOpen(_ admission: NativeSettingsAssistantOpenAdmission) throws {
        try admission.validateOwner(gate)
        if let existing = assistantOpens[admission.requestID] {
            guard existing.admission === admission else { throw NativeSettingsSessionError.requestReused }
            return
        }
        guard retainedOperationCount < 64 else { throw NativeSettingsSessionError.capacity }
        guard records[admission.requestID] == nil, pairingApprovals[admission.requestID] == nil,
              !assistantIdentityExists(admission.requestID) else { throw NativeSettingsSessionError.requestReused }
        assistantOpens[admission.requestID] = AssistantOpenRecord(admission: admission)
    }

    private func assistantIdentityExists(_ id: UUID) -> Bool {
        assistantOpens[id] != nil || assistantOpens.values.contains { $0.knownChatID == id }
            || assistantSends.values.contains { $0.admission.turnID == id || $0.admission.requestID == id || $0.operations[id] != nil }
    }

    private func requireAssistantRead() throws {
        guard !closing, !finalizingDrain, !closeAcknowledged, proof != nil else { throw NativeSettingsSessionError.closed }
    }

    private func requireAssistant(_ chat: AssistantHandle, live: Bool) throws {
        guard let proof, let information, !closeAcknowledged,
              assistantOpens[chat.openRequestID]?.handle === chat else { throw NativeSettingsSessionError.closed }
        try chat.validate(issuer: assistantIssuer, gate: gate, parentID: proof.sessionID, hostID: information.hostID)
        if live {
            try requireLive()
            try chat.validateLifetime()
            guard assistantOpens[chat.openRequestID]?.retiring == false else { throw NativeSettingsSessionError.closed }
        }
    }

    private func requireAssistantOperation(_ original: AssistantOperationHandle, live: Bool) throws {
        try requireAssistant(original.chat, live: live)
        guard let turn = assistantSends[original.turnID], turn.admission.chat === original.chat,
              turn.operations[original.operationID] === original else { throw NativeSettingsSessionError.invalidRequest }
    }

    private func assistantRead(_ key: AssistantReadKey, operation: NativeSettingsWire.Operation, liveChat: AssistantHandle? = nil) async throws -> NativeSettingsAssistantWire.Reply {
        if let flight = assistantReads[key] { return try await flight.task.value.get() }
        let task = Task<Result<NativeSettingsAssistantWire.Reply, NativeSettingsSessionError>, Never> {
            do { return .success(try await self.assistantRPC(operation, liveChat: liveChat)) }
            catch { return .failure(self.sanitize(error)) }
        }
        assistantReads[key] = AssistantReadFlight(task: task)
        let result = await task.value
        assistantReads.removeValue(forKey: key)
        return try result.get()
    }

    private func assistantRPC(_ operation: NativeSettingsWire.Operation, requestID: UUID = UUID(),
                              openAdmission: NativeSettingsAssistantOpenAdmission? = nil,
                              sendAdmission: NativeSettingsAssistantSendAdmission? = nil,
                              liveChat: AssistantHandle? = nil) async throws -> NativeSettingsAssistantWire.Reply {
        guard case .assistant(let reply) = try await rpc(operation, requestID: requestID, assistantOpen: openAdmission,
                                                         assistantSend: sendAdmission, assistantChat: liveChat) else {
            throw NativeSettingsSessionError.invalidResponse
        }
        return reply
    }

    private func finishAssistantOpen(_ admission: NativeSettingsAssistantOpenAdmission) async -> Result<AssistantOpenOutcome, NativeSettingsSessionError> {
        do {
            try await admission.validatePresentation(gate)
            try requireLive()
            guard assistantOpens[admission.requestID]?.retiring == false else { throw NativeSettingsSessionError.closed }
            switch try await assistantRPC(.agentOpen, requestID: admission.requestID, openAdmission: admission) {
            case .opened(let opened):
                try acceptAssistantOpenState(.ready(opened), original: admission)
                guard let handle = assistantOpens[admission.requestID]?.handle else { throw NativeSettingsSessionError.invalidResponse }
                return .success(.opened(handle))
            case .openState(_, let state):
                try acceptAssistantOpenState(state, original: admission)
                if case .ready = state, let handle = assistantOpens[admission.requestID]?.handle { return .success(.opened(handle)) }
                return .success(.state(state))
            case .unavailable:
                try acceptAssistantOpenState(.unavailable, original: admission)
                return .success(.unavailable)
            default: throw NativeSettingsSessionError.invalidResponse
            }
        } catch { return .failure(sanitize(error)) }
    }

    private func finishAssistantSend(_ admission: NativeSettingsAssistantSendAdmission) async -> Result<AssistantSendOutcome, NativeSettingsSessionError> {
        defer { admission.cancel() }
        do {
            try await admission.validatePresentation(gate)
            try requireAssistant(admission.chat, live: true)
            let text = try admission.takeText(gate)
            let operation = NativeSettingsWire.Operation.agentSend(chatID: admission.chat.opened.chatID,
                turnID: admission.turnID, providerBindingSHA256: admission.chat.opened.providerBindingSHA256, text: text)
            switch try await assistantRPC(operation, requestID: admission.requestID, sendAdmission: admission) {
            case .accepted(let chatID, let turnID): return .success(.accepted(chatID: chatID, turnID: turnID))
            case .unavailable:
                assistantSends[admission.turnID]?.pending = false
                assistantOpens[admission.chat.openRequestID]?.retiring = true
                return .success(.unavailable)
            default: throw NativeSettingsSessionError.invalidResponse
            }
        } catch { return .failure(sanitize(error)) }
    }

    private func acceptAssistantOpenState(_ state: NativeSettingsAssistantWire.OpenState,
                                          original: NativeSettingsAssistantOpenAdmission) throws {
        guard let existing = assistantOpens[original.requestID], existing.admission === original else {
            throw NativeSettingsSessionError.invalidRequest
        }
        if let previous = existing.state, assistantOriginalClosed(previous) {
            switch (previous, state) {
            case (.unavailable, .unavailable): break
            case (.closed(let old), .closed(let next)) where old == next: break
            default: throw NativeSettingsSessionError.invalidResponse
            }
        }
        switch state {
        case .ready(let opened):
            if let known = existing.knownChatID, known != opened.chatID { throw NativeSettingsSessionError.invalidResponse }
            assistantOpens[original.requestID]?.knownChatID = opened.chatID
            if let handle = existing.handle {
                let old = handle.opened
                guard old.chatID == opened.chatID, old.provider == opened.provider, old.model == opened.model,
                      old.recipient == opened.recipient, old.providerBindingSHA256 == opened.providerBindingSHA256,
                      old.tools == opened.tools, old.expiresAt == opened.expiresAt else { throw NativeSettingsSessionError.invalidResponse }
            } else {
                guard let proof, let information, let deadline else { throw NativeSettingsSessionError.closed }
                let now = try NativeSettingsGate.wallTime()
                let remaining = min(opened.expiresAt > now ? opened.expiresAt - now : 0, 300_000)
                let childDeadline = min(deadline, min(original.capture.deadline,
                    ContinuousClock.now.advanced(by: .milliseconds(Int64(remaining)))))
                assistantOpens[original.requestID]?.handle = AssistantHandle(opened: opened, original: original,
                    issuer: assistantIssuer, gate: gate, parentID: proof.sessionID, hostID: information.hostID, deadline: childDeadline)
            }
        case .closing(let chatID):
            if let known = existing.knownChatID, known != chatID { throw NativeSettingsSessionError.invalidResponse }
            assistantOpens[original.requestID]?.knownChatID = chatID
        case .closed(let chatID):
            if let known = existing.knownChatID, known != chatID { throw NativeSettingsSessionError.invalidResponse }
            if let chatID { assistantOpens[original.requestID]?.knownChatID = chatID }
            assistantOpens[original.requestID]?.retiring = true
            assistantOpens[original.requestID]?.status = nil
        case .unavailable:
            assistantOpens[original.requestID]?.retiring = true
            assistantOpens[original.requestID]?.status = nil
        case .unknown(let chatID):
            if let chatID {
                if let known = existing.knownChatID, known != chatID { throw NativeSettingsSessionError.invalidResponse }
                assistantOpens[original.requestID]?.knownChatID = chatID
            }
        case .notAccepted: break
        }
        assistantOpens[original.requestID]?.state = state
    }

    private func assistantOriginalClosed(_ state: NativeSettingsAssistantWire.OpenState) -> Bool {
        switch state {
        case .closed, .unavailable: true
        default: false
        }
    }

    private func closeAssistantOriginal(_ admission: NativeSettingsAssistantOpenAdmission) async throws -> NativeSettingsAssistantWire.OpenState {
        assistantOpens[admission.requestID]?.retiring = true
        if let task = assistantOpens[admission.requestID]?.task { _ = await task.value }
        let sends = assistantSends.values.filter { $0.admission.chat.openRequestID == admission.requestID }
        for original in sends { original.admission.cancel() }
        for original in sends { if let task = original.task { _ = await task.value } }
        guard case .openState(_, let state) = try await assistantRead(.closeOpen(admission.requestID),
            operation: .agentCloseOpen(openRequestID: admission.requestID)) else { throw NativeSettingsSessionError.invalidResponse }
        try acceptAssistantOpenState(state, original: admission)
        return state
    }

    private func acceptAssistantStatus(_ status: NativeSettingsAssistantWire.Status, chat: AssistantHandle) throws -> AssistantObservation {
        try requireAssistant(chat, live: false)
        guard status.chatID == chat.opened.chatID else { throw NativeSettingsSessionError.invalidResponse }
        let previous = assistantOpens[chat.openRequestID]?.status
        if let previous {
            guard status.textRevision >= previous.textRevision else { throw NativeSettingsSessionError.invalidResponse }
            if status.textRevision == previous.textRevision {
                guard status.turnID == previous.turnID, status.assistantText == previous.assistantText,
                      status.provisional?.attemptID == previous.provisional?.attemptID,
                      status.provisional?.state.rawValue == previous.provisional?.state.rawValue,
                      status.provisional?.text == previous.provisional?.text else { throw NativeSettingsSessionError.invalidResponse }
            }
        }
        var handles: [AssistantOperationHandle] = []
        if let turnID = status.turnID {
            guard let turn = assistantSends[turnID], turn.admission.chat === chat else { throw NativeSettingsSessionError.invalidResponse }
            guard status.modelAttempts >= turn.modelAttempts, status.chargedTokens >= turn.chargedTokens,
                  status.reservedOutputTokens >= turn.reservedOutputTokens else { throw NativeSettingsSessionError.invalidResponse }
            let members = Set(turn.operations.keys).union(status.operationIDs)
            guard members.count <= 8 else { throw NativeSettingsSessionError.capacity }
            for id in status.operationIDs {
                if let original = turn.operations[id] { handles.append(original); continue }
                guard records[id] == nil, pairingApprovals[id] == nil,
                      !assistantIdentityExists(id) else { throw NativeSettingsSessionError.requestReused }
                let original = AssistantOperationHandle(chat: chat, turnID: turnID, operationID: id)
                assistantSends[turnID]?.operations[id] = original
                handles.append(original)
            }
            switch status.phase {
            case .completed: assistantSends[turnID]?.pending = false
            case .closed, .unavailable, .stoppedUncertain, .stoppedUnusable:
                assistantSends[turnID]?.pending = false
                assistantOpens[chat.openRequestID]?.retiring = true
            case .ready, .running, .closing, .unknown: break
            }
        } else {
            guard status.operationIDs.isEmpty, status.assistantText.isEmpty else { throw NativeSettingsSessionError.invalidResponse }
        }
        if let state = assistantOpens[chat.openRequestID]?.state, assistantOriginalClosed(state) { throw NativeSettingsSessionError.closed }
        if let turnID = status.turnID {
            assistantSends[turnID]?.modelAttempts = status.modelAttempts
            assistantSends[turnID]?.chargedTokens = status.chargedTokens
            assistantSends[turnID]?.reservedOutputTokens = status.reservedOutputTokens
        }
        assistantOpens[chat.openRequestID]?.status = status
        return AssistantObservation(status: status, operations: handles)
    }

    func describe() async throws -> SettingRegistry {
        try requireLive()
        guard case .described(let registry) = try await rpc(.describe) else { throw NativeSettingsSessionError.invalidResponse }
        return registry
    }

    func snapshot(_ scope: SettingsScope) async throws -> SettingsSnapshot {
        try requireLive()
        guard case .snapshot(let snapshot) = try await rpc(.get(scope)) else { throw NativeSettingsSessionError.invalidResponse }
        return snapshot
    }

    func changes(after: UInt64?, limit: Int = 32) async throws -> SettingsChangePage {
        try requireLive()
        guard case .changes(let page) = try await rpc(.changes(after: after, limit: limit)) else {
            throw NativeSettingsSessionError.invalidResponse
        }
        return page
    }

    func devices(after: UUID?, limit: Int = 25) async throws -> SettingsDevicePage {
        try requireLive()
        guard let information else { throw NativeSettingsSessionError.closed }
        guard case .devices(let page) = try await rpc(.devices(scope: .host(hostID: information.hostID), after: after, limit: limit)) else {
            throw NativeSettingsSessionError.invalidResponse
        }
        return page
    }

    func details(operationID: UUID, reference: NativeActionReference) async throws -> Details {
        try requireLive()
        guard pairingApprovals[operationID] == nil else { throw NativeSettingsSessionError.requestReused }
        guard records[operationID] != nil || retainedOperationCount < 64 else { throw NativeSettingsSessionError.capacity }
        guard case .details(let operation, let target) = try await rpc(.details(operationID, reference)) else {
            throw NativeSettingsSessionError.invalidResponse
        }
        if let original = records[operationID] {
            guard original.target == target else { throw NativeSettingsSessionError.requestReused }
        } else {
            guard retainedOperationCount < 64 else { throw NativeSettingsSessionError.capacity }
            guard !assistantIdentityExists(operationID) else { throw NativeSettingsSessionError.requestReused }
            records[operationID] = Record(target: target, initialIntent: nil)
        }
        try accept(operation, id: operationID)
        return Details(operation: operation, target: target)
    }

    func perform(_ admission: NativeSettingsAdmission) async throws -> Outcome {
        try admission.validateOwner(gate)
        guard let id = admission.intent.operationID else { throw NativeSettingsSessionError.invalidRequest }
        if Task.isCancelled {
            switch admission.intent {
            case .update, .requestAction, .pairDevice, .revokeDevice:
                if records[id]?.initialTask == nil { admission.cancel() }
            default:
                if records[id]?.executionTask == nil { admission.cancel() }
            }
        }
        switch admission.intent {
        case .update(let request):
            return try await initial(admission, target: .patch(request: request), operation: .update(request))
        case .requestAction(let request):
            return try await initial(admission, target: .native(request: request), operation: .requestAction(request))
        case .pairDevice(let request):
            return try await initial(admission, target: .device(request: request), operation: .pairDevice(request))
        case .revokeDevice(let request):
            return try await initial(admission, target: .device(request: request), operation: .revokeDevice(request))
        case .confirmPatch(let invocation):
            try prepareExecution(admission, id: id)
            guard case .patch = records[id]?.target else { throw NativeSettingsSessionError.invalidRequest }
            return try await execute(admission, operation: .confirmPatch(invocation))
        case .confirmDevice(let invocation):
            try prepareExecution(admission, id: id)
            guard case .device(let request) = records[id]?.target else { throw NativeSettingsSessionError.invalidRequest }
            try pruneInvitations()
            if request.action == .pair, records[id]?.executionTask == nil, invitations.count >= 8 {
                throw NativeSettingsSessionError.capacity
            }
            return try await execute(admission, operation: .confirmDevice(invocation))
        case .open, .present, .submitCredential: throw NativeSettingsSessionError.invalidRequest
        }
    }

    func submitCredential(_ admission: NativeSettingsAdmission, secret: String) async throws -> Outcome {
        try admission.validateOwner(gate)
        guard case .submitCredential(let invocation) = admission.intent else { throw NativeSettingsSessionError.invalidRequest }
        let id = invocation.operationID
        if let record = records[id], let intent = record.executionIntent, let task = record.executionTask {
            guard intent == admission.intent else { throw NativeSettingsSessionError.requestReused }
            return try await task.value.get()
        }
        if Task.isCancelled { admission.cancel() }
        try prepareExecution(admission, id: id)
        guard case .native(let request) = records[id]?.target, case .credentialEntry = request.action else {
            throw NativeSettingsSessionError.invalidRequest
        }
        return try await execute(admission, operation: .submitCredential(invocation, secret))
    }

    func present(_ admission: NativeSettingsAdmission,
                 presenter: @escaping @MainActor @Sendable (Presentation) async throws -> NativeActionObservation) async throws -> Outcome {
        try admission.validateOwner(gate)
        guard case .present(let invocation) = admission.intent else { throw NativeSettingsSessionError.invalidRequest }
        let id = invocation.operationID
        if let record = records[id], let intent = record.executionIntent, let task = record.executionTask {
            guard intent == admission.intent else { throw NativeSettingsSessionError.requestReused }
            return try await task.value.get()
        }
        if Task.isCancelled { admission.cancel() }
        try prepareExecution(admission, id: id)
        guard let record = records[id], case .native(let request) = record.target else { throw NativeSettingsSessionError.invalidRequest }
        switch request.action {
        case .notificationAuthorization, .openSection: break
        case .credentialEntry: throw NativeSettingsSessionError.invalidRequest
        }
        let task = Task { await self.finishPresentation(admission, invocation: invocation, presenter: presenter) }
        records[id]?.executionIntent = admission.intent
        records[id]?.executionTask = task
        return try await task.value.get()
    }

    func showInvitation(_ admission: NativeSettingsAdmission,
                        presenter: @escaping @MainActor @Sendable (PairingPresentation) async throws -> Void) async throws -> PairingStatus {
        try admission.validateOwner(gate)
        guard case .confirmDevice(let invocation) = admission.intent, let record = records[invocation.operationID],
              record.executionIntent == admission.intent,
              case .device(let request) = record.target, request.action == .pair else { throw NativeSettingsSessionError.invalidRequest }
        if let task = record.invitationTask { return try await task.value.get() }
        if Task.isCancelled { admission.cancel() }
        try admission.validate(for: gate)
        try requireLive()
        try pruneInvitations()
        guard let retained = invitations[invocation.operationID] else { throw NativeSettingsSessionError.expired }
        let task = Task { await self.finishInvitation(admission, retained: retained, presenter: presenter) }
        records[invocation.operationID]?.invitationTask = task
        return try await task.value.get()
    }

    nonisolated fileprivate func capturePairingActivity(expiresAt: UInt64) throws -> (NativeSettingsGate, NativeSettingsGate.Capture) {
        (gate, try gate.capture(expiresAt: expiresAt))
    }

    func approvePairing(_ admission: NativePairingApprovalAdmission) async throws -> PairingApprovalStatus {
        try admission.validateOwner(gate)
        let id = admission.review.operationID
        if let original = pairingApprovals[id] {
            guard original.admission === admission, let task = original.task else { throw NativeSettingsSessionError.requestReused }
            return try await task.value.get()
        }
        if Task.isCancelled { admission.cancel() }
        try admission.validate(gate)
        try requireLive()
        try pruneInvitations()
        let review = admission.review
        guard retainedOperationCount < 64, pairingApprovals.count < 8 else { throw NativeSettingsSessionError.capacity }
        guard !assistantIdentityExists(id) else { throw NativeSettingsSessionError.requestReused }
        guard !pairingApprovals.values.contains(where: { $0.admission.review.creationOperationID == review.creationOperationID }) else {
            throw NativeSettingsSessionError.requestReused
        }
        guard records[id] == nil, let creation = records[review.creationOperationID],
              case .device(let request) = creation.target, request.action == .pair,
              !creation.invitationRetired, let identity = creation.invitationIdentity,
              let invitation = invitations[review.creationOperationID], let information, let proof,
              identity.pairingID == review.request.pairingID, identity.expiresAt == review.request.expiresAt,
              review.request.hostID == information.hostID, request.scope == .host(hostID: information.hostID),
              ContinuousClock.now < identity.deadline else { throw NativeSettingsSessionError.invalidRequest }
        let original = try PairingInvitation(Data(invitation.value.displayText().utf8), now: NativeSettingsGate.wallTime())
        guard original.id == identity.pairingID, original.expires == identity.expiresAt,
              original.inviter.deviceID == information.hostID, original.inviter.fingerprint == identity.fingerprint else {
            throw NativeSettingsSessionError.invalidResponse
        }
        pairingApprovals[id] = PairingApprovalRecord(admission: admission, sessionID: proof.sessionID,
                                                    deadline: identity.deadline, invitation: original)
        let task = Task { await self.finishPairingApproval(admission) }
        pairingApprovals[id]?.task = task
        return try await task.value.get()
    }

    func pairingApproval(_ admission: NativePairingApprovalAdmission) async throws -> PairingApprovalStatus {
        try requireApproval(admission)
        if let task = pairingApprovals[admission.review.operationID]?.task { _ = await task.value }
        try requireApproval(admission)
        return try await readPairingApproval(admission)
    }

    func showPairingAcceptance(_ admission: NativePairingApprovalAdmission,
                               presenter: @escaping @MainActor @Sendable (PairingAcceptancePresentation) async throws -> Void) async throws -> PairingApprovalStatus {
        try requireApproval(admission)
        if let original = pairingApprovals[admission.review.operationID]?.presentation { return try await original.value.get() }
        if Task.isCancelled { admission.cancel() }
        try checkPairingDelivery(admission)
        let task = Task { await self.finishPairingAcceptance(admission, presenter: presenter) }
        pairingApprovals[admission.review.operationID]?.presentation = task
        return try await task.value.get()
    }

    private func requireApproval(_ admission: NativePairingApprovalAdmission) throws {
        try admission.validateOwner(gate)
        guard !finalizingDrain, !closeAcknowledged, proof != nil,
              pairingApprovals[admission.review.operationID]?.admission === admission else { throw NativeSettingsSessionError.closed }
    }

    private func checkPairingDelivery(_ admission: NativePairingApprovalAdmission) throws {
        do {
            try admission.validate(gate)
            try requireLive()
        } catch {
            retirePairingDelivery(admission)
            throw error
        }
        guard let original = pairingApprovals[admission.review.operationID], original.admission === admission,
              original.invitation != nil, original.sessionID == proof?.sessionID,
              let creation = records[admission.review.creationOperationID], let identity = creation.invitationIdentity,
              identity.pairingID == admission.review.request.pairingID,
              identity.expiresAt == admission.review.request.expiresAt,
              try NativeSettingsGate.wallTime() < identity.expiresAt,
              ContinuousClock.now < identity.deadline, ContinuousClock.now < original.deadline else {
            retirePairingDelivery(admission)
            throw NativeSettingsSessionError.expired
        }
    }

    private func retirePairingDelivery(_ admission: NativePairingApprovalAdmission) {
        admission.cancel()
        if pairingApprovals[admission.review.operationID]?.admission === admission {
            pairingApprovals[admission.review.operationID]?.invitation = nil
        }
    }

    private func finishPairingApproval(_ admission: NativePairingApprovalAdmission) async -> Result<PairingApprovalStatus, NativeSettingsSessionError> {
        defer { admission.retirePayload() }
        do {
            try await admission.validatePresentation(gate)
            try checkPairingDelivery(admission)
            let input = try admission.originalInput()
            guard let original = pairingApprovals[admission.review.operationID], let sessionDeadline = deadline else {
                throw NativeSettingsSessionError.closed
            }
            guard case .pairingApproved(let result) = try await rpc(.approvePairing(input), requestID: admission.rpcID,
                                                                   pairingAdmission: admission, pairingDeadline: min(original.deadline, sessionDeadline)) else {
                throw NativeSettingsSessionError.invalidResponse
            }
            return .success(try acceptPairingApproval(result, admission: admission))
        } catch { return .failure(sanitize(error)) }
    }

    private func acceptPairingApproval(_ result: NativeSettingsWire.PairingApprovalResult, admission: NativePairingApprovalAdmission) throws -> PairingApprovalStatus {
        let id = admission.review.operationID
        let review = admission.review
        let metadata = result.approval
        guard let original = pairingApprovals[id], original.admission === admission,
              result.operation.binding.clientSessionID == original.sessionID,
              result.operation.binding.expiresAt <= review.request.expiresAt,
              metadata.requestID == id, metadata.nativeRequestID == review.nativeRequestID,
              metadata.creationOperationID == review.creationOperationID, metadata.pairingID == review.request.pairingID,
              metadata.scope == .host(hostID: review.request.hostID), metadata.responder == review.request.responder,
              metadata.expiresAt == review.request.expiresAt, metadata.label == review.label,
              metadata.expectedFingerprint == review.expectedFingerprint, metadata.access == review.access,
              original.metadata == nil || original.metadata == metadata,
              original.status == nil || original.status?.operation.binding == result.operation.binding else {
            throw NativeSettingsSessionError.invalidResponse
        }
        if let previous = original.status, case .finished = previous.operation.phase,
           previous.operation.phase != result.operation.phase { throw NativeSettingsSessionError.invalidResponse }
        var available = false
        if result.acceptance != nil {
            do { try checkPairingDelivery(admission); available = true }
            catch { available = false }
        }
        let status = PairingApprovalStatus(operation: result.operation, approval: metadata, hostPin: result.hostPin, acceptanceAvailable: available)
        pairingApprovals[id]?.metadata = metadata
        pairingApprovals[id]?.status = status
        if case .finished(let outcome) = result.operation.phase {
            if case .completed = outcome.state {
                if result.acceptance == nil { retirePairingDelivery(admission) }
            } else { retirePairingDelivery(admission) }
        }
        if case .committed(_, _, _, .some) = result.hostPin { retirePairingDelivery(admission) }
        return status
    }

    private func readPairingApproval(_ admission: NativePairingApprovalAdmission) async throws -> PairingApprovalStatus {
        guard case .pairingApproved(let result) = try await rpc(.pairingApproval(admission.review.operationID)) else {
            throw NativeSettingsSessionError.invalidResponse
        }
        return try acceptPairingApproval(result, admission: admission)
    }

    private func finishPairingAcceptance(_ admission: NativePairingApprovalAdmission,
                                         presenter: @escaping @MainActor @Sendable (PairingAcceptancePresentation) async throws -> Void) async -> Result<PairingApprovalStatus, NativeSettingsSessionError> {
        do {
            if let task = pairingApprovals[admission.review.operationID]?.task { _ = await task.value }
            try await admission.validatePresentation(gate)
            try checkPairingDelivery(admission)
            guard case .pairingApproved(let result) = try await rpc(.pairingApproval(admission.review.operationID)),
                  let acceptance = result.acceptance, let original = pairingApprovals[admission.review.operationID],
                  let invitation = original.invitation else {
                throw NativeSettingsSessionError.operationUncertain
            }
            let status = try acceptPairingApproval(result, admission: admission)
            let bytes = try acceptance.transferData()
            let host = try invitation.finish(bytes, responder: admission.review.request.responder, now: NativeSettingsGate.wallTime())
            guard host == invitation.inviter else { throw NativeSettingsSessionError.invalidResponse }
            try checkPairingDelivery(admission)
            guard let sessionDeadline = deadline else { throw NativeSettingsSessionError.closed }
            let presentation = PairingAcceptancePresentation(operationID: admission.review.operationID, pairingID: acceptance.pairingID,
                                                           expiresAt: acceptance.expiresAt, value: bytes, admission: admission,
                                                           gate: gate, deadline: min(original.deadline, sessionDeadline))
            try await Self.invokePairingAcceptance(presentation: presentation, presenter: presenter)
            return .success(status)
        } catch { return .failure(sanitize(error)) }
    }

    @MainActor
    private static func invokePairingAcceptance(presentation: PairingAcceptancePresentation,
                                                presenter: @MainActor @Sendable (PairingAcceptancePresentation) async throws -> Void) async throws {
        try presentation.validateDelivery()
        try await presenter(presentation)
    }

    func inspect(operationID: UUID) async throws -> SettingsOperationInspection {
        try requireOriginal(operationID)
        return try await inspectOriginal(operationID, operation: .inspect(operationID))
    }

    func reconcile(operationID: UUID) async throws -> SettingsOperationInspection {
        try requireOriginal(operationID)
        return try await inspectOriginal(operationID, operation: .reconcile(operationID))
    }

    func retryFinalization(operationID: UUID) async throws -> SettingsOperationInspection {
        try requireOriginal(operationID)
        if let task = records[operationID]?.executionTask { _ = await task.value }
        try requireOriginal(operationID)
        guard let record = records[operationID], let observation = record.observation,
              case .present(let invocation) = record.executionIntent else { throw NativeSettingsSessionError.invalidRequest }
        return try await finalizeOriginal(operationID, invocation: invocation, observation: observation)
    }

    func cancel(operationID: UUID) async throws -> SettingsOperationInspection {
        try requireOriginal(operationID)
        if let record = records[operationID] {
            if let task = record.initialTask { _ = await task.value }
            if let task = record.executionTask { _ = await task.value }
            if let task = record.invitationTask { _ = await task.value }
        }
        try requireOriginal(operationID)
        return try await inspectOriginal(operationID, operation: .cancel(operationID))
    }

    func pairing(operationID: UUID) async throws -> PairingStatus {
        try requireOriginal(operationID)
        return try await pairingOriginal(operationID, cancel: false)
    }

    func cancelPairing(operationID: UUID) async throws -> PairingStatus {
        try requireOriginal(operationID)
        if let task = records[operationID]?.invitationTask { _ = await task.value }
        try requireOriginal(operationID)
        return try await pairingOriginal(operationID, cancel: true)
    }

    func drain(assistantOpens originals: [NativeSettingsAssistantOpenAdmission] = []) async -> DrainReport {
        gate.invalidate()
        closing = true
        for original in originals {
            do { try retainAssistantOpen(original) }
            catch {
                assistantDrainFailures.append(sanitize(error))
                assistantDrainUnresolved.insert(original.requestID)
            }
        }
        if let closingTask { return await closingTask.value }
        let task = Task { await self.finishDrain() }
        closingTask = task
        return await task.value
    }

    private func finishOpen(_ admission: NativeSettingsAdmission, started: ContinuousClock.Instant) async -> Result<Information, NativeSettingsSessionError> {
        var attempted = false
        do {
            try admission.validate(for: gate)
            attempted = true
            guard case .opened(let opened) = try await rpc(.open) else { throw NativeSettingsSessionError.invalidResponse }
            proof = opened.proof
            let value = Information(hostID: opened.hostID, expiresAt: opened.expiresAt, connectionKey: connectionKey)
            information = value
            let now = try NativeSettingsGate.wallTime()
            let remaining = opened.expiresAt > now ? min(opened.expiresAt - now, 300_000) : 0
            deadline = min(started.advanced(by: .seconds(300)), ContinuousClock.now.advanced(by: .milliseconds(Int64(remaining))))
            guard remaining > 0 else { throw NativeSettingsSessionError.expired }
            return .success(value)
        } catch let error as NativeSettingsSessionError {
            if proof != nil || !attempted { return .failure(error) }
            return .failure(.openUncertain)
        } catch { return .failure(.openUncertain) }
    }

    private func initial(_ admission: NativeSettingsAdmission, target: SettingsActionTarget, operation: NativeSettingsWire.Operation) async throws -> Outcome {
        let id = target.requestID
        guard pairingApprovals[id] == nil else { throw NativeSettingsSessionError.requestReused }
        if let record = records[id] {
            guard record.target == target, record.initialIntent == admission.intent, let task = record.initialTask else {
                throw NativeSettingsSessionError.requestReused
            }
            return try await task.value.get()
        }
        try admission.validate(for: gate)
        try requireLive()
        guard retainedOperationCount < 64 else { throw NativeSettingsSessionError.capacity }
        guard !assistantIdentityExists(id) else { throw NativeSettingsSessionError.requestReused }
        records[id] = Record(target: target, initialIntent: admission.intent)
        let task = Task { await self.finishInitial(admission, operation: operation, id: id) }
        records[id]?.initialTask = task
        return try await task.value.get()
    }

    private func finishInitial(_ admission: NativeSettingsAdmission, operation: NativeSettingsWire.Operation, id: UUID) async -> Result<Outcome, NativeSettingsSessionError> {
        do {
            try admission.validate(for: gate)
            try requireLive()
            guard let record = records[id] else { throw NativeSettingsSessionError.invalidRequest }
            let response = try await rpc(operation, requestID: record.initialRPC)
            return .success(try consume(response, id: id))
        } catch { return .failure(sanitize(error)) }
    }

    private func prepareExecution(_ admission: NativeSettingsAdmission, id: UUID) throws {
        if let record = records[id], let intent = record.executionIntent {
            guard intent == admission.intent else { throw NativeSettingsSessionError.requestReused }
            return
        }
        try admission.validate(for: gate)
        try requireLive()
        guard let record = records[id] else { throw NativeSettingsSessionError.invalidRequest }
        let invocation: NativeActionInvocation
        switch admission.intent {
        case .confirmPatch(let value), .confirmDevice(let value), .present(let value), .submitCredential(let value): invocation = value
        default: throw NativeSettingsSessionError.invalidRequest
        }
        let reference: NativeActionReference
        if let inspection = record.inspection {
            switch inspection.phase {
            case .awaitingConfirmation(let value): reference = .confirmation(reference: value)
            case .awaitingAction(let value): reference = .action(reference: value)
            default: throw NativeSettingsSessionError.operationUncertain
            }
        } else if let result = record.prepared {
            switch result.state {
            case .requiresConfirmation(let value): reference = .confirmation(reference: value)
            case .requiresUserAction(let value): reference = .action(reference: value)
            default: throw NativeSettingsSessionError.operationUncertain
            }
        } else { throw NativeSettingsSessionError.operationUncertain }
        guard invocation.reference == reference, admission.expiresAt == reference.expiresAt else {
            throw NativeSettingsSessionError.requestReused
        }
    }

    private func execute(_ admission: NativeSettingsAdmission, operation: NativeSettingsWire.Operation) async throws -> Outcome {
        guard let id = admission.intent.operationID, let record = records[id] else { throw NativeSettingsSessionError.invalidRequest }
        if let task = record.executionTask {
            guard record.executionIntent == admission.intent else { throw NativeSettingsSessionError.requestReused }
            return try await task.value.get()
        }
        let task = Task { await self.finishExecution(admission, operation: operation, id: id) }
        records[id]?.executionIntent = admission.intent
        records[id]?.executionTask = task
        return try await task.value.get()
    }

    private func finishExecution(_ admission: NativeSettingsAdmission, operation: NativeSettingsWire.Operation, id: UUID) async -> Result<Outcome, NativeSettingsSessionError> {
        do {
            try admission.validate(for: gate)
            try requireLive()
            guard let record = records[id] else { throw NativeSettingsSessionError.invalidRequest }
            return .success(try consume(try await rpc(operation, requestID: record.executionRPC), id: id))
        } catch { return .failure(sanitize(error)) }
    }

    private func finishPresentation(_ admission: NativeSettingsAdmission, invocation: NativeActionInvocation,
                                    presenter: @escaping @MainActor @Sendable (Presentation) async throws -> NativeActionObservation) async -> Result<Outcome, NativeSettingsSessionError> {
        let id = invocation.operationID
        do {
            try admission.validate(for: gate)
            try requireLive()
            guard let record = records[id] else { throw NativeSettingsSessionError.invalidRequest }
            let response = try await rpc(.admit(invocation), requestID: record.executionRPC)
            if case .inspected = response { return .success(try consume(response, id: id)) }
            guard case .admitted(let inspection, let target) = response, target == record.target,
                  case .native(let request) = target else { throw NativeSettingsSessionError.invalidResponse }
            try accept(inspection, id: id)
            let presented = await Self.invokePresentation(admission, gate: gate,
                                                         presentation: Presentation(invocation: invocation, action: request.action), presenter: presenter)
            let observation: NativeActionObservation
            switch presented {
            case .notStarted: observation = .dismissed
            case .observed(let value): observation = value
            case .uncertain:
                records[id]?.presentationUncertain = true
                throw NativeSettingsSessionError.operationUncertain
            }
            records[id]?.observation = observation
            return .success(.inspected(try await finalizeOriginal(id, invocation: invocation, observation: observation)))
        } catch { return .failure(sanitize(error)) }
    }

    @MainActor
    private static func invokePresentation(_ admission: NativeSettingsAdmission, gate: NativeSettingsGate, presentation: Presentation,
                                           presenter: @MainActor @Sendable (Presentation) async throws -> NativeActionObservation) async -> Presented {
        do { try admission.validatePresentation(for: gate) }
        catch { return .notStarted }
        do {
            let observation = try await presenter(presentation)
            switch (presentation.action, observation) {
            case (_, .dismissed), (.notificationAuthorization, .notificationAuthorization): break
            case (.openSection(let expected), .sectionOpened(let actual)) where expected == actual: break
            default: return .uncertain
            }
            return .observed(observation)
        } catch { return .uncertain }
    }

    private func finishInvitation(_ admission: NativeSettingsAdmission, retained: RetainedInvitation,
                                  presenter: @escaping @MainActor @Sendable (PairingPresentation) async throws -> Void) async -> Result<PairingStatus, NativeSettingsSessionError> {
        let value = retained.value
        do {
            try await Self.invokeInvitation(admission, gate: gate, retained: retained, presenter: presenter)
            return .success(try await pairingOriginal(value.operationID, cancel: true))
        } catch {
            do { _ = try await pairingOriginal(value.operationID, cancel: true) }
            catch { return .failure(.operationUncertain) }
            return .failure(sanitize(error))
        }
    }

    @MainActor
    private static func invokeInvitation(_ admission: NativeSettingsAdmission, gate: NativeSettingsGate, retained: RetainedInvitation,
                                         presenter: @MainActor @Sendable (PairingPresentation) async throws -> Void) async throws {
        try admission.validatePresentation(for: gate)
        let value = retained.value
        guard try NativeSettingsGate.wallTime() < value.expiresAt, ContinuousClock.now < retained.deadline else {
            throw NativeSettingsSessionError.expired
        }
        try await presenter(PairingPresentation(operationID: value.operationID, pairingID: value.pairingID,
                                               expiresAt: value.expiresAt, fingerprint: value.fingerprint, invitation: value.displayText()))
    }

    private func requireLive() throws {
        guard !closing else { throw NativeSettingsSessionError.closed }
        try gate.requireCurrentActivity()
        guard let information, proof != nil, let deadline else { throw NativeSettingsSessionError.closed }
        guard try NativeSettingsGate.wallTime() < information.expiresAt, ContinuousClock.now < deadline else {
            throw NativeSettingsSessionError.expired
        }
    }

    private func requireOriginal(_ id: UUID) throws {
        guard !finalizingDrain, !closeAcknowledged, proof != nil, records[id] != nil else { throw NativeSettingsSessionError.closed }
    }

    private func rpc(_ operation: NativeSettingsWire.Operation, requestID: UUID = UUID(),
                     pairingAdmission: NativePairingApprovalAdmission? = nil,
                     pairingDeadline: ContinuousClock.Instant? = nil,
                     assistantOpen: NativeSettingsAssistantOpenAdmission? = nil,
                     assistantSend: NativeSettingsAssistantSendAdmission? = nil,
                     assistantChat: AssistantHandle? = nil) async throws -> NativeSettingsWire.Response {
        guard calls.count < 4, !closeAcknowledged else { throw NativeSettingsSessionError.capacity }
        let originalProof = proof
        let originalHostID = information?.hostID
        let originalExpiry = information?.expiresAt
        let originalDeadline = deadline
        let bytes: Data
        do { bytes = try NativeSettingsWire.encode(operation, requestID: requestID, proof: originalProof) }
        catch { throw NativeSettingsSessionError.invalidRequest }
        let identifier = UUID()
        let client = client
        let connection = connection
        let gate = gate
        let task = Task<Result<NativeSettingsWire.Response, NativeSettingsSessionError>, Never> {
            do {
                if let pairingAdmission {
                    try await pairingAdmission.validatePresentation(gate)
                    guard let pairingDeadline, ContinuousClock.now < pairingDeadline else {
                        throw NativeSettingsSessionError.expired
                    }
                }
                if let assistantOpen {
                    try await assistantOpen.validatePresentation(gate)
                    guard let originalExpiry, let originalDeadline, try NativeSettingsGate.wallTime() < originalExpiry,
                          ContinuousClock.now < originalDeadline else { throw NativeSettingsSessionError.expired }
                }
                if let assistantSend { try await assistantSend.validatePresentation(gate) }
                if let assistantChat { try await assistantChat.validatePresentation(gate) }
                let response = try await client.requestSensitive(bytes, connection: connection)
                do { return .success(try NativeSettingsWire.decode(response, for: operation, proof: originalProof,
                                                                    requestID: requestID, hostID: originalHostID)) }
                catch { return .failure(.invalidResponse) }
            } catch {
                if assistantOpen != nil || assistantSend != nil || assistantChat != nil {
                    return .failure(error as? NativeSettingsSessionError ?? .operationUncertain)
                }
                return .failure(.operationUncertain)
            }
        }
        calls[identifier] = task
        let result = await task.value
        calls.removeValue(forKey: identifier)
        return try result.get()
    }

    private func consume(_ response: NativeSettingsWire.Response, id: UUID) throws -> Outcome {
        switch response {
        case .prepared(let result):
            guard let record = records[id], result.requestID == id, result.scope == record.target.scope else { throw NativeSettingsSessionError.invalidResponse }
            records[id]?.prepared = result
            return .prepared(result)
        case .inspected(let operation):
            try accept(operation, id: id)
            return .inspected(operation)
        case .deviceConfirmed(.existing(let operation)), .deviceConfirmed(.revoked(let operation)):
            try accept(operation, id: id)
            return .inspected(operation)
        case .deviceConfirmed(.pairing(let creation)), .pairing(let creation): return .pairing(try acceptPairing(creation, id: id))
        default: throw NativeSettingsSessionError.invalidResponse
        }
    }

    private func accept(_ operation: SettingsOperationInspection, id: UUID) throws {
        guard let original = records[id], operation.binding.requestID == id,
              operation.binding.scope == original.target.scope,
              operation.binding.expectedRevision == original.target.expectedRevision,
              original.inspection == nil || original.inspection?.binding == operation.binding else {
            throw NativeSettingsSessionError.invalidResponse
        }
        records[id]?.inspection = operation
    }

    private func acceptPairing(_ creation: NativeSettingsWire.PairingCreation, id: UUID) throws -> PairingStatus {
        try accept(creation.operation, id: id)
        guard case .device(let request) = records[id]?.target, request.action == .pair else { throw NativeSettingsSessionError.invalidResponse }
        try pruneInvitations()
        if creation.lifecycle == .retained, let invitation = creation.invitation, records[id]?.invitationRetired != true {
            let now = try NativeSettingsGate.wallTime()
            if let original = records[id]?.invitationIdentity {
                guard original.pairingID == invitation.pairingID, original.expiresAt == invitation.expiresAt,
                      original.fingerprint == invitation.fingerprint else { throw NativeSettingsSessionError.invalidResponse }
            } else {
                guard let sessionDeadline = deadline else { throw NativeSettingsSessionError.closed }
                let remaining = invitation.expiresAt > now ? min(invitation.expiresAt - now, 300_000) : 0
                records[id]?.invitationIdentity = InvitationIdentity(pairingID: invitation.pairingID, expiresAt: invitation.expiresAt,
                    fingerprint: invitation.fingerprint, deadline: min(sessionDeadline, ContinuousClock.now.advanced(by: .milliseconds(Int64(remaining)))))
            }
            guard let original = records[id]?.invitationIdentity else { throw NativeSettingsSessionError.invalidResponse }
            if now < original.expiresAt, ContinuousClock.now < original.deadline {
                guard invitations[id] != nil || invitations.count < 8 else { throw NativeSettingsSessionError.capacity }
                if invitations[id] == nil { invitations[id] = RetainedInvitation(value: invitation, deadline: original.deadline) }
            } else {
                records[id]?.invitationRetired = true
                invitations.removeValue(forKey: id)
            }
        } else {
            invitations.removeValue(forKey: id)
            records[id]?.invitationRetired = true
        }
        let status = PairingStatus(operation: creation.operation, lifecycle: creation.lifecycle, invitationAvailable: invitations[id] != nil)
        records[id]?.pairing = status
        return status
    }

    private func pruneInvitations() throws {
        let wall: UInt64
        do { wall = try NativeSettingsGate.wallTime() }
        catch {
            invitations.removeAll()
            throw error
        }
        let expired = invitations.filter { _, item in
            wall >= item.value.expiresAt || ContinuousClock.now >= item.deadline
        }.map(\.key)
        for id in expired {
            invitations.removeValue(forKey: id)
            records[id]?.invitationRetired = true
        }
    }

    private func inspectOriginal(_ id: UUID, operation: NativeSettingsWire.Operation) async throws -> SettingsOperationInspection {
        guard case .inspected(let inspection) = try await rpc(operation) else { throw NativeSettingsSessionError.invalidResponse }
        try accept(inspection, id: id)
        return inspection
    }

    private func finalizeOriginal(_ id: UUID, invocation: NativeActionInvocation, observation: NativeActionObservation) async throws -> SettingsOperationInspection {
        guard let record = records[id], record.observation == observation,
              record.executionIntent == .present(invocation) else { throw NativeSettingsSessionError.invalidRequest }
        guard case .inspected(let inspection) = try await rpc(.finalize(invocation, observation), requestID: record.finalizationRPC) else {
            throw NativeSettingsSessionError.invalidResponse
        }
        try accept(inspection, id: id)
        return inspection
    }

    private func pairingOriginal(_ id: UUID, cancel: Bool) async throws -> PairingStatus {
        if cancel {
            for record in Array(pairingApprovals.values) where record.admission.review.creationOperationID == id {
                retirePairingDelivery(record.admission)
            }
        }
        guard case .pairing(let creation) = try await rpc(cancel ? .cancelPairing(id) : .pairing(id)) else {
            throw NativeSettingsSessionError.invalidResponse
        }
        return try acceptPairing(creation, id: id)
    }

    private func finishDrain() async -> DrainReport {
        var failures = assistantDrainFailures
        var unresolvedAssistantOpens = assistantDrainUnresolved
        var pairingLifecycles: [UUID: NativeSettingsWire.PairingLifecycle] = [:]
        var pairingUnresolved: Set<UUID> = []
        if let openingTask, case .failure(let error) = await openingTask.value { failures.append(error) }
        let approvals = Array(pairingApprovals.values)
        for record in approvals {
            retirePairingDelivery(record.admission)
            if let task = record.task, case .failure(let error) = await task.value { failures.append(error) }
            if let presentation = record.presentation, case .failure(let error) = await presentation.value { failures.append(error) }
        }
        let owned = Array(records.values)
        for record in owned {
            if let task = record.initialTask, case .failure(let error) = await task.value { failures.append(error) }
            if let task = record.executionTask, case .failure(let error) = await task.value { failures.append(error) }
            if let task = record.invitationTask, case .failure(let error) = await task.value { failures.append(error) }
        }
        let assistantOriginals = assistantOpens.values.map(\.admission)
        for original in assistantOriginals {
            assistantOpens[original.requestID]?.retiring = true
            if let task = assistantOpens[original.requestID]?.task { _ = await task.value }
        }
        for original in Array(assistantSends.values) {
            original.admission.cancel()
            if let task = original.task { _ = await task.value }
        }
        for flight in Array(assistantReads.values) { _ = await flight.task.value }
        finalizingDrain = true
        while !calls.isEmpty {
            let pending = Array(calls.values)
            for task in pending { _ = await task.value }
            await Task.yield()
        }
        for original in assistantOriginals {
            do {
                guard proof != nil else { throw NativeSettingsSessionError.operationUncertain }
                let state = try await closeAssistantOriginal(original)
                guard assistantOriginalClosed(state) else { throw NativeSettingsSessionError.operationUncertain }
            } catch {
                unresolvedAssistantOpens.insert(original.requestID)
                failures.append(sanitize(error))
            }
        }
        if !unresolvedAssistantOpens.isEmpty, !failures.contains(.operationUncertain) { failures.append(.operationUncertain) }
        let identifiers = records.keys.sorted { $0.uuidString < $1.uuidString }
        if proof != nil {
            for id in identifiers {
                do {
                    if case .device(let request) = records[id]?.target, request.action == .pair {
                        pairingUnresolved.insert(id)
                        let status = try await pairingOriginal(id, cancel: true)
                        pairingLifecycles[id] = status.lifecycle
                        switch status.lifecycle {
                        case .cancelled, .expired, .noLongerPending: pairingUnresolved.remove(id)
                        case .retained, .cancellationPending, .unavailable: failures.append(.operationUncertain)
                        }
                    }
                    let inspection = try await inspectOriginal(id, operation: .inspect(id))
                    if let record = records[id], let observation = record.observation,
                       case .present(let invocation) = record.executionIntent {
                        _ = try await finalizeOriginal(id, invocation: invocation, observation: observation)
                    } else if inspection.write?.resolution == nil, inspection.write != nil {
                        _ = try await inspectOriginal(id, operation: .reconcile(id))
                    } else {
                        switch inspection.phase {
                        case .awaitingAction, .awaitingConfirmation, .inFlight:
                            _ = try await inspectOriginal(id, operation: .cancel(id))
                        case .applying, .finished: break
                        }
                    }
                } catch { failures.append(sanitize(error)) }
            }
            for record in approvals {
                do { _ = try await readPairingApproval(record.admission) }
                catch { failures.append(sanitize(error)) }
            }
            if unresolvedAssistantOpens.isEmpty {
                do {
                    guard case .closed = try await rpc(.close, requestID: closeRequestID) else { throw NativeSettingsSessionError.invalidResponse }
                    closeAcknowledged = true
                    proof = nil
                } catch { failures.append(sanitize(error)) }
            }
        }
        invitations.removeAll()
        let unresolved = identifiers.filter { id in
            if records[id]?.presentationUncertain == true { return true }
            guard let operation = records[id]?.inspection else { return true }
            if operation.write != nil, operation.write?.resolution == nil { return true }
            switch operation.phase {
            case .finished(let result): return result.state == .unknown
            default: return true
            }
        }
        let approvalUnresolved = pairingApprovals.compactMap { id, record -> UUID? in
            guard let status = record.status, case .finished(let result) = status.operation.phase, result.state != .unknown else { return id }
            return nil
        }
        let unresolvedIDs = Set(unresolved + approvalUnresolved).union(pairingUnresolved)
        return DrainReport(failures: failures, unresolvedOperationIDs: unresolvedIDs.sorted { $0.uuidString < $1.uuidString },
                           pairingLifecycles: pairingLifecycles,
                           unresolvedAssistantOpenRequestIDs: unresolvedAssistantOpens.sorted { $0.uuidString < $1.uuidString })
    }

    private func sanitize(_ error: any Error) -> NativeSettingsSessionError {
        error as? NativeSettingsSessionError ?? .operationUncertain
    }
}

final class NativePairingCandidate: @unchecked Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    struct Review: Equatable, Sendable {
        let pairingID: UUID
        let hostID: UUID
        let responder: DevicePublicKeys
        let expiresAt: UInt64
    }

    let review: Review
    private let lock = NSLock()
    private var request: NativeSettingsWire.SignedPairingRequest?

    var description: String { "NativePairingCandidate([private])" }
    var debugDescription: String { description }

    init(privateRequest data: Data) throws {
        let value = try NativeSettingsWire.SignedPairingRequest.parse(data)
        review = Review(pairingID: value.pairingID, hostID: value.inviter, responder: value.responder, expiresAt: value.expiresAt)
        request = value
    }

    func discard() { lock.withLock { request = nil } }

    fileprivate func take() throws -> NativeSettingsWire.SignedPairingRequest {
        try lock.withLock {
            guard let value = request else { throw NativeSettingsSessionError.requestReused }
            request = nil
            return value
        }
    }
}

final class NativePairingApprovalAdmission: @unchecked Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    struct Review: Equatable, Sendable {
        let operationID: UUID
        let nativeRequestID: UUID
        let creationOperationID: UUID
        let request: NativePairingCandidate.Review
        let label: String
        let expectedFingerprint: String
        let access: NativeSettingsWire.PairingAccess
    }

    let review: Review
    fileprivate let rpcID = UUID()
    private let owner: NativeSettingsGate
    private let activity: NativeSettingsGate.Capture
    private let lock = NSLock()
    private var cancelled = false
    private var payload: NativeSettingsWire.PairingApprovalInput?

    var description: String { "NativePairingApprovalAdmission([private])" }
    var debugDescription: String { description }

    private init(input: NativeSettingsWire.PairingApprovalInput, owner: NativeSettingsGate, activity: NativeSettingsGate.Capture) {
        review = Review(operationID: input.operationID, nativeRequestID: input.nativeRequestID,
                        creationOperationID: input.creationOperationID,
                        request: NativePairingCandidate.Review(pairingID: input.request.pairingID, hostID: input.request.inviter,
                                                             responder: input.request.responder, expiresAt: input.request.expiresAt),
                        label: input.label, expectedFingerprint: input.expectedFingerprint, access: input.access)
        self.owner = owner
        self.activity = activity
        payload = input
    }

    @MainActor
    static func capture(session: NativeSettingsSession, creationOperationID: UUID, candidate: NativePairingCandidate,
                        label: String, expectedFingerprint: String, access: NativeSettingsWire.PairingAccess) throws -> NativePairingApprovalAdmission {
        guard NSApplication.shared.isActive else { throw NativeSettingsSessionError.inactive }
        try SettingsWire.identifier(creationOperationID)
        try SettingsWire.text(label, maximum: 256)
        try access.validate()
        guard expectedFingerprint == candidate.review.responder.fingerprint else { throw NativeSettingsSessionError.invalidRequest }
        let (gate, activity) = try session.capturePairingActivity(expiresAt: candidate.review.expiresAt)
        let input = NativeSettingsWire.PairingApprovalInput(operationID: UUID(), nativeRequestID: UUID(), creationOperationID: creationOperationID,
                                                          request: try candidate.take(), label: label, expectedFingerprint: expectedFingerprint, access: access)
        try input.validate()
        return NativePairingApprovalAdmission(input: input, owner: gate, activity: activity)
    }

    func cancel() {
        lock.withLock {
            cancelled = true
            payload = nil
        }
    }

    fileprivate func validateOwner(_ gate: NativeSettingsGate) throws {
        guard owner === gate else { throw NativeSettingsSessionError.invalidRequest }
    }

    fileprivate func validate(_ gate: NativeSettingsGate) throws {
        try validateOwner(gate)
        guard !lock.withLock({ cancelled }) else { throw NativeSettingsSessionError.inactive }
        try gate.validate(activity)
    }

    @MainActor
    fileprivate func validatePresentation(_ gate: NativeSettingsGate) throws {
        guard NSApplication.shared.isActive else { throw NativeSettingsSessionError.inactive }
        try validate(gate)
    }

    fileprivate func originalInput() throws -> NativeSettingsWire.PairingApprovalInput {
        try lock.withLock {
            guard !cancelled, let payload else { throw NativeSettingsSessionError.inactive }
            return payload
        }
    }

    fileprivate func retirePayload() { lock.withLock { payload = nil } }
}

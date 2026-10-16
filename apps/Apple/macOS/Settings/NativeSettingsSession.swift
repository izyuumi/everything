import Foundation

actor NativeSettingsSession {
    struct Information: Equatable, Sendable {
        let hostID: UUID
        let expiresAt: UInt64
        let connectionKey: UUID
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
        guard records[operationID] != nil || records.count < 64 else { throw NativeSettingsSessionError.capacity }
        guard case .details(let operation, let target) = try await rpc(.details(operationID, reference)) else {
            throw NativeSettingsSessionError.invalidResponse
        }
        if let original = records[operationID] {
            guard original.target == target else { throw NativeSettingsSessionError.requestReused }
        } else {
            guard records.count < 64 else { throw NativeSettingsSessionError.capacity }
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

    func drain() async -> DrainReport {
        gate.invalidate()
        closing = true
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
        if let record = records[id] {
            guard record.target == target, record.initialIntent == admission.intent, let task = record.initialTask else {
                throw NativeSettingsSessionError.requestReused
            }
            return try await task.value.get()
        }
        try admission.validate(for: gate)
        try requireLive()
        guard records.count < 64 else { throw NativeSettingsSessionError.capacity }
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

    private func rpc(_ operation: NativeSettingsWire.Operation, requestID: UUID = UUID()) async throws -> NativeSettingsWire.Response {
        guard calls.count < 4, !closeAcknowledged else { throw NativeSettingsSessionError.capacity }
        let originalProof = proof
        let bytes: Data
        do { bytes = try NativeSettingsWire.encode(operation, requestID: requestID, proof: originalProof) }
        catch { throw NativeSettingsSessionError.invalidRequest }
        let identifier = UUID()
        let client = client
        let connection = connection
        let task = Task<Result<NativeSettingsWire.Response, NativeSettingsSessionError>, Never> {
            do {
                let response = try await client.requestSensitive(bytes, connection: connection)
                do { return .success(try NativeSettingsWire.decode(response, for: operation, proof: originalProof)) }
                catch { return .failure(.invalidResponse) }
            } catch { return .failure(.operationUncertain) }
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
        guard case .pairing(let creation) = try await rpc(cancel ? .cancelPairing(id) : .pairing(id)) else {
            throw NativeSettingsSessionError.invalidResponse
        }
        return try acceptPairing(creation, id: id)
    }

    private func finishDrain() async -> DrainReport {
        var failures: [NativeSettingsSessionError] = []
        if let openingTask, case .failure(let error) = await openingTask.value { failures.append(error) }
        let owned = Array(records.values)
        for record in owned {
            if let task = record.initialTask, case .failure(let error) = await task.value { failures.append(error) }
            if let task = record.executionTask, case .failure(let error) = await task.value { failures.append(error) }
            if let task = record.invitationTask, case .failure(let error) = await task.value { failures.append(error) }
        }
        finalizingDrain = true
        while !calls.isEmpty {
            let pending = Array(calls.values)
            for task in pending { _ = await task.value }
            await Task.yield()
        }
        let identifiers = records.keys.sorted { $0.uuidString < $1.uuidString }
        if proof != nil {
            for id in identifiers {
                do {
                    if case .device(let request) = records[id]?.target, request.action == .pair {
                        _ = try await pairingOriginal(id, cancel: true)
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
            do {
                guard case .closed = try await rpc(.close, requestID: closeRequestID) else { throw NativeSettingsSessionError.invalidResponse }
                closeAcknowledged = true
                proof = nil
            } catch { failures.append(sanitize(error)) }
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
        return DrainReport(failures: failures, unresolvedOperationIDs: unresolved)
    }

    private func sanitize(_ error: any Error) -> NativeSettingsSessionError {
        error as? NativeSettingsSessionError ?? .operationUncertain
    }
}

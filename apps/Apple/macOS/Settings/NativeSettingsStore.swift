import AppKit
import Foundation
import Observation

@MainActor
@Observable
final class NativeSettingsStore {
    struct Action: Sendable {
        let operationID: UUID
        let snapshot: SettingsSnapshot
        let registry: SettingRegistry
        fileprivate let ownerID: UUID
        fileprivate let admission: NativeSettingsAdmission
    }

    struct Cleanup: Identifiable, Sendable {
        let id: UUID
        let connectionKey: UUID
        let report: NativeSettingsSession.DrainReport?

        var hasIssues: Bool {
            guard let report else { return false }
            return !report.failures.isEmpty || !report.unresolvedOperationIDs.isEmpty
        }
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
        var reloadAfterRead = false
        var refreshRequested = false
        var retirement: Task<Cleanup, Never>?
        var report: NativeSettingsSession.DrainReport?

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
    var actionCapacityReached: Bool { (current?.operations.count ?? 0) >= 64 }

    var canEdit: Bool { !isClosed && !isOpening && !isRefreshing && draft != nil && !operations.contains(where: \.blocksNewChanges) }

    var canSave: Bool {
        canEdit && !snapshotIsStale && !draftConflict && draft?.isDirty == true
            && draft?.validationMessage == nil && snapshot?.revision == draft?.baseline.revision
            && registry == draft?.registry
            && snapshot?.revision != UInt64(Int64.max) && (current?.operations.count ?? 64) < 64
    }

    var canAct: Bool { canRefresh && !snapshotIsStale && current?.effects.isEmpty == true }

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
        surfaceVisible = visible
        current?.lifecycle.setSurfaceVisible(visible)
        if isForeground && observationIsStale { requestRefresh() }
        updateObserver()
    }

    func setApplicationActive(_ active: Bool) {
        guard !isClosed else { return }
        if applicationActive != active { invalidateObservation() }
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
        guard owner.operations.count < 64 else { throw NativeSettingsSessionError.capacity }
        guard !owner.operations.values.contains(where: \.blocksNewChanges) else { throw NativeSettingsSessionError.operationUncertain }
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
              owner.actions[target.requestID] == nil else { throw NativeSettingsSessionError.invalidRequest }
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

    func hostAvailabilityChanged(_ available: Bool) {
        guard !isClosed else { return }
        invalidateObservation()
        stopObserver()
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
        let tasks = owners.values.compactMap(\.retirement)
        let task = Task {
            var reports: [Cleanup] = []
            for task in tasks { reports.append(await task.value) }
            return reports
        }
        drainTask = task
        return await task.value
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
        owner.retirement = Task { [self, owner, work, observer, effects] in
            if let work { await work.value }
            if let observer { await observer.value }
            for effect in effects { await effect.value }
            let report = await owner.lifecycle.drain()
            let result = Cleanup(id: owner.id, connectionKey: owner.connectionKey, report: report)
            owner.report = report
            if !result.hasIssues { owners.removeValue(forKey: owner.id) }
            refreshCleanup()
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
        issue = nil
        invalidateObservation()
    }

    private func refreshCleanup() {
        retainedOwnerCount = owners.count
        cleanup = owners.values.filter { $0.id != currentID }.map {
            Cleanup(id: $0.id, connectionKey: $0.connectionKey, report: $0.report)
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
                case .inspected(let inspection): try owner.operations[id]?.adopt(inspection)
                case .pairing(let status): try owner.operations[id]?.adopt(status.operation)
                }
            } catch { owner.operations[id]?.issue = sanitize(error) }
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
        guard isCurrent(owner) else { return }
        operations = owner.operations.values.sorted { $0.id.uuidString < $1.id.uuidString }
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
                if isCurrent(owner) { issue = sanitize(error); invalidateObservation() }
            }
            if owner.observerID == observerID {
                owner.observer = nil
                owner.observerID = nil
            }
            if Task.isCancelled && isCurrent(owner) && isForeground && !observationIsStale { updateObserver() }
        }
    }
}

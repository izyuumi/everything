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
            return !report.failures.isEmpty || !report.unresolvedOperationIDs.isEmpty
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
            && current.map({ !credentialLinksNeedRecovery($0) }) == true
    }

    var canSave: Bool {
        canEdit && !snapshotIsStale && !draftConflict && draft?.isDirty == true
            && draft?.validationMessage == nil && snapshot?.revision == draft?.baseline.revision
            && registry == draft?.registry
            && snapshot?.revision != UInt64(Int64.max) && current.map({ identityCount($0) < 64 }) == true
    }

    var canAct: Bool { canRefresh && !snapshotIsStale && current?.effects.isEmpty == true }

    var canEnterCredential: Bool {
        canAct && !actionCapacityReached && draft?.isDirty == false && metadata != nil
            && metadata?.revision == snapshot?.revision && credentialPresentation == nil
            && !operations.contains(where: \.blocksNewChanges) && current.map({ !credentialLinksNeedRecovery($0) }) == true
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
        if surfaceVisible != visible { retireCredentialPresentations() }
        surfaceVisible = visible
        current?.lifecycle.setSurfaceVisible(visible)
        if isForeground && observationIsStale { requestRefresh() }
        updateObserver()
    }

    func setApplicationActive(_ active: Bool) {
        guard !isClosed else { return }
        if applicationActive != active { invalidateObservation() }
        if applicationActive != active { retireCredentialPresentations() }
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
        guard identityCount(owner) < 64 else { throw NativeSettingsSessionError.capacity }
        guard !owner.operations.values.contains(where: \.blocksNewChanges), !credentialLinksNeedRecovery(owner) else {
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
            guard owner.operations[requestID.value] == nil,
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
        if !available { retireCredentialPresentations() }
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
        let retirementID = UUID()
        owner.retirementID = retirementID
        owner.retirement = Task { [self, owner, work, observer, effects] in
            if let work { await work.value }
            if let observer { await observer.value }
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
                case .inspected(let inspection): try owner.operations[id]?.adopt(inspection)
                case .pairing(let status): try owner.operations[id]?.adopt(status.operation)
                }
                if let context = owner.credentials[id], context.details == nil {
                    if context.retired, case .awaitingAction = owner.operations[id]?.inspection?.phase {
                        let cancelled = try await owner.lifecycle.session.cancel(operationID: id)
                        try owner.operations[id]?.adopt(cancelled)
                    } else if !context.retired { try await prepareCredentialPresentation(owner, context: context) }
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
        if isCurrent(owner) { operations = owner.operations.values.sorted { $0.id.uuidString < $1.id.uuidString } }
        publishCredentials()
    }

    private func identityCount(_ owner: Owner) -> Int {
        owner.operations.count + owner.credentials.values.filter { $0.initial || $0.link != nil }.count
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
        return NativeSettingsSession.DrainReport(failures: failures,
            unresolvedOperationIDs: unresolved.sorted { $0.uuidString < $1.uuidString })
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
                if isCurrent(owner) { issue = sanitize(error); invalidateObservation(); retireCredentialPresentations() }
            }
            if owner.observerID == observerID {
                owner.observer = nil
                owner.observerID = nil
            }
            if Task.isCancelled && isCurrent(owner) && isForeground && !observationIsStale { updateObserver() }
        }
    }
}

import AppKit
import Foundation
import Observation

@MainActor
@Observable
final class ThisMacSettingsStore {
    struct Draft: Equatable {
        let baseline: DevicePreferenceSnapshot
        var completion: Bool
        var attention: Bool

        init(snapshot: DevicePreferenceSnapshot) {
            baseline = snapshot
            completion = snapshot.completion
            attention = snapshot.attention
        }

        var isModified: Bool {
            completion != baseline.completion || attention != baseline.attention
        }
    }

    struct AppearanceDraft: Equatable {
        let baseline: DeviceAppearanceSnapshot
        var appearance: DeviceAppearance

        init(snapshot: DeviceAppearanceSnapshot) {
            baseline = snapshot
            appearance = snapshot.appearance
        }

        var isModified: Bool { appearance != baseline.appearance }
    }

    enum NotificationIssue {
        case inactive
        case capacity
        case invalidRequest

        var message: String {
            switch self {
            case .inactive: "Open Settings in the active Everything app before requesting permission."
            case .capacity: "This app session has reached its notification action limit."
            case .invalidRequest: "The notification action could not be prepared."
            }
        }
    }

    private struct PreferenceRecord {
        let patch: DevicePreferencePatch
        let baseline: DevicePreferenceSnapshot
        var result: DevicePreferenceResult?
    }

    private struct NotificationRecord {
        let admission: DeviceNotificationActions.Admission
        var result: DeviceNotificationActionResult?
    }

    private struct AppearanceRecord {
        let patch: DeviceAppearancePatch
        var result: DevicePreferenceResult?
    }

    private(set) var snapshot: DevicePreferenceSnapshot?
    private(set) var draft: Draft?
    private(set) var pendingPatch: DevicePreferencePatch?
    private(set) var lastPreferenceResult: DevicePreferenceResult?
    private(set) var preferenceError: DevicePreferenceError?
    private(set) var appearanceSnapshot: DeviceAppearanceSnapshot?
    private(set) var appearanceDraft: AppearanceDraft?
    private(set) var pendingAppearancePatch: DeviceAppearancePatch?
    private(set) var lastAppearanceResult: DevicePreferenceResult?
    private(set) var appearanceError: DevicePreferenceError?
    private(set) var notificationObservation: DeviceNotificationObservation?
    private(set) var lastNotificationResult: DeviceNotificationActionResult?
    private(set) var notificationIssue: NotificationIssue?
    private(set) var notificationGuidance: DeviceNotificationGuidance = .none
    private(set) var isOpening = false
    private(set) var isRefreshing = false
    private(set) var isSaving = false
    private(set) var isInspecting = false
    private(set) var isSavingAppearance = false
    private(set) var isInspectingAppearance = false
    private(set) var isAuthorizing = false
    private(set) var isObservingAuthorization = false
    private(set) var isClosed = false

    @ObservationIgnored private let directory: URL
    @ObservationIgnored private let authorization: NotificationAuthorizationService
    @ObservationIgnored private let notificationActions: DeviceNotificationActions
    @ObservationIgnored private var preferences: DevicePreferenceStore?
    @ObservationIgnored private var foregroundSession: DeviceForegroundSession?
    private var surfaceVisible = false
    private var applicationActive = false
    private var snapshotReadFailed = true
    private var appearanceReadFailed = true
    private var expectedReadback: DevicePreferenceSnapshot?
    private var expectedAppearanceReadback: DeviceAppearanceSnapshot?
    private var preferenceRecords: [UUID: PreferenceRecord] = [:]
    private var appearanceRecords: [UUID: AppearanceRecord] = [:]
    private var notificationRecords: [UUID: NotificationRecord] = [:]
    @ObservationIgnored private var openingTask: Task<Void, Never>?
    @ObservationIgnored private var refreshTask: Task<Void, Never>?
    @ObservationIgnored private var snapshotReadTask: (id: UUID, task: Task<Void, Never>)?
    @ObservationIgnored private var writeTask: Task<Void, Never>?
    @ObservationIgnored private var inspectionTask: Task<Void, Never>?
    @ObservationIgnored private var authorizationTask: Task<Void, Never>?
    @ObservationIgnored private var authorizationObservationTask: Task<Void, Never>?
    @ObservationIgnored private var authorizationObservationID: UUID?
    @ObservationIgnored private var authorizationObservationRefreshNeeded = false
    @ObservationIgnored private var observerTask: Task<Void, Never>?
    @ObservationIgnored private var observerID: UUID?
    private static let maximumRecords = 64

    init(directory: URL) {
        self.directory = directory
        let authorization = NotificationAuthorizationService()
        self.authorization = authorization
        notificationActions = DeviceNotificationActions(authorization: authorization)
    }

    var isForeground: Bool {
        !isClosed && surfaceVisible && applicationActive && NSApplication.shared.isActive
    }

    var hasDraftChanges: Bool { draft?.isModified == true }
    var savedAppearance: DeviceAppearance? { appearanceSnapshot?.appearance }
    var hasAppearanceDraftChanges: Bool { appearanceDraft?.isModified == true }

    var hasRevisionConflict: Bool {
        guard let draft else { return false }
        return draft.isModified && draft.baseline.revision != latestKnownRevision
    }

    var hasAppearanceRevisionConflict: Bool {
        guard let appearanceDraft else { return false }
        return appearanceDraft.isModified && appearanceDraft.baseline.revision != latestKnownRevision
    }

    var isSnapshotStale: Bool {
        guard let snapshot else { return true }
        return snapshotReadFailed || !isForeground || snapshot.revision < latestKnownRevision
    }

    var isAppearanceSnapshotStale: Bool {
        guard let appearanceSnapshot else { return true }
        return appearanceReadFailed || !applicationActive || appearanceSnapshot.revision < latestKnownRevision
    }

    var canEdit: Bool {
        isForeground && snapshot != nil && !hasUnresolvedPreferenceWrite
            && !snapshotReadFailed && !isOpening && !preferenceOperationPending && !isRefreshing
    }

    var canSave: Bool {
        canEdit && hasDraftChanges && !hasRevisionConflict && !snapshotReadFailed
            && expectedReadback == nil && !isAuthorizing && !isRefreshing
            && retainedRecordCount < Self.maximumRecords
            && draft.map { $0.baseline.revision < UInt64(Int64.max) } == true
    }

    var canReload: Bool {
        !isClosed && pendingPatch == nil && pendingAppearancePatch == nil
            && !isOpening && !preferenceOperationPending && !isRefreshing
    }

    var canInspect: Bool { !isClosed && pendingPatch != nil && !preferenceOperationPending }

    var canEditAppearance: Bool {
        isForeground && appearanceSnapshot != nil && !hasUnresolvedPreferenceWrite
            && !appearanceReadFailed && !isOpening && !preferenceOperationPending && !isRefreshing
    }

    var canSaveAppearance: Bool {
        canEditAppearance && hasAppearanceDraftChanges && !hasAppearanceRevisionConflict
            && !isAuthorizing && retainedRecordCount < Self.maximumRecords
            && appearanceDraft.map { $0.baseline.revision < UInt64(Int64.max) } == true
    }

    var canReloadAppearance: Bool { canReload }

    var canInspectAppearance: Bool {
        !isClosed && pendingAppearancePatch != nil && !preferenceOperationPending
    }

    var canAuthorize: Bool {
        isForeground && preferences != nil && foregroundSession != nil && snapshot != nil && !snapshotReadFailed
            && !hasUnresolvedPreferenceWrite && !isOpening && !preferenceOperationPending
            && !isAuthorizing && !isRefreshing && retainedRecordCount < Self.maximumRecords
    }

    var capacityReached: Bool { retainedRecordCount >= Self.maximumRecords }

    var preferenceStatus: String? {
        if isSaving { return "Saving preferences on this Mac…" }
        if isInspecting { return "Inspecting the original save…" }
        if pendingPatch != nil { return "This save has no confirmed outcome. Inspect the original save before making another change." }
        if hasRevisionConflict { return "Saved preferences changed while you were editing. Reload to review the current values." }
        guard let result = lastPreferenceResult else { return nil }
        switch result.outcome {
        case .applied: return expectedReadback == nil ? "The original save applied on this Mac." : "The original save applied. Refresh its saved values before editing again."
        case .conflict: return "The original save conflicted with newer preferences. Your draft has been kept."
        }
    }

    var appearanceStatus: String? {
        if isSavingAppearance { return "Saving appearance on this Mac…" }
        if isInspectingAppearance { return "Inspecting the original appearance save…" }
        if pendingAppearancePatch != nil { return "This appearance save has no confirmed outcome. Inspect the original save before making another change." }
        if hasAppearanceRevisionConflict { return "Saved preferences changed while you were editing appearance. Reload to review the current value." }
        guard let result = lastAppearanceResult else { return nil }
        switch result.outcome {
        case .applied: return expectedAppearanceReadback == nil ? "The original appearance save applied on this Mac." : "The original appearance save applied. Refresh its saved value before editing again."
        case .conflict: return "The original appearance save conflicted with newer preferences."
        }
    }

    var notificationStatus: String? {
        if isAuthorizing { return "Waiting for the notification request to finish…" }
        guard let result = lastNotificationResult else { return nil }
        switch result.outcome {
        case .completed: return "Notification settings were observed after the interaction."
        case .conflict: return "Preferences changed before notification admission. Review the saved values before trying again."
        case .cancelled: return "The notification action was cancelled before an interaction was admitted."
        case .expired: return "The notification action expired. A new request requires another foreground gesture."
        case .failed: return "The notification interaction failed. Any available observed status is shown."
        case .unavailable: return "Notification settings are unavailable."
        case .capacity: return "The notification service has reached its retained action limit."
        case .requestReused: return "The notification request does not match its retained action."
        }
    }

    func open() {
        guard !isClosed, preferences == nil, openingTask == nil else { return }
        isOpening = true
        let directory = directory
        openingTask = Task { [self] in
            let opened = await Task.detached(priority: .userInitiated) {
                do { return Result<DevicePreferenceStore, DevicePreferenceError>.success(try DevicePreferenceStore.local(directory: directory)) }
                catch { return .failure(error as? DevicePreferenceError ?? .storage) }
            }.value
            switch opened {
            case .success(let store):
                preferences = store
                await readSnapshots(from: store)
            case .failure(let error):
                preferenceError = error
                appearanceError = error
                snapshotReadFailed = true
                appearanceReadFailed = true
            }
            isOpening = false
            openingTask = nil
            updateObserver()
        }
    }

    func setSurfaceVisible(_ visible: Bool) {
        guard !isClosed else { return }
        let changed = surfaceVisible != visible
        surfaceVisible = visible
        if !isForeground { invalidateAuthorizationObservation() }
        updateForegroundSession()
        updateObserver()
        if visible && changed { refresh() }
    }

    func setApplicationActive(_ active: Bool) {
        guard !isClosed else { return }
        let changed = applicationActive != active
        applicationActive = active
        if !isForeground { invalidateAuthorizationObservation() }
        updateForegroundSession()
        updateObserver()
        if active && changed { refresh() }
    }

    func setCompletion(_ value: Bool) {
        guard canEdit else { return }
        draft?.completion = value
    }

    func setAttention(_ value: Bool) {
        guard canEdit else { return }
        draft?.attention = value
    }

    func setAppearance(_ value: DeviceAppearance) {
        guard canEditAppearance else { return }
        appearanceDraft?.appearance = value
    }

    func refresh() {
        guard !isClosed else { return }
        if let preferences { startRefresh(preferences, discardDraft: false) }
        else { open() }
        if isForeground { observeAuthorization() }
    }

    func reload() {
        guard canReload else { return }
        guard let preferences else { open(); return }
        startRefresh(preferences, discardDraft: true)
        if isForeground { observeAuthorization() }
    }

    func reloadAppearance() {
        guard canReloadAppearance else { return }
        guard let preferences else { open(); return }
        startRefresh(preferences, discardDraft: false, discardAppearanceDraft: true)
    }

    func save() {
        guard canSave, let draft, let preferences else { return }
        var changes: [DevicePreferenceChange] = []
        if draft.completion != draft.baseline.completion { changes.append(DevicePreferenceChange(key: .completion, value: draft.completion)) }
        if draft.attention != draft.baseline.attention { changes.append(DevicePreferenceChange(key: .attention, value: draft.attention)) }
        let patch = DevicePreferencePatch(requestID: UUID(), deviceID: draft.baseline.deviceID, expectedRevision: draft.baseline.revision, changes: changes)
        do { try patch.validate() }
        catch { preferenceError = .invalidRequest; return }
        preferenceRecords[patch.requestID] = PreferenceRecord(patch: patch, baseline: draft.baseline, result: nil)
        pendingPatch = patch
        isSaving = true
        preferenceError = nil
        writeTask = Task { [self, preferences, patch] in
            do { receive(try await preferences.apply(patch), for: patch) }
            catch { preferenceError = error as? DevicePreferenceError ?? .storage }
            await readSnapshots(from: preferences)
            isSaving = false
            writeTask = nil
        }
    }

    func inspectPendingSave() {
        guard canInspect, let patch = pendingPatch, let preferences else { return }
        isInspecting = true
        inspectionTask = Task { [self, preferences, patch] in
            do {
                if let result = try await preferences.result(for: patch.requestID) { receive(result, for: patch) }
            } catch { preferenceError = error as? DevicePreferenceError ?? .storage }
            await readSnapshots(from: preferences)
            isInspecting = false
            inspectionTask = nil
        }
    }

    func saveAppearance() {
        guard canSaveAppearance, let draft = appearanceDraft, let preferences else { return }
        let patch = DeviceAppearancePatch(requestID: UUID(), deviceID: draft.baseline.deviceID, expectedRevision: draft.baseline.revision, appearance: draft.appearance)
        do { try patch.validate() }
        catch { appearanceError = .invalidRequest; return }
        appearanceRecords[patch.requestID] = AppearanceRecord(patch: patch, result: nil)
        pendingAppearancePatch = patch
        isSavingAppearance = true
        appearanceError = nil
        writeTask = Task { [self, preferences, patch] in
            do { receive(try await preferences.applyAppearance(patch), for: patch) }
            catch { appearanceError = error as? DevicePreferenceError ?? .storage }
            await readSnapshots(from: preferences)
            isSavingAppearance = false
            writeTask = nil
        }
    }

    func inspectAppearanceSave() {
        guard canInspectAppearance, let patch = pendingAppearancePatch, let preferences else { return }
        isInspectingAppearance = true
        inspectionTask = Task { [self, preferences, patch] in
            do {
                if let result = try await preferences.appearanceResult(for: patch.requestID) { receive(result, for: patch) }
            } catch { appearanceError = error as? DevicePreferenceError ?? .storage }
            await readSnapshots(from: preferences)
            isInspectingAppearance = false
            inspectionTask = nil
        }
    }

    func requestNotificationAuthorization() {
        guard canAuthorize, let snapshot, let preferences, let session = foregroundSession else {
            notificationIssue = .inactive
            return
        }
        let requestID = UUID()
        let admission: DeviceNotificationActions.Admission
        do {
            admission = try notificationActions.prepare(requestID: requestID, expectedRevision: snapshot.revision, expiresAt: Date().addingTimeInterval(60), session: session, preferences: preferences)
        } catch let error as DeviceNotificationActions.PreparationError {
            switch error {
            case .capacity: notificationIssue = .capacity
            case .requestReused: notificationIssue = .invalidRequest
            }
            return
        } catch {
            notificationIssue = .invalidRequest
            return
        }
        notificationRecords[requestID] = NotificationRecord(admission: admission, result: nil)
        notificationIssue = nil
        notificationGuidance = .none
        isAuthorizing = true
        authorizationTask = Task { [self, admission, preferences] in
            let result = await notificationActions.authorize(admission)
            invalidateAuthorizationObservation()
            notificationRecords[requestID]?.result = result
            lastNotificationResult = result
            switch result.outcome {
            case .completed(_, let observation, let guidance):
                receive(observation)
                notificationGuidance = guidance
            case .failed(let observation):
                if let observation { receive(observation) }
            case .unavailable(let observation): receive(observation)
            default: break
            }
            if isForeground { observeAuthorization() }
            await readSnapshots(from: preferences)
            isAuthorizing = false
            authorizationTask = nil
        }
    }

    func invalidate() {
        guard !isClosed else { return }
        isClosed = true
        surfaceVisible = false
        applicationActive = false
        foregroundSession?.close()
        foregroundSession = nil
        invalidateAuthorizationObservation()
        observerTask?.cancel()
        for (requestID, record) in notificationRecords where record.result == nil { notificationActions.cancel(requestID: requestID) }
    }

    func drain() async {
        invalidate()
        await drainSurface()
    }

    func drainSurface() async {
        let cancelledObserver = observerTask?.isCancelled == true ? observerTask : nil
        let tasks = [openingTask, cancelledObserver, refreshTask, snapshotReadTask?.task, writeTask, inspectionTask, authorizationTask, authorizationObservationTask].compactMap { $0 }
        for task in tasks { await task.value }
    }

    private var retainedRecordCount: Int { preferenceRecords.count + appearanceRecords.count + notificationRecords.count }

    private var preferenceOperationPending: Bool {
        isSaving || isInspecting || isSavingAppearance || isInspectingAppearance
    }

    private var hasUnresolvedPreferenceWrite: Bool {
        pendingPatch != nil || pendingAppearancePatch != nil || expectedReadback != nil || expectedAppearanceReadback != nil
    }

    private var latestKnownRevision: UInt64 {
        var known = max(max(snapshot?.revision ?? 0, appearanceSnapshot?.revision ?? 0), max(revision(of: lastPreferenceResult), revision(of: lastAppearanceResult)))
        for record in notificationRecords.values {
            if let result = record.result, case .conflict(let observedRevision) = result.outcome {
                known = max(known, observedRevision)
            }
        }
        return known
    }

    private func revision(of result: DevicePreferenceResult?) -> UInt64 {
        guard let result else { return 0 }
        switch result.outcome {
        case .applied(let revision): return revision
        case .conflict(let observedRevision): return observedRevision
        }
    }

    private func startRefresh(_ preferences: DevicePreferenceStore, discardDraft: Bool, discardAppearanceDraft: Bool = false) {
        guard refreshTask == nil else { return }
        isRefreshing = true
        refreshTask = Task { [self, preferences] in
            await readSnapshots(from: preferences, discardDraft: discardDraft, discardAppearanceDraft: discardAppearanceDraft)
            isRefreshing = false
            refreshTask = nil
        }
    }

    private func readSnapshots(from preferences: DevicePreferenceStore, discardDraft: Bool = false, discardAppearanceDraft: Bool = false) async {
        let task: Task<Void, Never>
        if let reading = snapshotReadTask {
            task = reading.task
        } else {
            let id = UUID()
            let reading = Task { [self, preferences] in
                await loadSnapshots(from: preferences)
                if snapshotReadTask?.id == id { snapshotReadTask = nil }
            }
            snapshotReadTask = (id, reading)
            task = reading
        }
        await task.value
        if discardDraft, !snapshotReadFailed, pendingPatch == nil, expectedReadback == nil, let snapshot {
            draft = Draft(snapshot: snapshot)
        }
        if discardAppearanceDraft, !appearanceReadFailed, pendingAppearancePatch == nil, expectedAppearanceReadback == nil, let appearanceSnapshot {
            appearanceDraft = AppearanceDraft(snapshot: appearanceSnapshot)
        }
    }

    private func loadSnapshots(from preferences: DevicePreferenceStore) async {
        do { receive(try await preferences.snapshot(), discardDraft: false) }
        catch {
            preferenceError = error as? DevicePreferenceError ?? .storage
            snapshotReadFailed = true
        }
        do { receive(try await preferences.appearanceSnapshot(), discardDraft: false) }
        catch {
            appearanceError = error as? DevicePreferenceError ?? .storage
            appearanceReadFailed = true
        }
    }

    private func receive(_ incoming: DevicePreferenceSnapshot, discardDraft: Bool) {
        if let appearanceSnapshot, incoming.deviceID != appearanceSnapshot.deviceID {
            preferenceError = .wrongDevice
            snapshotReadFailed = true
            return
        }
        if let snapshot {
            guard incoming.deviceID == snapshot.deviceID else { preferenceError = .wrongDevice; snapshotReadFailed = true; return }
            guard incoming.revision >= snapshot.revision else { return }
            guard incoming.revision != snapshot.revision || incoming == snapshot else { preferenceError = .storage; snapshotReadFailed = true; return }
        }
        snapshot = incoming
        snapshotReadFailed = false
        if let expectedReadback {
            guard incoming.revision >= expectedReadback.revision else { snapshotReadFailed = true; return }
            if incoming.revision == expectedReadback.revision {
                guard incoming == expectedReadback else { preferenceError = .storage; snapshotReadFailed = true; return }
                draft = Draft(snapshot: incoming)
            }
            self.expectedReadback = nil
        }
        if pendingPatch == nil && (discardDraft || draft == nil || draft?.isModified == false) { draft = Draft(snapshot: incoming) }
        if pendingPatch == nil { preferenceError = nil }
        updateForegroundSession()
    }

    private func receive(_ incoming: DeviceAppearanceSnapshot, discardDraft: Bool) {
        if let snapshot, incoming.deviceID != snapshot.deviceID {
            appearanceError = .wrongDevice
            appearanceReadFailed = true
            return
        }
        if let appearanceSnapshot {
            guard incoming.deviceID == appearanceSnapshot.deviceID else { appearanceError = .wrongDevice; appearanceReadFailed = true; return }
            guard incoming.revision >= appearanceSnapshot.revision else { return }
            guard incoming.revision != appearanceSnapshot.revision || incoming == appearanceSnapshot else { appearanceError = .storage; appearanceReadFailed = true; return }
        }
        if let expectedAppearanceReadback {
            guard incoming.revision >= expectedAppearanceReadback.revision else { appearanceReadFailed = true; return }
            if incoming.revision == expectedAppearanceReadback.revision {
                guard incoming == expectedAppearanceReadback else { appearanceError = .storage; appearanceReadFailed = true; return }
                appearanceDraft = AppearanceDraft(snapshot: incoming)
            }
            self.expectedAppearanceReadback = nil
        }
        appearanceSnapshot = incoming
        appearanceReadFailed = false
        if pendingAppearancePatch == nil && (discardDraft || appearanceDraft == nil || appearanceDraft?.isModified == false) {
            appearanceDraft = AppearanceDraft(snapshot: incoming)
        }
        if pendingAppearancePatch == nil { appearanceError = nil }
    }

    private func receive(_ result: DevicePreferenceResult, for patch: DevicePreferencePatch) {
        guard result.requestID == patch.requestID, result.deviceID == patch.deviceID,
              let record = preferenceRecords[patch.requestID], record.patch == patch else {
            preferenceError = .requestReused
            return
        }
        switch result.outcome {
        case .applied(let revision):
            guard revision == patch.expectedRevision + 1, revision <= UInt64(Int64.max) else { preferenceError = .storage; return }
            var completion = record.baseline.completion
            var attention = record.baseline.attention
            for change in patch.changes {
                switch change.key {
                case .completion: completion = change.value
                case .attention: attention = change.value
                }
            }
            expectedReadback = DevicePreferenceSnapshot(deviceID: patch.deviceID, revision: revision, completion: completion, attention: attention)
        case .conflict(let observedRevision):
            guard observedRevision > 0, observedRevision <= UInt64(Int64.max), observedRevision != patch.expectedRevision else { preferenceError = .storage; return }
        }
        preferenceRecords[patch.requestID]?.result = result
        lastPreferenceResult = result
        pendingPatch = nil
        preferenceError = nil
    }

    private func receive(_ result: DevicePreferenceResult, for patch: DeviceAppearancePatch) {
        guard result.requestID == patch.requestID, result.deviceID == patch.deviceID,
              let record = appearanceRecords[patch.requestID], record.patch == patch else {
            appearanceError = .requestReused
            return
        }
        switch result.outcome {
        case .applied(let revision):
            guard revision == patch.expectedRevision + 1, revision <= UInt64(Int64.max) else { appearanceError = .storage; return }
            expectedAppearanceReadback = DeviceAppearanceSnapshot(deviceID: patch.deviceID, revision: revision, appearance: patch.appearance)
        case .conflict(let observedRevision):
            guard observedRevision > 0, observedRevision <= UInt64(Int64.max), observedRevision != patch.expectedRevision else { appearanceError = .storage; return }
        }
        appearanceRecords[patch.requestID]?.result = result
        lastAppearanceResult = result
        pendingAppearancePatch = nil
        appearanceError = nil
    }

    private func updateForegroundSession() {
        guard isForeground, let snapshot else {
            foregroundSession?.setForeground(false)
            for (requestID, record) in notificationRecords where record.result == nil { notificationActions.cancel(requestID: requestID) }
            return
        }
        if foregroundSession == nil {
            do { foregroundSession = try DeviceForegroundSession(deviceID: snapshot.deviceID) }
            catch { notificationIssue = .invalidRequest; return }
        }
        foregroundSession?.setForeground(true)
    }

    private func observeAuthorization() {
        guard isForeground else { return }
        guard authorizationObservationTask == nil else {
            authorizationObservationID = nil
            authorizationObservationRefreshNeeded = true
            return
        }
        let id = UUID()
        authorizationObservationID = id
        authorizationObservationRefreshNeeded = false
        isObservingAuthorization = true
        authorizationObservationTask = Task { [self] in
            let observation = await authorization.observe()
            if isForeground && authorizationObservationID == id { receive(observation) }
            isObservingAuthorization = false
            authorizationObservationTask = nil
            authorizationObservationID = nil
            let refreshNeeded = authorizationObservationRefreshNeeded
            authorizationObservationRefreshNeeded = false
            if refreshNeeded { observeAuthorization() }
        }
    }

    private func receive(_ observation: DeviceNotificationObservation) {
        notificationObservation = observation
    }

    private func invalidateAuthorizationObservation() {
        authorizationObservationID = nil
        authorizationObservationRefreshNeeded = false
    }

    private func updateObserver() {
        guard isForeground, preferences != nil else { observerTask?.cancel(); return }
        guard observerTask == nil else { return }
        let id = UUID()
        observerID = id
        observerTask = Task { [weak self] in
            while !Task.isCancelled {
                do { try await Task.sleep(for: .seconds(2)) }
                catch { break }
                guard let self, self.isForeground, let preferences = self.preferences else { break }
                await self.readSnapshots(from: preferences)
            }
            guard let self, self.observerID == id else { return }
            self.observerTask = nil
            self.observerID = nil
            self.updateObserver()
        }
    }
}

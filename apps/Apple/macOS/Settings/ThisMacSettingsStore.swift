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

    private(set) var snapshot: DevicePreferenceSnapshot?
    private(set) var draft: Draft?
    private(set) var pendingPatch: DevicePreferencePatch?
    private(set) var lastPreferenceResult: DevicePreferenceResult?
    private(set) var preferenceError: DevicePreferenceError?
    private(set) var notificationObservation: DeviceNotificationObservation?
    private(set) var lastNotificationResult: DeviceNotificationActionResult?
    private(set) var notificationIssue: NotificationIssue?
    private(set) var notificationGuidance: DeviceNotificationGuidance = .none
    private(set) var isOpening = false
    private(set) var isRefreshing = false
    private(set) var isSaving = false
    private(set) var isInspecting = false
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
    private var expectedReadback: DevicePreferenceSnapshot?
    private var preferenceRecords: [UUID: PreferenceRecord] = [:]
    private var notificationRecords: [UUID: NotificationRecord] = [:]
    @ObservationIgnored private var openingTask: Task<Void, Never>?
    @ObservationIgnored private var refreshTask: Task<Void, Never>?
    @ObservationIgnored private var writeTask: Task<Void, Never>?
    @ObservationIgnored private var inspectionTask: Task<Void, Never>?
    @ObservationIgnored private var authorizationTask: Task<Void, Never>?
    @ObservationIgnored private var authorizationObservationTask: Task<Void, Never>?
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

    var hasRevisionConflict: Bool {
        guard let draft, let snapshot else { return false }
        return draft.isModified && draft.baseline.revision != snapshot.revision
    }

    var isSnapshotStale: Bool { snapshotReadFailed || !isForeground }

    var canEdit: Bool {
        isForeground && snapshot != nil && pendingPatch == nil && expectedReadback == nil
            && !snapshotReadFailed && !isSaving && !isInspecting && !isRefreshing
    }

    var canSave: Bool {
        canEdit && hasDraftChanges && !hasRevisionConflict && !snapshotReadFailed
            && expectedReadback == nil && !isAuthorizing && !isRefreshing
            && retainedRecordCount < Self.maximumRecords
            && draft.map { $0.baseline.revision < UInt64(Int64.max) } == true
    }

    var canReload: Bool {
        !isClosed && pendingPatch == nil && !isSaving && !isInspecting && !isRefreshing
    }

    var canInspect: Bool { !isClosed && pendingPatch != nil && !isSaving && !isInspecting }

    var canAuthorize: Bool {
        isForeground && preferences != nil && foregroundSession != nil && snapshot != nil && !snapshotReadFailed
            && expectedReadback == nil && pendingPatch == nil && !isSaving && !isInspecting
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
                await readSnapshot(from: store)
                updateForegroundSession()
            case .failure(let error):
                preferenceError = error
                snapshotReadFailed = true
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
        updateForegroundSession()
        updateObserver()
        if visible && changed { open(); refresh(); observeAuthorization() }
    }

    func setApplicationActive(_ active: Bool) {
        guard !isClosed else { return }
        let changed = applicationActive != active
        applicationActive = active
        updateForegroundSession()
        updateObserver()
        if active && changed && surfaceVisible { open(); refresh(); observeAuthorization() }
    }

    func setCompletion(_ value: Bool) {
        guard canEdit else { return }
        draft?.completion = value
    }

    func setAttention(_ value: Bool) {
        guard canEdit else { return }
        draft?.attention = value
    }

    func refresh() {
        guard !isClosed else { return }
        guard let preferences else { open(); return }
        startRefresh(preferences, discardDraft: false)
        if isForeground { observeAuthorization() }
    }

    func reload() {
        guard canReload else { return }
        guard let preferences else { open(); return }
        startRefresh(preferences, discardDraft: true)
        if isForeground { observeAuthorization() }
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
            await readSnapshot(from: preferences)
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
            await readSnapshot(from: preferences)
            isInspecting = false
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
            await readSnapshot(from: preferences)
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
        observerTask?.cancel()
        for (requestID, record) in notificationRecords where record.result == nil { notificationActions.cancel(requestID: requestID) }
    }

    func drain() async {
        invalidate()
        await drainSurface()
    }

    func drainSurface() async {
        let cancelledObserver = observerTask?.isCancelled == true ? observerTask : nil
        let tasks = [openingTask, cancelledObserver, refreshTask, writeTask, inspectionTask, authorizationTask, authorizationObservationTask].compactMap { $0 }
        for task in tasks { await task.value }
    }

    private var retainedRecordCount: Int { preferenceRecords.count + notificationRecords.count }

    private func startRefresh(_ preferences: DevicePreferenceStore, discardDraft: Bool) {
        guard refreshTask == nil else { return }
        isRefreshing = true
        refreshTask = Task { [self, preferences] in
            await readSnapshot(from: preferences, discardDraft: discardDraft)
            isRefreshing = false
            refreshTask = nil
        }
    }

    private func readSnapshot(from preferences: DevicePreferenceStore, discardDraft: Bool = false) async {
        do { receive(try await preferences.snapshot(), discardDraft: discardDraft) }
        catch {
            preferenceError = error as? DevicePreferenceError ?? .storage
            snapshotReadFailed = true
        }
    }

    private func receive(_ incoming: DevicePreferenceSnapshot, discardDraft: Bool) {
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
        guard isForeground, authorizationObservationTask == nil else { return }
        isObservingAuthorization = true
        authorizationObservationTask = Task { [self] in
            let observation = await authorization.observe()
            if isForeground { receive(observation) }
            isObservingAuthorization = false
            authorizationObservationTask = nil
        }
    }

    private func receive(_ observation: DeviceNotificationObservation) {
        if let previous = notificationObservation, previous.observedAt > observation.observedAt { return }
        notificationObservation = observation
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
                await self.readSnapshot(from: preferences)
            }
            guard let self, self.observerID == id else { return }
            self.observerTask = nil
            self.observerID = nil
            self.updateObserver()
        }
    }
}

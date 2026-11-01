import Foundation
import Observation
import UIKit

enum ClientTab: Hashable {
    case chat, approvals, settings
}

enum ClientSettingsDestination: Hashable {
    case general, devices, host, privacy, about
    case hostAssistant
}

enum HostSettingsStep: Equatable {
    case access, snapshot, changes, update, open, send, poll, close
    case hostAction(RemoteSettingsHostAction)
}

struct HostSettingsValues {
    let snapshot: SettingsSnapshot
    let observedAt: UInt64
    let scope: ReplicaSettingsScopeToken
}

struct HostSettingsMutation {
    enum State {
        case sending
        case uncertain
        case recorded(RemoteSettingsOperation)
        case rejected(RemoteSettingsDenial)
    }

    let requestID: UUID
    let expectedRevision: UInt64
    let changes: [SettingChange]
    var state: State
    var attempts = 0

    var isOpen: Bool {
        switch state {
        case .sending, .uncertain: true
        case .recorded(let operation):
            if case .finished = operation.phase { false } else { true }
        case .rejected: false
        }
    }
}

struct HostSettingsTurn: Identifiable {
    let id: UUID
    let text: String
    var accepted: Bool
}

struct HostSettingsAssistant {
    var openRequestID: UUID?
    var chat: RemoteSettingsChatOpened?
    var turns: [HostSettingsTurn] = []
    var status: RemoteSettingsChatStatus?
    var textRevision: UInt64?
    var changesFloor: UInt64?
    var text = Data()
    var pollAfterDelay = false
    var ended: String?

    var assistantText: String { String(decoding: text, as: UTF8.self) }
    var pendingTurn: HostSettingsTurn? { turns.last.flatMap { $0.accepted ? nil : $0 } }
    var isRunning: Bool {
        pendingTurn != nil || status?.phase == .running
            || (turns.last.map { $0.accepted && status?.turnID != $0.id } ?? false)
    }
}

enum LocalSettingsCover: Hashable {
    case rootSheet, globalError, deviceDisable, localConfirmation
}

enum PairingPhase: Equatable {
    case idle, connecting
    case waiting(PairingAttempt)
    case paired(String)
    case failed(String)
}

@MainActor
@Observable
final class AppModel {
    var state: ReplicaViewState?
    var events: [WireEvent] = []
    var selectedConversation: UUID?
    var busy = false
    var error: String?
    var connectionEnabled = false
    private(set) var localSettings: ThisDeviceSettingsStore?
    private(set) var localSettingsIssue: String?
    private(set) var notificationDeliveryStatus: String?
    private(set) var hostSettingsAccess: ReplicaSettingsAccessDisplayView?
    private(set) var hostSettingsAccessFresh = false
    private(set) var hostSettingsAccessStatus: ReplicaSettingsAccessRequestStatus?
    private(set) var hostSettingsAccessIssue: String?
    private(set) var hostSettingsAccessBusy = false
    private(set) var hostSettingsBinding: ReplicaSettingsBindingView?
    private(set) var hostSettingsBindingFresh = false
    private(set) var hostSettingsSnapshot: ReplicaSettingsSnapshotView?
    private(set) var hostSettingsInspection: ReplicaSettingsInspectionView?
    private(set) var hostSettingsSnapshotFresh = false
    private(set) var hostSettingsInspectionFresh = false
    private(set) var hostSettingsSnapshotStatus: ReplicaSettingsAccessRequestStatus?
    private(set) var hostSettingsInspectionStatus: ReplicaSettingsAccessRequestStatus?
    private(set) var hostValues: HostSettingsValues?
    private(set) var hostChanges: [SettingsChangeHint] = []
    private(set) var hostMutation: HostSettingsMutation?
    private(set) var hostActionResult: RemoteSettingsHostAction?
    private(set) var hostSettingsStep: HostSettingsStep?
    private(set) var assistant = HostSettingsAssistant()
    private(set) var selectedTab: ClientTab = .chat
    private(set) var settingsPath: [ClientSettingsDestination] = []
    private(set) var pairingPhase: PairingPhase = .idle
    var privacyCoverEnabled: Bool { localSettings?.savedPrivacyCover ?? false }
    var pairingLink: String?
    @ObservationIgnored private var pairingTask: Task<Void, Never>?
    @ObservationIgnored private var service: ReplicaService?
    @ObservationIgnored private var opening: (id: UUID, task: Task<ReplicaService, Error>)?
    @ObservationIgnored private let notifications = NotificationService()
    @ObservationIgnored private var refreshEpoch = UUID()
    @ObservationIgnored private var localDeviceID: UUID?
    @ObservationIgnored private var localIdentityInvalid = false
    @ObservationIgnored private var localSceneActive = false
    @ObservationIgnored private var platformObserved = false
    @ObservationIgnored private var platformActive = false
    @ObservationIgnored private var localDestinationVisible = false
    @ObservationIgnored private var localCovers: Set<LocalSettingsCover> = []
    @ObservationIgnored private var localDrainTask: Task<Void, Never>?
    @ObservationIgnored private var localDrainPending = false
    @ObservationIgnored private var localPermanentDrain = false
    @ObservationIgnored private var notificationGeneration = UUID()
    @ObservationIgnored private var notificationDeliveryTask: Task<Void, Never>?
    @ObservationIgnored private var notificationDeliveryPending = false
    @ObservationIgnored private var accessInteraction: SettingsAccessInteraction?
    @ObservationIgnored private var accessControlTask: Task<Void, Never>?
    @ObservationIgnored private var accessSurfaceVisible = false
    @ObservationIgnored private var assistantSurfaceVisible = false
    @ObservationIgnored private var accessSurfaceEpoch = UUID()
    @ObservationIgnored private var hostSettingsQueue: [HostSettingsStep] = []
    @ObservationIgnored private var hostSettingsTask: Task<Void, Never>?
    @ObservationIgnored private var hostChangesCursor: UInt64 = 0
    @ObservationIgnored private var hostChangesPages = 0
    @ObservationIgnored private var hostAccessRetried = false
    @ObservationIgnored private var hostAutoRefreshScope: ReplicaSettingsScopeToken?

    private enum SettingsRequestInput {
        case access(ReplicaSettingsAccessInput)
        case bind(ReplicaSettingsBindInput)
        case snapshot(ReplicaSettingsSnapshotInput)
        case inspection(ReplicaSettingsInspectionInput)
    }

    private final class SettingsAccessInteraction {
        let service: ReplicaService
        let host: DevicePublicKeys
        let localID: UUID
        let scope: ReplicaSettingsScopeToken
        let epoch: UUID
        let started: ContinuousClock.Instant
        let wallFloor: UInt64
        let deadline: ContinuousClock.Instant
        var handle: ReplicaSettingsRequestHandle?
        var bindingInput: ReplicaSettingsBindInput?
        var bindingHandle: ReplicaSettingsRequestHandle?
        var snapshotInput: ReplicaSettingsSnapshotInput?
        var inspectionInput: ReplicaSettingsInspectionInput?
        var snapshotHandle: ReplicaSettingsRequestHandle?
        var inspectionHandle: ReplicaSettingsRequestHandle?
        var retired = false

        init(service: ReplicaService, host: DevicePublicKeys, localID: UUID,
             scope: ReplicaSettingsScopeToken, epoch: UUID, started: ContinuousClock.Instant, wallFloor: UInt64) {
            self.service = service
            self.host = host
            self.localID = localID
            self.scope = scope
            self.epoch = epoch
            self.started = started
            self.wallFloor = wallFloor
            deadline = started.advanced(by: .seconds(30))
        }
    }

    func scene(active: Bool) async {
        if !active {
            retireHostSettingsAccess()
            retireNotificationDelivery()
            let delivery = notificationDeliveryTask
            let access = accessControlTask
            let hostTask = hostSettingsTask
            await service?.stop()
            if let access { await access.value }
            if let hostTask { await hostTask.value }
            if let delivery { await delivery.value }
            await refresh()
            return
        }
        if service == nil {
            if opening == nil { opening = (UUID(), Task { try await ReplicaService.open() }) }
            guard let opening else { return }
            do {
                service = try await opening.task.value
                if self.opening?.id == opening.id { self.opening = nil }
            } catch {
                if self.opening?.id == opening.id { self.opening = nil }
                present(error)
                return
            }
        }
        guard !Task.isCancelled else { return }
        if connectionEnabled {
            do { try await service?.start() }
            catch is CancellationError { if Task.isCancelled { return } }
            catch { present(error) }
        }
        while !Task.isCancelled {
            await refresh()
            do { try await Task.sleep(for: .seconds(1)) } catch { return }
        }
    }

    func refresh() async {
        guard let service else { return }
        let captured = UUID()
        refreshEpoch = captured
        do {
            let updated = try await service.viewState()
            guard refreshEpoch == captured, !Task.isCancelled else { return }
            if selectedConversation == nil { selectedConversation = updated.snapshot.conversations.first?.id }
            adoptLocalSettings(identity: updated.identity)
            if state?.snapshot.host != updated.snapshot.host || updated.snapshot.revoked {
                retireHostSettingsAccess()
                retireNotificationDelivery()
                if state?.snapshot.host != updated.snapshot.host { forgetHostSettings() }
            } else if state?.settingsScope !== updated.settingsScope {
                retireHostSettingsAccess()
            }
            state = updated
            adoptHostSettingsAccess(updated)
            autoRefreshHostSettings()
            requestNotificationDelivery()
            let conversation = selectedConversation
            await service.selectConversation(conversation)
            let updatedEvents: [WireEvent]
            if let conversation { updatedEvents = try await service.events(conversation: conversation) }
            else { updatedEvents = [] }
            guard refreshEpoch == captured, selectedConversation == conversation, !Task.isCancelled else { return }
            events = updatedEvents
        } catch is CancellationError {
            hostSettingsAccessFresh = false
            hostSettingsBindingFresh = false
            hostSettingsSnapshot = nil
            hostSettingsInspection = nil
            hostSettingsSnapshotFresh = false
            hostSettingsInspectionFresh = false
            return
        } catch {
            hostSettingsAccessFresh = false
            hostSettingsBindingFresh = false
            hostSettingsSnapshot = nil
            hostSettingsInspection = nil
            hostSettingsSnapshotFresh = false
            hostSettingsInspectionFresh = false
            present(error)
        }
    }

    @discardableResult
    func perform(_ operation: @MainActor () async throws -> Void) async -> Bool {
        guard !busy else { return false }
        busy = true
        defer { busy = false }
        do {
            try await operation()
            await refresh()
            return true
        } catch is CancellationError {
            return false
        } catch {
            present(error)
            return false
        }
    }

    func connect() async {
        await perform {
            guard let service else { throw WireError.unavailable }
            try await service.start()
            connectionEnabled = true
        }
    }

    func disconnect() async {
        retireHostSettingsAccess()
        connectionEnabled = false
        retireNotificationDelivery()
        let delivery = notificationDeliveryTask
        let access = accessControlTask
        let hostTask = hostSettingsTask
        await service?.stop()
        if let access { await access.value }
        if let hostTask { await hostTask.value }
        if let delivery { await delivery.value }
        await refresh()
    }

    func saveRelay(_ origin: String) async {
        await perform {
            guard let service else { throw WireError.unavailable }
            try await service.configureRelay(origin)
        }
    }

    func newConversation() async {
        await perform {
            guard let service else { throw WireError.unavailable }
            selectedConversation = try await service.createConversation(title: "New conversation")
        }
    }

    func send(_ text: String) async -> Bool {
        await perform {
            guard let service, let selectedConversation else { throw WireError.unavailable }
            try await service.sendMessage(conversation: selectedConversation, text: text)
        }
    }

    func steer(_ task: ReplicaTask, instruction: String) async -> Bool {
        await perform {
            guard let service else { throw WireError.unavailable }
            try await service.steer(conversation: task.conversationID, task: task.id, instruction: instruction)
        }
    }

    func cancel(_ task: ReplicaTask) async {
        await perform {
            guard let service else { throw WireError.unavailable }
            try await service.cancel(conversation: task.conversationID, task: task.id)
        }
    }

    func resolve(_ approval: ReplicaApproval, decision: String) async {
        await perform {
            guard let service else { throw WireError.unavailable }
            try await service.resolve(approval, decision: decision)
        }
    }

    func synchronize() async {
        await perform {
            guard let service, let selectedConversation else { throw WireError.unavailable }
            try await service.synchronize(selectedConversation)
        }
    }

    func openPairingLink(_ url: URL) {
        pairingLink = url.absoluteString
    }

    func startPairing(_ text: String) {
        pairingTask?.cancel()
        pairingPhase = .connecting
        pairingTask = Task { [weak self] in await self?.runPairing(text) }
    }

    private func runPairing(_ text: String) async {
        do {
            var waited = 0
            while service == nil, waited < 100 {
                try await Task.sleep(for: .milliseconds(100))
                waited += 1
            }
            guard let service else { throw WireError.unavailable }
            let attempt = try await service.preparePairing(text, deviceName: Self.pairingDeviceName)
            guard !Task.isCancelled else { return }
            pairingPhase = .waiting(attempt)
            try await service.completePairing(attempt)
            guard !Task.isCancelled else { return }
            pairingPhase = .paired(attempt.hostName)
            await refresh()
            await connect()
        } catch is CancellationError {
        } catch {
            guard !Task.isCancelled else { return }
            pairingPhase = .failed((error as? LocalizedError)?.errorDescription ?? "Pairing could not be completed.")
        }
    }

    func cancelPairing() async {
        pairingTask?.cancel()
        pairingTask = nil
        await service?.cancelPairing()
        pairingPhase = .idle
    }

    private static var pairingDeviceName: String {
        var name = UIDevice.current.name.unicodeScalars.filter { !CharacterSet.controlCharacters.contains($0) }.map(String.init).joined()
        while name.utf8.count > 64 { name.removeLast() }
        return name.trimmingCharacters(in: .whitespaces).isEmpty ? "iPhone" : name
    }

    func disable() async {
        retireHostSettingsAccess()
        retireNotificationDelivery()
        let delivery = notificationDeliveryTask
        let access = accessControlTask
        let hostTask = hostSettingsTask
        await perform {
            guard let service else { throw WireError.unavailable }
            try await service.disable()
            connectionEnabled = false
        }
        if let access { await access.value }
        if let hostTask { await hostTask.value }
        if let delivery { await delivery.value }
        hostValues = nil
        hostChanges = []
        assistant = HostSettingsAssistant()
    }

    func localSceneChanged(active: Bool) {
        localSceneActive = active
        if !platformObserved {
            platformObserved = true
            platformActive = UIApplication.shared.applicationState == .active
        }
        updateLocalEligibility()
    }

    func localApplicationWillResignActive() {
        platformObserved = true
        platformActive = false
        updateLocalEligibility()
    }

    func localApplicationDidBecomeActive() {
        platformObserved = true
        platformActive = UIApplication.shared.applicationState == .active
        updateLocalEligibility()
    }

    func localTabChanged(_ tab: ClientTab) {
        guard selectedTab != tab else { return }
        localSettings?.setSurfaceVisible(false)
        selectedTab = tab
        updateLocalEligibility()
    }

    func localSettingsPathChanged(_ path: [ClientSettingsDestination]) {
        localSettings?.setSurfaceVisible(false)
        localDestinationVisible = false
        guard path.count <= 1 || path == [.host, .hostAssistant] else {
            localSettingsIssue = "The local settings destination could not be opened."
            requestLocalDrain()
            return
        }
        settingsPath = path
        if path.isEmpty { localCovers.remove(.localConfirmation) }
        updateLocalEligibility()
    }

    func localDestinationChanged(presented: Bool) {
        if !presented { localSettings?.setSurfaceVisible(false) }
        localDestinationVisible = presented && (settingsPath.last == .general || settingsPath.last == .privacy)
        updateLocalEligibility()
    }

    func localCoverChanged(_ cover: LocalSettingsCover, presented: Bool) {
        guard localCovers.contains(cover) != presented else { return }
        if presented {
            localSettings?.setSurfaceVisible(false)
            localCovers.insert(cover)
        } else {
            localCovers.remove(cover)
        }
        updateLocalEligibility()
    }

    func dismissError() {
        error = nil
        localCoverChanged(.globalError, presented: false)
    }

    private func adoptLocalSettings(identity: DevicePublicKeys) {
        if let localDeviceID {
            guard localDeviceID != identity.deviceID, !localIdentityInvalid else { return }
            localIdentityInvalid = true
            retireNotificationDelivery()
            localSettingsIssue = "The local preference identity changed. Existing work has been retained; reopen the app to inspect storage."
            localSettings?.invalidate()
            requestLocalDrain(permanent: true)
            return
        }
        localDeviceID = identity.deviceID
        do {
            guard let support = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first else {
                throw DevicePreferenceError.storage
            }
            let directory = support.appendingPathComponent("Vorra", isDirectory: true).appendingPathComponent("DevicePreferences", isDirectory: true)
            let owner = try ThisDeviceSettingsStore(directory: directory, deviceID: identity.deviceID)
            localSettings = owner
            owner.open()
            updateLocalEligibility()
        } catch {
            localSettingsIssue = "Local preferences could not be initialized. Existing storage has not been replaced."
        }
    }

    private func updateLocalEligibility() {
        if !hostSettingsAccessEligible { retireHostSettingsAccess() }
        if !localSceneActive || !platformActive { retireNotificationDelivery() }
        guard let localSettings else { return }
        let active = localSceneActive && platformActive
        let visible = selectedTab == .settings && localDestinationVisible && localCovers.isEmpty
        if !active || !visible { localSettings.setSurfaceVisible(false) }
        localSettings.setSceneActive(active)
        localSettings.setSurfaceVisible(visible)
        if !active || !visible { requestLocalDrain() }
    }

    var hostSettingsAccessIsCurrent: Bool {
        guard hostSettingsAccessFresh, let original = accessInteraction,
              isCurrentAccessInteraction(original), let display = hostSettingsAccess,
              let now = try? WireID.now() else { return false }
        return original.handle === display.requestHandle && now < display.expiresAt
    }

    var canCheckHostSettingsAccess: Bool {
        hostSettingsAccessEligible && !hostSettingsAccessBusy && hostSettingsTask == nil
            && hostSettingsAccessStatus?.blocksNewRequest != true
            && hostSettingsBinding?.stage.blocksNewRequest != true
            && hostSettingsSnapshotStatus?.blocksNewRequest != true
            && hostSettingsInspectionStatus?.blocksNewRequest != true
    }

    var canBindHostSettings: Bool {
        guard canCheckHostSettingsAccess, hostSettingsAccessIsCurrent, hostSettingsBinding == nil,
              accessInteraction?.bindingInput == nil, let display = hostSettingsAccess,
              case .available = display.display.status else { return false }
        return true
    }

    var hostSettingsBindingIsCurrent: Bool {
        guard hostSettingsBindingFresh, let original = accessInteraction,
              isCurrentSettingsScopeInteraction(original), let input = original.bindingInput,
              let display = hostSettingsBinding, original.bindingHandle === display.requestHandle,
              let expiresAt = display.expiresAt, expiresAt > input.wallFloor,
              expiresAt > original.wallFloor, let wall = try? WireID.now(), wall < expiresAt else { return false }
        let deadline = min(
            input.monotonicFloor.advanced(by: .milliseconds(Int64(expiresAt - input.wallFloor))),
            original.started.advanced(by: .milliseconds(Int64(expiresAt - original.wallFloor)))
        )
        let monotonic = ContinuousClock.now
        return wall >= input.wallFloor && wall >= original.wallFloor
            && monotonic >= input.monotonicFloor && monotonic < deadline
    }

    var canReadHostSettingsSnapshot: Bool {
        guard canCheckHostSettingsAccess, hostSettingsBindingIsCurrent,
              let display = hostSettingsAccess, case .available(let summary) = display.display.status else { return false }
        return !summary.readableKeys.isEmpty
    }

    var canInspectHostSettingsOriginal: Bool {
        canCheckHostSettingsAccess && hostSettingsBindingIsCurrent
    }

    var hostSettingsSnapshotIsCurrent: Bool {
        guard hostSettingsSnapshotFresh, let original = accessInteraction,
              let capture = original.snapshotInput, let display = hostSettingsSnapshot,
              original.snapshotHandle === display.requestHandle,
              isCurrentSnapshotInteraction(original, capture: capture),
              display.expiresAt > capture.wallFloor, display.expiresAt <= capture.expiresAt,
              let wall = try? WireID.now(), wall >= capture.wallFloor, wall < display.expiresAt else { return false }
        let deadline = capture.monotonicFloor.advanced(by: .milliseconds(Int64(display.expiresAt - capture.wallFloor)))
        return ContinuousClock.now < deadline
    }

    var hostSettingsInspectionIsCurrent: Bool {
        guard hostSettingsInspectionFresh, let original = accessInteraction,
              let capture = original.inspectionInput, let display = hostSettingsInspection,
              original.inspectionHandle === display.requestHandle,
              isCurrentInspectionInteraction(original, capture: capture),
              display.expiresAt > capture.wallFloor, display.expiresAt <= capture.expiresAt,
              let wall = try? WireID.now(), wall >= capture.wallFloor, wall < display.expiresAt else { return false }
        let deadline = capture.monotonicFloor.advanced(by: .milliseconds(Int64(display.expiresAt - capture.wallFloor)))
        return ContinuousClock.now < deadline
    }

    private func isCurrentSnapshotInteraction(_ original: SettingsAccessInteraction,
                                               capture: ReplicaSettingsSnapshotInput) -> Bool {
        guard isCurrentSettingsScopeInteraction(original), original.snapshotInput === capture,
              hostSettingsBindingIsCurrent, let wall = try? WireID.now() else { return false }
        let monotonic = ContinuousClock.now
        return wall >= capture.wallFloor && wall < capture.expiresAt
            && monotonic >= capture.monotonicFloor && monotonic < capture.deadline
    }

    private func isCurrentInspectionInteraction(_ original: SettingsAccessInteraction,
                                               capture: ReplicaSettingsInspectionInput) -> Bool {
        guard isCurrentSettingsScopeInteraction(original), original.inspectionInput === capture,
              hostSettingsBindingIsCurrent, let wall = try? WireID.now() else { return false }
        let monotonic = ContinuousClock.now
        return wall >= capture.wallFloor && wall < capture.expiresAt
            && monotonic >= capture.monotonicFloor && monotonic < capture.deadline
    }

    private func isCurrentSettingsRequest(_ input: SettingsRequestInput, original: SettingsAccessInteraction) -> Bool {
        switch input {
        case .access, .bind: isCurrentAccessInteraction(original)
        case .snapshot(let capture): isCurrentSnapshotInteraction(original, capture: capture)
        case .inspection(let capture): isCurrentInspectionInteraction(original, capture: capture)
        }
    }

    private var hostSettingsAccessEligible: Bool {
        (accessSurfaceVisible || assistantSurfaceVisible) && localSceneActive && platformActive
            && UIApplication.shared.applicationState == .active && selectedTab == .settings
            && (settingsPath == [.host] || settingsPath == [.host, .hostAssistant]) && localCovers.isEmpty
            && connectionEnabled && state?.connection == "Connected" && state?.settingsScope != nil
            && state?.snapshot.host != nil && state?.snapshot.revoked == false
    }

    private func isCurrentSettingsScopeInteraction(_ original: SettingsAccessInteraction) -> Bool {
        accessInteraction === original && !original.retired && accessSurfaceEpoch == original.epoch
            && service === original.service && hostSettingsAccessEligible && state?.snapshot.host == original.host
            && state?.identity.deviceID == original.localID && state?.settingsScope === original.scope
    }

    private func isCurrentAccessInteraction(_ original: SettingsAccessInteraction) -> Bool {
        guard isCurrentSettingsScopeInteraction(original), let wall = try? WireID.now() else { return false }
        let now = ContinuousClock.now
        return wall >= original.wallFloor && wall - original.wallFloor < 30_000
            && now >= original.started && now < original.deadline
    }

    func checkHostSettingsAccess() {
        guard canCheckHostSettingsAccess, accessControlTask == nil,
              let service, let state, let host = state.snapshot.host, let scope = state.settingsScope else { return }
        let input: ReplicaSettingsAccessInput
        do {
            let gesture = try ReplicaSettingsAccessGesture(service: service, scope: scope)
            input = try gesture.take()
        } catch {
            hostSettingsAccessIssue = "The access check could not capture its original time."
            return
        }
        let original = SettingsAccessInteraction(service: service, host: host,
                                                 localID: state.identity.deviceID, scope: scope,
                                                 epoch: accessSurfaceEpoch, started: input.monotonicFloor,
                                                 wallFloor: input.wallFloor)
        accessInteraction = original
        startHostSettingsRequest(.access(input), original: original)
    }

    func bindHostSettings() {
        guard canBindHostSettings, accessControlTask == nil, let original = accessInteraction,
              let display = hostSettingsAccess, let handle = original.handle,
              handle === display.requestHandle, isCurrentAccessInteraction(original) else { return }
        let input: ReplicaSettingsBindInput
        do {
            let gesture = try ReplicaSettingsBindGesture(service: original.service, scope: original.scope, accessHandle: handle)
            input = try gesture.take()
        } catch {
            hostSettingsAccessIssue = "The binding request could not capture its original time."
            return
        }
        original.bindingInput = input
        startHostSettingsRequest(.bind(input), original: original)
    }

    func readHostSettingsSnapshot() {
        guard canReadHostSettingsSnapshot, accessControlTask == nil, let original = accessInteraction,
              let binding = hostSettingsBinding, original.bindingHandle === binding.requestHandle,
              isCurrentSettingsScopeInteraction(original) else { return }
        let input: ReplicaSettingsSnapshotInput
        do {
            let gesture = try ReplicaSettingsSnapshotGesture(service: original.service, scope: original.scope,
                                                              bindingHandle: binding.requestHandle)
            input = try gesture.take()
        } catch {
            hostSettingsAccessIssue = "The value read could not capture its original time."
            return
        }
        original.snapshotInput = input
        startHostSettingsRequest(.snapshot(input), original: original)
    }

    func inspectHostSettingsOriginal(requestID: UUID, operationID: UUID?) {
        guard canInspectHostSettingsOriginal, accessControlTask == nil, let original = accessInteraction,
              let binding = hostSettingsBinding, original.bindingHandle === binding.requestHandle,
              isCurrentSettingsScopeInteraction(original) else { return }
        let input: ReplicaSettingsInspectionInput
        do {
            let gesture = try ReplicaSettingsInspectionGesture(service: original.service, scope: original.scope,
                                                              bindingHandle: binding.requestHandle, requestID: requestID, operationID: operationID)
            input = try gesture.take()
        } catch {
            hostSettingsAccessIssue = "The inspection could not capture its original time."
            return
        }
        original.inspectionInput = input
        startHostSettingsRequest(.inspection(input), original: original)
    }

    func cancelHostSettingsSnapshot() {
        retireHostSettingsAccess()
    }

    func cancelHostSettingsInspection() {
        retireHostSettingsAccess()
    }

    private func startHostSettingsRequest(_ input: SettingsRequestInput, original: SettingsAccessInteraction) {
        guard accessControlTask == nil else { return }
        if case .access = input { hostSettingsAccessFresh = false }
        if case .snapshot = input { hostSettingsSnapshotFresh = false }
        if case .inspection = input { hostSettingsInspectionFresh = false }
        hostSettingsAccessIssue = nil
        hostSettingsAccessBusy = true
        accessControlTask = Task { [self, original] in
            defer {
                hostSettingsAccessBusy = false
                accessControlTask = nil
            }
            do {
                guard isCurrentSettingsRequest(input, original: original) else { throw CancellationError() }
                switch input {
                case .access(let capture):
                    original.handle = try await original.service.requestSettingsAccess(capture)
                case .bind(let capture):
                    original.bindingHandle = try await original.service.requestSettingsBind(capture)
                case .snapshot(let capture):
                    original.snapshotHandle = try await original.service.requestSettingsSnapshot(capture)
                case .inspection(let capture):
                    original.inspectionHandle = try await original.service.requestSettingsInspection(capture)
                }
                guard !Task.isCancelled, isCurrentSettingsRequest(input, original: original) else {
                    await cancelAccessInteraction(original)
                    return
                }
                await refresh()
                if Task.isCancelled || original.retired { await cancelAccessInteraction(original) }
            } catch is CancellationError {
                await cancelAccessInteraction(original)
            } catch {
                await cancelAccessInteraction(original)
                if accessInteraction === original {
                    hostSettingsAccessFresh = false
                    hostSettingsBindingFresh = false
                    hostSettingsSnapshotFresh = false
                    hostSettingsInspectionFresh = false
                    switch input {
                    case .access:
                        hostSettingsAccessIssue = "The Host access check could not be started. No Host setting was changed."
                    case .bind:
                        hostSettingsAccessIssue = "The original binding request could not finish. Its Host outcome has not been verified."
                    case .snapshot:
                        hostSettingsAccessIssue = "The original Host value read could not finish. No current values were verified."
                    case .inspection:
                        hostSettingsAccessIssue = "The original inspection could not finish. No current outcome was verified."
                    }
                }
            }
        }
    }

    func cancelHostSettingsAccess() {
        retireHostSettingsAccess()
    }

    func cancelHostSettingsBinding() {
        retireHostSettingsAccess()
    }

    func hostSettingsAccessSurfaceChanged(visible: Bool) {
        guard accessSurfaceVisible != visible else { return }
        accessSurfaceVisible = visible
        if !hostSettingsAccessEligible { retireHostSettingsAccess() }
        if visible { autoRefreshHostSettings() }
    }

    func hostSettingsAssistantSurfaceChanged(visible: Bool) {
        guard assistantSurfaceVisible != visible else { return }
        assistantSurfaceVisible = visible
        if !hostSettingsAccessEligible { retireHostSettingsAccess() }
        if visible, assistant.chat != nil, assistant.ended == nil { enqueueHostSettings(assistant.pendingTurn == nil ? [.poll] : [.send]) }
    }

    private func retireHostSettingsAccess() {
        hostSettingsQueue.removeAll()
        hostSettingsTask?.cancel()
        hostSettingsAccessFresh = false
        hostSettingsBindingFresh = false
        hostSettingsSnapshotFresh = false
        hostSettingsInspectionFresh = false
        guard let original = accessInteraction else { return }
        if !original.retired { accessSurfaceEpoch = UUID() }
        original.retired = true
        accessControlTask?.cancel()
        guard accessControlTask == nil, original.handle != nil || original.bindingHandle != nil || original.snapshotHandle != nil || original.inspectionHandle != nil else { return }
        hostSettingsAccessBusy = true
        accessControlTask = Task { [self, original] in
            await cancelAccessInteraction(original)
            hostSettingsAccessBusy = false
            accessControlTask = nil
        }
    }

    private func cancelAccessInteraction(_ original: SettingsAccessInteraction) async {
        let inspection = original.inspectionHandle
        let snapshot = original.snapshotHandle
        let binding = original.bindingHandle
        let access = original.handle
        if let inspection { await original.service.cancelSettings(inspection) }
        if original.inspectionHandle === inspection { original.inspectionHandle = nil }
        if let snapshot { await original.service.cancelSettings(snapshot) }
        if original.snapshotHandle === snapshot { original.snapshotHandle = nil }
        if let binding { await original.service.cancelSettings(binding) }
        if original.bindingHandle === binding { original.bindingHandle = nil }
        if let access { await original.service.cancelSettings(access) }
        if original.handle === access { original.handle = nil }
    }

    private func adoptHostSettingsAccess(_ updated: ReplicaViewState) {
        hostSettingsInspection = updated.settingsInspection
        hostSettingsInspectionStatus = updated.settingsInspectionRequest
        hostSettingsSnapshot = updated.settingsSnapshot
        hostSettingsSnapshotStatus = updated.settingsSnapshotRequest
        hostSettingsSnapshotFresh = false
        hostSettingsInspectionFresh = false
        hostSettingsBinding = updated.settingsBinding
        hostSettingsBindingFresh = false
        if let binding = updated.settingsBinding, let original = accessInteraction,
           isCurrentSettingsScopeInteraction(original), original.bindingHandle === binding.requestHandle,
           case .currentConnectionObservation = binding.freshness {
            hostSettingsBindingFresh = true
        }
        if let snapshot = updated.settingsSnapshot, let original = accessInteraction,
           let capture = original.snapshotInput, original.snapshotHandle === snapshot.requestHandle,
           isCurrentSnapshotInteraction(original, capture: capture),
           case .currentConnectionObservation = snapshot.freshness {
            hostSettingsSnapshotFresh = true
        }
        if let inspection = updated.settingsInspection, let original = accessInteraction,
           let capture = original.inspectionInput, original.inspectionHandle === inspection.requestHandle,
           isCurrentInspectionInteraction(original, capture: capture),
           case .currentConnectionObservation = inspection.freshness {
            hostSettingsInspectionFresh = true
        }
        hostSettingsAccessStatus = updated.settingsAccessRequest
        guard let display = updated.settingsAccess else {
            hostSettingsAccess = nil
            hostSettingsAccessFresh = false
            return
        }
        hostSettingsAccess = display
        guard let original = accessInteraction, isCurrentAccessInteraction(original),
              original.handle === display.requestHandle else {
            hostSettingsAccessFresh = false
            return
        }
        if case .currentConnectionObservation = display.freshness { hostSettingsAccessFresh = true }
        else { hostSettingsAccessFresh = false }
    }

    var hostSettingsBusy: Bool { hostSettingsTask != nil || accessControlTask != nil }

    var canRefreshHostSettings: Bool { hostSettingsAccessEligible && !hostSettingsBusy }

    var hostSettingsSummary: ReplicaSettingsAccessDisplaySummary? {
        guard let access = hostSettingsAccess, access.requestsAvailable,
              case .available(let summary) = access.display.status else { return nil }
        return summary
    }

    var hostValuesCurrent: Bool {
        guard let hostValues, let scope = state?.settingsScope else { return false }
        return hostValues.scope === scope && hostSettingsSummary != nil
    }

    var canSaveHostSettings: Bool {
        hostSettingsAccessEligible && !hostSettingsBusy && hostValuesCurrent
            && hostSettingsSummary?.writableKeys.isEmpty == false && hostMutation?.isOpen != true
    }

    var canAskHostSettings: Bool {
        guard hostSettingsAccessEligible, let summary = hostSettingsSummary,
              case .configuredProvider = summary.sessionConsent else { return false }
        return true
    }

    func refreshHostSettings() {
        hostSettingsAccessIssue = nil
        hostChangesPages = 0
        hostAccessRetried = false
        var steps: [HostSettingsStep] = [.access]
        if hostMutation?.isOpen == true { steps.append(.update) }
        steps += [.snapshot, .changes]
        enqueueHostSettings(steps)
    }

    func saveHostSettings(_ changes: [SettingChange]) {
        guard canSaveHostSettings, let hostValues, !changes.isEmpty else { return }
        hostSettingsAccessIssue = nil
        hostMutation = HostSettingsMutation(requestID: UUID(), expectedRevision: hostValues.snapshot.revision,
                                            changes: changes, state: .sending)
        hostChangesPages = 0
        enqueueHostSettings([.update, .snapshot, .changes])
    }

    func reconcileHostSettingsMutation() {
        guard hostMutation?.isOpen == true else { return }
        refreshHostSettings()
    }

    func forgetHostSettingsMutation() {
        guard hostSettingsTask == nil else { return }
        hostMutation = nil
    }

    func confirmHostAction(_ action: RemoteSettingsHostAction) {
        hostActionResult = nil
        enqueueHostSettings([.hostAction(action)])
    }

    func openHostSettingsAssistant() {
        guard canAskHostSettings, assistant.chat == nil || assistant.ended != nil else { return }
        assistant = HostSettingsAssistant(openRequestID: UUID())
        enqueueHostSettings([.open])
    }

    func sendHostSettingsAssistant(_ text: String) -> Bool {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard canAskHostSettings, assistant.chat != nil, assistant.ended == nil, !assistant.isRunning,
              !trimmed.isEmpty, text.utf8.count <= RemoteSettingsQuery.maximumSessionTextBytes,
              !text.contains("\0") else { return false }
        assistant.turns.append(HostSettingsTurn(id: UUID(), text: text, accepted: false))
        assistant.changesFloor = hostChangesCursor
        enqueueHostSettings([.send])
        return true
    }

    func closeHostSettingsAssistant() {
        guard assistant.chat != nil, assistant.ended == nil else {
            assistant = HostSettingsAssistant()
            return
        }
        enqueueHostSettings([.close])
    }

    private func forgetHostSettings() {
        hostValues = nil
        hostChanges = []
        hostChangesCursor = 0
        hostMutation = nil
        hostActionResult = nil
        hostAutoRefreshScope = nil
        assistant = HostSettingsAssistant()
    }

    private func autoRefreshHostSettings() {
        guard accessSurfaceVisible, settingsPath == [.host], hostSettingsAccessEligible, !hostSettingsBusy,
              let scope = state?.settingsScope, hostAutoRefreshScope !== scope, !hostValuesCurrent else { return }
        hostAutoRefreshScope = scope
        refreshHostSettings()
    }

    private func enqueueHostSettings(_ steps: [HostSettingsStep]) {
        for step in steps where !hostSettingsQueue.contains(step) && hostSettingsQueue.count < 16 {
            hostSettingsQueue.append(step)
        }
        startHostSettingsDriver()
    }

    private func startHostSettingsDriver() {
        guard hostSettingsTask == nil, accessControlTask == nil, hostSettingsAccessEligible,
              !hostSettingsQueue.isEmpty else { return }
        hostSettingsTask = Task { [self] in
            while !Task.isCancelled, hostSettingsAccessEligible, !hostSettingsQueue.isEmpty {
                let step = hostSettingsQueue.removeFirst()
                hostSettingsStep = step
                await runHostSettings(step)
            }
            hostSettingsStep = nil
            hostSettingsTask = nil
            if !Task.isCancelled, hostSettingsAccessEligible, !hostSettingsQueue.isEmpty { startHostSettingsDriver() }
        }
    }

    private func hostSettingsQuery(_ step: HostSettingsStep) -> RemoteSettingsQuery? {
        switch step {
        case .access: return .accessStatus
        case .snapshot:
            guard let summary = hostSettingsSummary, !summary.readableKeys.isEmpty else { return nil }
            return .snapshot(keys: summary.readableKeys)
        case .changes:
            guard hostSettingsSummary != nil else { return nil }
            return .changes(afterSequence: hostChangesCursor, limit: 32)
        case .update:
            guard let mutation = hostMutation, mutation.isOpen else { return nil }
            return .update(requestID: mutation.requestID, expectedRevision: mutation.expectedRevision, changes: mutation.changes)
        case .hostAction(let action): return .hostAction(action)
        case .open:
            guard let openRequestID = assistant.openRequestID, assistant.chat == nil else { return nil }
            return .sessionOpen(openRequestID: openRequestID)
        case .send:
            guard let chat = assistant.chat, assistant.ended == nil, let turn = assistant.pendingTurn else { return nil }
            return .sessionSend(chatID: chat.chatID, turnID: turn.id, providerBindingSHA256: chat.providerBindingSHA256, text: turn.text)
        case .poll:
            guard let chat = assistant.chat, assistant.ended == nil else { return nil }
            let offset = assistant.textRevision == nil ? 0 : UInt64(assistant.text.count)
            return .sessionPoll(chatID: chat.chatID, textOffset: min(offset, RemoteSettingsQuery.maximumTextOffset))
        case .close:
            guard let chat = assistant.chat else { return nil }
            return .sessionClose(chatID: chat.chatID)
        }
    }

    private func runHostSettings(_ step: HostSettingsStep) async {
        if step == .poll, assistant.pollAfterDelay {
            assistant.pollAfterDelay = false
            do { try await Task.sleep(for: .milliseconds(1500)) } catch { return }
        }
        guard let service, let state, let host = state.snapshot.host, let scope = state.settingsScope,
              let query = hostSettingsQuery(step) else { return }
        let input: ReplicaSettingsAccessInput
        do { input = try ReplicaSettingsAccessGesture(service: service, scope: scope, query: query).take() }
        catch {
            hostSettingsAccessIssue = "The Host request could not capture its original time."
            return
        }
        var original: SettingsAccessInteraction?
        if step == .access {
            let interaction = SettingsAccessInteraction(service: service, host: host, localID: state.identity.deviceID,
                                                        scope: scope, epoch: accessSurfaceEpoch,
                                                        started: input.monotonicFloor, wallFloor: input.wallFloor)
            accessInteraction = interaction
            original = interaction
            hostSettingsAccessFresh = false
        }
        if step == .update, var mutation = hostMutation {
            mutation.attempts += 1
            mutation.state = mutation.attempts == 1 ? .sending : .uncertain
            hostMutation = mutation
        }
        var handle: ReplicaSettingsRequestHandle?
        do {
            let requested = try await service.requestSettingsAccess(input)
            handle = requested
            original?.handle = requested
            var outcome = ReplicaSettingsQueryOutcome.pending
            while case .pending = outcome {
                try await Task.sleep(for: .milliseconds(200))
                outcome = await service.settingsQueryOutcome(requested)
            }
            if step == .access { await refresh() }
            applyHostSettings(step, outcome: outcome)
        } catch {
            if let handle { await service.cancelSettings(handle) }
            interruptHostSettings(step, error: error)
        }
    }

    private func interruptHostSettings(_ step: HostSettingsStep, error: Error) {
        if step == .update, var mutation = hostMutation, mutation.isOpen {
            mutation.state = .uncertain
            hostMutation = mutation
        }
        if error is CancellationError { return }
        if (error as? WireError) == .unavailable, step != .access, !hostAccessRetried {
            hostAccessRetried = true
            hostSettingsQueue.insert(contentsOf: [.access, step], at: 0)
            return
        }
        hostSettingsQueue.removeAll()
        hostSettingsAccessIssue = step == .update
            ? "The change could not be confirmed. Its original identity is kept; refresh to reconcile with the Host."
            : "The Host request could not be completed. Current Host values have not been verified."
    }

    private func applyHostSettings(_ step: HostSettingsStep, outcome: ReplicaSettingsQueryOutcome) {
        switch outcome {
        case .pending: return
        case .accessObserved:
            guard step == .access else { return interruptHostSettings(step, error: WireError.unavailable) }
            if case .grantUnavailable = hostSettingsAccess?.display.status {
                hostSettingsQueue.removeAll()
                hostSettingsAccessIssue = "The Host reports no Settings access for this iPhone. Host values below are not current."
            }
        case .ended(let status):
            if step == .update, var mutation = hostMutation, mutation.isOpen {
                mutation.state = .uncertain
                hostMutation = mutation
            }
            if step == .send || step == .open {
                hostSettingsAccessIssue = "The Settings chat request did not finish. Its identity is kept and is resent when you return."
            } else if status != .cancelled {
                hostSettingsAccessIssue = step == .update
                    ? "No Host reply arrived. The change outcome is unknown; refresh to reconcile."
                    : "The Host request did not finish. Current Host values have not been verified."
            }
            hostSettingsQueue.removeAll()
        case .received(let result, let observedAt):
            applyHostSettings(step, result: result, observedAt: observedAt)
        }
    }

    private func applyHostSettings(_ step: HostSettingsStep, result: ReplicaSettingsQueryResult, observedAt: UInt64) {
        switch result {
        case .snapshot(let snapshot):
            if let scope = state?.settingsScope { hostValues = HostSettingsValues(snapshot: snapshot, observedAt: observedAt, scope: scope) }
        case .changes(let page):
            hostChanges = Array((hostChanges + page.changes).suffix(32))
            if let next = page.nextSequence { hostChangesCursor = max(hostChangesCursor, next) }
            hostChangesPages += 1
            if page.changes.count == 32, hostChangesPages < 8 { hostSettingsQueue.insert(.changes, at: 0) }
        case .update(let requestID, let operation):
            guard var mutation = hostMutation, mutation.requestID == requestID else { return }
            mutation.state = .recorded(operation)
            hostMutation = mutation
        case .hostActionRequired(let action):
            hostActionResult = action
        case .sessionOpened(let chat):
            assistant.chat = chat
        case .sessionAccepted(let chatID, let turnID):
            guard assistant.chat?.chatID == chatID,
                  let index = assistant.turns.firstIndex(where: { $0.id == turnID }) else { return }
            assistant.turns[index].accepted = true
            enqueueHostSettings([.poll])
        case .sessionStatus(let status):
            applyAssistantStatus(status, closing: step == .close)
        case .denied(let reason):
            applyHostSettingsDenial(step, reason: reason)
        }
    }

    private func applyAssistantStatus(_ status: RemoteSettingsChatStatus, closing: Bool) {
        guard assistant.chat?.chatID == status.chatID else { return }
        if assistant.textRevision != status.textRevision {
            assistant.textRevision = status.textRevision
            assistant.text = Data()
        }
        if status.textOffset == UInt64(assistant.text.count) { assistant.text.append(Data(status.text.utf8)) }
        let previous = assistant.status?.operationIDs ?? []
        assistant.status = status
        switch status.phase {
        case .ready, .running, .completed:
            if closing { assistant.ended = "This Settings chat is closed." }
        default:
            assistant.ended = closing ? "This Settings chat is closed." : "This Settings chat ended on the Host."
        }
        if UInt64(assistant.text.count) < status.textBytes, UInt64(assistant.text.count) <= RemoteSettingsQuery.maximumTextOffset {
            enqueueHostSettings([.poll])
        } else if assistant.isRunning, assistant.ended == nil {
            assistant.pollAfterDelay = true
            enqueueHostSettings([.poll])
        }
        if status.operationIDs != previous, !status.operationIDs.isEmpty {
            enqueueHostSettings([.snapshot, .changes])
        }
    }

    private func applyHostSettingsDenial(_ step: HostSettingsStep, reason: RemoteSettingsDenial) {
        if step == .update, var mutation = hostMutation {
            switch reason {
            case .conflict, .outsideScope, .writeBlocked, .invalid:
                mutation.state = .rejected(reason)
            case .grantUnavailable, .capacity:
                mutation.state = mutation.attempts > 1 ? .uncertain : .rejected(reason)
            case .grantChanged, .outcomeUnknown, .unavailable:
                mutation.state = .uncertain
            }
            hostMutation = mutation
        }
        switch reason {
        case .grantChanged where !hostAccessRetried:
            hostAccessRetried = true
            hostSettingsQueue.insert(contentsOf: [.access, step], at: 0)
            return
        case .grantUnavailable, .grantChanged:
            hostSettingsQueue.removeAll()
            hostSettingsAccessIssue = "Host Settings access for this iPhone was revoked or changed. Controls are disabled until a refresh shows current access."
        case .outsideScope:
            if step == .open || step == .send || step == .poll || step == .close {
                assistant.ended = "The Host refused this Settings chat. Its consent, scope or chat changed; open a new chat after refreshing."
            } else {
                hostSettingsAccessIssue = "The Host refused a request outside this iPhone's current scope."
            }
            if !hostAccessRetried {
                hostAccessRetried = true
                hostSettingsQueue = [.access]
            }
        case .conflict:
            hostSettingsAccessIssue = step == .update
                ? "Not saved: Host settings changed since they were read. Review the refreshed values."
                : "The Host reported a conflicting reuse of this request."
            if step == .update { hostSettingsQueue = [.snapshot, .changes] }
        case .capacity:
            hostSettingsAccessIssue = step == .open
                ? "Another Settings chat for this iPhone is still live on the Host. Close it or wait for it to expire."
                : "The Host is at capacity. Try again later."
        case .invalid:
            if step == .poll {
                assistant.textRevision = nil
                assistant.text = Data()
            } else {
                hostSettingsAccessIssue = "The Host rejected the request as invalid."
            }
        case .outcomeUnknown, .unavailable, .writeBlocked:
            hostSettingsAccessIssue = step == .open || step == .send
                ? "The Settings assistant is unavailable on the Host (no configured model, credential or free runtime). No other provider is used."
                : "The Host could not complete this request now. Nothing is reported as saved."
        }
    }

    private func requestLocalDrain(permanent: Bool = false) {
        guard let owner = localSettings else { return }
        localDrainPending = true
        localPermanentDrain = localPermanentDrain || permanent
        guard localDrainTask == nil else { return }
        localDrainTask = Task { [self, owner] in
            while localDrainPending {
                localDrainPending = false
                if localPermanentDrain || !localSceneActive || !platformActive {
                    if let notificationDeliveryTask { await notificationDeliveryTask.value }
                }
                if localPermanentDrain { await owner.drain() }
                else { await owner.drainSurface() }
            }
            localDrainTask = nil
        }
    }

    private func present(_ error: Error) {
        localCoverChanged(.globalError, presented: true)
        self.error = (error as? WireError)?.localizedDescription ?? "The operation could not be completed. Existing local work has been preserved."
    }

    private func notificationEligible(service expectedService: ReplicaService, owner: ThisDeviceSettingsStore,
                                      host: DevicePublicKeys, generation: UUID) -> Bool {
        service === expectedService && localSettings === owner && !owner.isClosed && !localIdentityInvalid
            && localDeviceID == owner.deviceID && state?.identity.deviceID == owner.deviceID
            && state?.snapshot.host == host && state?.snapshot.revoked == false && connectionEnabled
            && localSceneActive && platformActive && UIApplication.shared.applicationState == .active
            && notificationGeneration == generation
    }

    private func requestNotificationDelivery() {
        guard let service, let owner = localSettings, let host = state?.snapshot.host else { return }
        let generation = notificationGeneration
        guard notificationEligible(service: service, owner: owner, host: host, generation: generation) else { return }
        notificationDeliveryPending = true
        guard notificationDeliveryTask == nil else { return }
        notificationDeliveryTask = Task { [self, service, owner, host] in
            while notificationDeliveryPending && !Task.isCancelled
                && notificationEligible(service: service, owner: owner, host: host, generation: generation) {
                notificationDeliveryPending = false
                do {
                    let page = try await service.claimNotifications()
                    guard !Task.isCancelled,
                          notificationEligible(service: service, owner: owner, host: host, generation: generation) else { break }
                    for notice in page.notices {
                        guard !Task.isCancelled,
                              notificationEligible(service: service, owner: owner, host: host, generation: generation) else { break }
                        let outcome = try await notifications.deliver(notice, prepare: { [self, service, owner, host] in
                            let saved: DevicePreferenceSnapshot
                            do { saved = try await owner.notificationDeliveryPreferences() }
                            catch {
                                notificationDeliveryStatus = "A claimed notification was skipped because saved preferences were unavailable."
                                return false
                            }
                            guard !Task.isCancelled,
                                  notificationEligible(service: service, owner: owner, host: host, generation: generation) else { return false }
                            let enabled: Bool
                            switch notice.category {
                            case .completion: enabled = saved.completion
                            case .attention: enabled = saved.attention
                            }
                            guard enabled else {
                                notificationDeliveryStatus = "Claimed notifications follow the saved completion and attention choices. Disabled notices are skipped."
                                return false
                            }
                            try await service.validateNotificationScope(host: host, localID: owner.deviceID)
                            return !Task.isCancelled && notificationEligible(service: service, owner: owner, host: host, generation: generation)
                        }, isEligible: { [self, service, owner, host] in
                            notificationEligible(service: service, owner: owner, host: host, generation: generation)
                        })
                        switch outcome {
                        case .submitted:
                            notificationDeliveryStatus = "The OS accepted the latest notification request. Presentation is not guaranteed."
                        case .suppressedPermission:
                            notificationDeliveryStatus = "A claimed notification was skipped because current OS settings do not allow alerts. No permission prompt was opened."
                        case .suppressedPreparation: break
                        case .suppressedEligibility:
                            notificationDeliveryStatus = "A claimed notification was skipped because its original foreground or device context changed."
                        case .retiredAfterSubmission:
                            notificationDeliveryStatus = "Notification submission finished after foreground retirement. Pending removal was requested; presentation may already have occurred."
                        }
                    }
                } catch is CancellationError {
                    break
                } catch {
                    notificationDeliveryStatus = "A notification attempt failed. Claimed events will not be retried; preference saves are separate."
                    break
                }
            }
            notificationDeliveryTask = nil
        }
    }

    private func retireNotificationDelivery() {
        notificationGeneration = UUID()
        notificationDeliveryPending = false
        notificationDeliveryTask?.cancel()
    }
}

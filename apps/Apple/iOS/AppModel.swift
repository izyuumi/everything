import Foundation
import Observation
import UIKit

enum ClientTab: Hashable {
    case chat, approvals, settings
}

enum ClientSettingsDestination: Hashable {
    case thisDevice
}

enum LocalSettingsCover: Hashable {
    case rootSheet, globalError, deviceDisable, localConfirmation
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
    private(set) var selectedTab: ClientTab = .chat
    private(set) var settingsPath: [ClientSettingsDestination] = []
    var pairing: PairingSummary?
    var pairingRequest = ""
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
    @ObservationIgnored private var accessSurfaceEpoch = UUID()

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
            await service?.stop()
            if let access { await access.value }
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
            } else if state?.settingsScope !== updated.settingsScope {
                retireHostSettingsAccess()
            }
            state = updated
            adoptHostSettingsAccess(updated)
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
            return
        } catch {
            hostSettingsAccessFresh = false
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
        await service?.stop()
        if let access { await access.value }
        if let delivery { await delivery.value }
        await refresh()
    }

    func saveRelay(_ origin: String) async {
        await perform {
            guard let service else { throw WireError.unavailable }
            try await service.configureRelay(origin)
        }
    }

    func addConversation(_ value: String, title: String) async -> Bool {
        await perform {
            guard let service, let id = UUID(uuidString: value), id != WireID.zero else { throw WireError.invalid }
            try await service.addConversation(id, title: title)
            selectedConversation = id
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

    func resolve(_ approval: ReplicaApproval, allow: Bool) async {
        await perform {
            guard let service else { throw WireError.unavailable }
            try await service.resolve(approval, allow: allow)
        }
    }

    func synchronize() async {
        await perform {
            guard let service, let selectedConversation else { throw WireError.unavailable }
            try await service.synchronize(selectedConversation)
        }
    }

    func inspectInvitation(_ value: String) async {
        await perform {
            guard let service else { throw WireError.unavailable }
            pairing = try await service.inspectInvitation(value)
            pairingRequest = ""
        }
    }

    func requestPairing(confirmedFingerprint: String) async {
        await perform {
            guard let service else { throw WireError.unavailable }
            pairingRequest = try await service.createPairingRequest(confirmedFingerprint: confirmedFingerprint)
        }
    }

    func finishPairing(_ value: String) async -> Bool {
        await perform {
            guard let service else { throw WireError.unavailable }
            try await service.finishPairing(value)
            pairing = nil
            pairingRequest = ""
        }
    }

    func cancelPairing() async {
        await service?.cancelPairing()
        pairing = nil
        pairingRequest = ""
    }

    func disable() async {
        retireHostSettingsAccess()
        retireNotificationDelivery()
        let delivery = notificationDeliveryTask
        let access = accessControlTask
        await perform {
            guard let service else { throw WireError.unavailable }
            try await service.disable()
            connectionEnabled = false
        }
        if let access { await access.value }
        if let delivery { await delivery.value }
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
        guard path.count <= 1 else {
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
        localDestinationVisible = presented && settingsPath.last == .thisDevice
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
            let directory = support.appendingPathComponent("Everything", isDirectory: true).appendingPathComponent("DevicePreferences", isDirectory: true)
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
        hostSettingsAccessEligible && !hostSettingsAccessBusy
            && hostSettingsAccessStatus?.blocksNewRequest != true
    }

    private var hostSettingsAccessEligible: Bool {
        accessSurfaceVisible && localSceneActive && platformActive && UIApplication.shared.applicationState == .active
            && selectedTab == .settings && settingsPath.isEmpty && localCovers.isEmpty
            && connectionEnabled && state?.connection == "Connected" && state?.settingsScope != nil
            && state?.snapshot.host != nil && state?.snapshot.revoked == false
    }

    private func isCurrentAccessInteraction(_ original: SettingsAccessInteraction) -> Bool {
        guard accessInteraction === original, !original.retired, accessSurfaceEpoch == original.epoch,
              service === original.service, hostSettingsAccessEligible, state?.snapshot.host == original.host,
              state?.identity.deviceID == original.localID, state?.settingsScope === original.scope,
              let wall = try? WireID.now() else { return false }
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
        hostSettingsAccessFresh = false
        hostSettingsAccessIssue = nil
        hostSettingsAccessBusy = true
        accessControlTask = Task { [self, original] in
            defer {
                hostSettingsAccessBusy = false
                accessControlTask = nil
            }
            do {
                guard isCurrentAccessInteraction(original) else { throw CancellationError() }
                let handle = try await original.service.requestSettingsAccess(input)
                original.handle = handle
                guard !Task.isCancelled, isCurrentAccessInteraction(original) else {
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
                    hostSettingsAccessIssue = "The Host access check could not be started. No Host setting was changed."
                }
            }
        }
    }

    func cancelHostSettingsAccess() {
        retireHostSettingsAccess()
    }

    func hostSettingsAccessSurfaceChanged(visible: Bool) {
        guard accessSurfaceVisible != visible else { return }
        accessSurfaceVisible = visible
        if !visible { retireHostSettingsAccess() }
    }

    private func retireHostSettingsAccess() {
        hostSettingsAccessFresh = false
        guard let original = accessInteraction else { return }
        if !original.retired { accessSurfaceEpoch = UUID() }
        original.retired = true
        accessControlTask?.cancel()
        guard accessControlTask == nil, let handle = original.handle else { return }
        hostSettingsAccessBusy = true
        accessControlTask = Task { [self, original] in
            await original.service.cancelSettings(handle)
            original.handle = nil
            hostSettingsAccessBusy = false
            accessControlTask = nil
        }
    }

    private func cancelAccessInteraction(_ original: SettingsAccessInteraction) async {
        guard let handle = original.handle else { return }
        await original.service.cancelSettings(handle)
        if original.handle === handle { original.handle = nil }
    }

    private func adoptHostSettingsAccess(_ updated: ReplicaViewState) {
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

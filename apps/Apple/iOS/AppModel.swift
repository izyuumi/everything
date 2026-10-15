import Foundation
import Observation

@MainActor
@Observable
final class AppModel {
    var state: ReplicaViewState?
    var events: [WireEvent] = []
    var selectedConversation: UUID?
    var busy = false
    var error: String?
    var connectionEnabled = false
    var notificationsEnabled = false
    var pairing: PairingSummary?
    var pairingRequest = ""
    @ObservationIgnored private var service: ReplicaService?
    @ObservationIgnored private var opening: (id: UUID, task: Task<ReplicaService, Error>)?
    @ObservationIgnored private let notifications = NotificationService()
    @ObservationIgnored private var refreshEpoch = UUID()
    @ObservationIgnored private var lastCursors: [UUID: UInt64] = [:]

    func scene(active: Bool) async {
        if !active {
            await service?.stop()
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
            do { try await service?.start() } catch { present(error) }
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
            for conversation in updated.snapshot.conversations {
                if let previous = lastCursors[conversation.id], conversation.cursor > previous, notificationsEnabled {
                    try await notifications.activity()
                }
                guard refreshEpoch == captured, !Task.isCancelled else { return }
                lastCursors[conversation.id] = max(lastCursors[conversation.id] ?? 0, conversation.cursor)
            }
            guard refreshEpoch == captured, !Task.isCancelled else { return }
            state = updated
            let conversation = selectedConversation
            await service.selectConversation(conversation)
            let updatedEvents: [WireEvent]
            if let conversation { updatedEvents = try await service.events(conversation: conversation) }
            else { updatedEvents = [] }
            guard refreshEpoch == captured, selectedConversation == conversation, !Task.isCancelled else { return }
            events = updatedEvents
        } catch is CancellationError {
            return
        } catch { present(error) }
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
        connectionEnabled = false
        await service?.stop()
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
        await perform {
            guard let service else { throw WireError.unavailable }
            try await service.disable()
            connectionEnabled = false
        }
    }

    func authorizeNotifications() async {
        await perform { notificationsEnabled = try await notifications.authorize() }
    }

    private func present(_ error: Error) {
        self.error = (error as? WireError)?.localizedDescription ?? "The operation could not be completed. Existing local work has been preserved."
    }
}

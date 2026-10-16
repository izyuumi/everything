import AppKit
import Foundation
import Observation

@MainActor
@Observable
final class SettingsAssistantController {
    enum Phase: String {
        case idle, opening, ready, sending, accepted, running, completed, paused, closing, closed, unavailable, stoppedUncertain, stoppedUnusable, unknown
    }

    struct RecentTurn: Identifiable {
        let id: UUID
        let question: String
        let answer: String
    }

    struct Summary {
        let context: NativeSettingsAssistantWire.ContextObservation?
        let attempts: UInt32
        let chargedTokens: UInt64
        let reservedOutputTokens: UInt64
        let providerComplete: Bool
        let usageComplete: Bool
    }

    struct Cleanup: Identifiable {
        let id: UUID
        let requestID: UUID
        let connectionKey: UUID
        let closed: Bool
        let parentRetired: Bool
        let issue: NativeSettingsSessionError?
    }

    @MainActor
    final class Navigation {
        fileprivate let open: @MainActor @Sendable (SettingsSection) async throws -> NativeSettingsStore.AssistantSectionResult
        fileprivate let retire: @MainActor @Sendable () -> Void

        init(open: @escaping @MainActor @Sendable (SettingsSection) async throws -> NativeSettingsStore.AssistantSectionResult,
             retire: @escaping @MainActor @Sendable () -> Void) {
            self.open = open
            self.retire = retire
        }
    }

    @MainActor
    @Observable
    final class Card: Identifiable {
        let id: UUID
        fileprivate let original: NativeSettingsSession.AssistantOperationHandle
        fileprivate let contextID: UUID
        fileprivate(set) var inspection: SettingsOperationInspection?
        fileprivate(set) var receipt: NativeSettingsSession.AssistantDetailsReceipt?
        fileprivate(set) var target: SettingsActionTarget?
        fileprivate(set) var issue: NativeSettingsSessionError?
        fileprivate(set) var isWorking = false

        fileprivate init(original: NativeSettingsSession.AssistantOperationHandle, contextID: UUID) {
            self.original = original
            self.contextID = contextID
            id = original.operationID
        }
    }

    @MainActor
    private final class Completion<Value: Sendable> {
        private var result: Result<Value, NativeSettingsSessionError>?
        private var waiter: CheckedContinuation<Result<Value, NativeSettingsSessionError>, Never>?

        func finish(_ value: Result<Value, NativeSettingsSessionError>) {
            guard result == nil else { return }
            result = value
            waiter?.resume(returning: value)
            waiter = nil
        }

        func value() async -> Result<Value, NativeSettingsSessionError> {
            if let result { return result }
            return await withCheckedContinuation { waiter = $0 }
        }
    }

    private enum Question {
        case none
        case review(String, UUID, UUID)
        case submitted(String, UUID)
    }

    @MainActor
    @Observable
    fileprivate final class Context {
        let id = UUID()
        let action: NativeSettingsStore.AssistantOpenAction
        let connectionKey: UUID
        var chat: NativeSettingsSession.AssistantHandle?
        var knownChatID: UUID?
        var retired = false
        var knownClosed = false
        var closeAttempted = false
        var parentRetired = false
        var issue: NativeSettingsSessionError?
        var phase = Phase.opening
        var pendingTurn: UUID?
        var turns: [UUID: NativeSettingsStore.AssistantSendAction] = [:]
        var archivedTurns: Set<UUID> = []
        var cards: [UUID: Card] = [:]
        var ownDevice: NativeSettingsStore.DeviceHandle?
        var deviceHandoff = false
        var navigating = false
        var openTask: Task<Void, Never>?
        var sendTask: Task<Void, Never>?
        var cardTask: Task<Void, Never>?
        var closeTask: Task<Void, Never>?

        init(action: NativeSettingsStore.AssistantOpenAction, connectionKey: UUID) {
            self.action = action
            self.connectionKey = connectionKey
        }
    }

    private(set) var phase = Phase.idle
    private(set) var composer = ""
    private(set) var recent: [RecentTurn] = []
    private(set) var committed = ""
    private(set) var provisional = ""
    private(set) var provisionalState: NativeSettingsAssistantWire.ProvisionalState?
    private(set) var cards: [Card] = []
    private(set) var summary: Summary?
    private(set) var issue: NativeSettingsSessionError?
    private(set) var inputIssue: String?
    private(set) var expanded = false
    private var windowAvailable = false
    private(set) var isInvalidated = false
    @ObservationIgnored private let store: NativeSettingsStore
    @ObservationIgnored private let navigation: Navigation?
    private var connectionKey: UUID
    @ObservationIgnored private weak var window: NSWindow?
    private var surfaceVisible = false
    private var applicationActive = false
    private var covered = false
    @ObservationIgnored private var publication = UUID()
    private var question = Question.none
    @ObservationIgnored private var contexts: [UUID: Context] = [:]
    @ObservationIgnored private var current: Context?
    @ObservationIgnored private var pollTask: Task<Void, Never>?
    @ObservationIgnored private var pollContextID: UUID?
    @ObservationIgnored private var pollRequested = false
    @ObservationIgnored private var drainTask: Task<[Cleanup], Never>?

    init(store: NativeSettingsStore, connectionKey: UUID, navigation: Navigation? = nil) {
        self.store = store
        self.connectionKey = connectionKey
        self.navigation = navigation
    }

    var disclosure: NativeSettingsAssistantWire.Opened? { canPublish ? current?.chat?.opened : nil }
    var reviewText: String? {
        guard canPublish, case .review(let text, _, _) = question else { return nil }
        return text
    }
    var submittedQuestion: String? {
        guard canPublish, case .submitted(let text, _) = question else { return nil }
        return text
    }
    var navigationAvailable: Bool { navigation != nil }
    var controlsAreStale: Bool { store.snapshotIsStale }
    var controlsConflict: Bool { store.draftConflict }
    var canPublish: Bool {
        guard let current, permitsPresentation(current), childIsLive(current) else { return false }
        return !current.deviceHandoff && current.ownDevice == nil && !current.navigating
    }
    var canOpen: Bool {
        surfaceEligible && store.sessionPresent && store.information != nil && contexts.count < 8
            && !contexts.values.contains { store.isCurrentAssistantOwner($0.action) }
    }
    var canReview: Bool {
        guard canPublish, let current, let chat = current.chat,
              current.openTask == nil, current.sendTask == nil, current.pendingTurn == nil, current.issue == nil,
              current.phase == .ready || current.phase == .completed else { return false }
        return (try? chat.validateLifetime()) != nil
    }
    var canSend: Bool {
        guard canReview, let current, let chat = current.chat,
              case .review(_, let contextID, let captured) = question else { return false }
        return contextID == current.id && captured == publication && chat.opened.chatID == current.knownChatID
    }
    var hasCurrentContext: Bool { current != nil }
    var canRecoverOpenedChild: Bool {
        guard canInspectOpen, let current else { return false }
        return !current.retired && surfaceEligible && current.chat == nil && current.phase == .ready
    }
    var canInspectOpen: Bool {
        guard let current else { return false }
        return !isInvalidated && current.openTask == nil && current.closeTask == nil
            && current.cardTask == nil && store.isCurrentAssistantOwner(current.action)
    }
    var cleanup: [Cleanup] {
        contexts.values.map {
            Cleanup(id: $0.id, requestID: $0.action.requestID, connectionKey: $0.connectionKey,
                    closed: $0.knownClosed, parentRetired: $0.parentRetired, issue: $0.issue)
        }.sorted { $0.id.uuidString < $1.id.uuidString }
    }

    func attachWindow(_ selected: NSWindow?) {
        if window !== selected, window != nil { parentWillRetire() }
        window = selected
        windowAvailable = selected != nil
    }

    func setExpanded(_ value: Bool) {
        guard !isInvalidated else { return }
        expanded = value
        if !value { retireCurrent() }
        else { reconcileCleanup() }
    }

    func setSurfaceVisible(_ value: Bool) {
        surfaceVisible = value && !isInvalidated
        if !surfaceVisible { retireCurrent() }
    }

    func setApplicationActive(_ value: Bool) {
        applicationActive = value && NSApplication.shared.isActive && !isInvalidated
        if !applicationActive { retireCurrent() }
    }

    func setCovered(_ value: Bool) {
        guard covered != value else { return }
        covered = value
        pausePublication()
        if !value {
            reconcileDevicePresentation()
            resumePublication()
        }
    }

    func parentWillRetire() { retireCurrent() }

    func applyConnectionKey(_ value: UUID) {
        guard connectionKey != value else { return }
        retireCurrent()
        connectionKey = value
    }

    func ownershipChanged() {
        if let current, !store.isCurrentAssistantOwner(current.action) { retireCurrent() }
        reconcileDevicePresentation()
        reconcileCleanup()
        resumePublication()
    }

    func setComposer(_ value: String) {
        guard canReview else { return }
        guard value.utf8.count <= 8192, !value.contains("\0") else {
            inputIssue = "Keep the question within 8192 UTF-8 bytes and remove null characters."
            return
        }
        composer = value
        if case .review = question { question = .none }
        inputIssue = nil
    }

    func open() {
        reconcileCleanup()
        guard canOpen else { issue = .inactive; return }
        do {
            let action = try store.prepareAssistantOpen()
            let context = Context(action: action, connectionKey: connectionKey)
            contexts[context.id] = context
            current = context
            clearPrivateCells()
            issue = nil
            phase = .opening
            startOpen(context)
        } catch { issue = sanitize(error) }
    }

    func reviewQuestion() {
        guard canReview, let current, !composer.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return }
        question = .review(composer, current.id, publication)
        inputIssue = nil
    }

    func discardReview() {
        if case .review = question { question = .none }
    }

    func sendReviewedQuestion() {
        guard canSend, let context = current, let chat = context.chat,
              case .review(let text, _, _) = question else { return }
        do {
            let consent = try store.acceptAssistantDisclosure(chat)
            let action = try store.prepareAssistantSend(chat, consent: consent, text: text)
            context.turns[action.turnID] = action
            context.pendingTurn = action.turnID
            context.phase = .sending
            context.issue = nil
            question = .submitted(text, action.turnID)
            composer = ""
            committed = ""
            provisional = ""
            provisionalState = nil
            issue = nil
            phase = .sending
            context.sendTask = Task { [self, context, action] in
                if !context.retired, permitsPresentation(context) {
                    let result = await callback { completion in try self.store.sendAssistant(action, completion: completion) }
                    switch result {
                    case .success(.accepted(let chatID, let turnID)):
                        if chatID == chat.opened.chatID, turnID == action.turnID { context.phase = .accepted }
                        else { context.phase = .unknown; context.issue = .invalidResponse }
                    case .success(.unavailable):
                        context.phase = .unavailable
                        context.issue = nil
                    case .failure(let error): context.phase = .unknown; context.issue = error
                    }
                    if permitsPresentation(context) { phase = context.phase; issue = context.issue }
                } else if !context.retired {
                    context.phase = .unknown
                    context.issue = .inactive
                }
                context.sendTask = nil
                if current === context, !context.retired { requestRefresh() }
                releaseIfSettled(context)
            }
        } catch { issue = sanitize(error) }
    }

    func refresh() { requestRefresh() }

    func inspectOriginalOpen() {
        guard canInspectOpen, let context = current else { return }
        let captured = publication
        context.cardTask = Task { [self, context] in
            let result = await callback { completion in try self.store.assistantOpenStatus(context.action, completion: completion) }
            switch result {
            case .success(let state): accept(state, context: context)
            case .failure(let error): context.issue = error
            }
            if mayPublish(context, captured: captured) { issue = context.issue; phase = context.phase }
            context.cardTask = nil
            releaseIfSettled(context)
        }
    }

    func recoverOpenedChild() {
        guard canRecoverOpenedChild, let context = current else { return }
        startOpen(context)
    }

    func cancel() { retireCurrent() }
    func clear() { retireCurrent() }

    func inspect(_ card: Card) {
        guard let context = cardContext(card), context.cardTask == nil else { return }
        let captured = publication
        card.isWorking = true
        card.issue = nil
        context.cardTask = Task { [self, context, card] in
            let lookup = await callback { completion in try self.store.inspectAssistantOperation(card.original, completion: completion) }
            if mayPublish(context, captured: captured) {
                switch lookup {
                case .success(.notRecorded): card.issue = .operationUncertain; card.receipt = nil
                case .success(.inspected(let operation)):
                    card.inspection = operation
                    let reference: NativeActionReference?
                    switch operation.phase {
                    case .awaitingConfirmation(let value): reference = .confirmation(reference: value)
                    case .awaitingAction(let value): reference = .action(reference: value)
                    default: reference = nil
                    }
                    if let reference {
                        let details = await callback { completion in
                            try self.store.assistantActionDetails(card.original, reference: reference, completion: completion)
                        }
                        if mayPublish(context, captured: captured) {
                            switch details {
                            case .success(let receipt): card.receipt = receipt; card.target = receipt.details.target
                            case .failure(let error): card.receipt = nil; card.issue = error
                            }
                        }
                    } else { card.receipt = nil }
                case .failure(let error): card.issue = error
                }
            }
            card.isWorking = false
            context.cardTask = nil
            releaseIfSettled(context)
        }
    }

    func nativeOperation(_ card: Card) -> NativeSettingsOperation? {
        guard cardContext(card) != nil, let target = card.target else { return nil }
        return store.operations.first { $0.id == card.id && $0.target == target }
    }

    func canConfirm(_ card: Card) -> Bool {
        guard let operation = nativeOperation(card), case .patch = operation.target else { return false }
        return store.canAct && operation.confirmation != nil && operation.issue == nil
            && operation.target.expectedRevision == store.snapshot?.revision
    }

    func confirm(_ card: Card) {
        guard canConfirm(card) else { return }
        store.confirm(card.id)
    }

    func inspectNative(_ card: Card) {
        guard nativeOperation(card) != nil else { return }
        store.inspect(card.id)
    }

    func reconcileNative(_ card: Card) {
        guard nativeOperation(card)?.canReconcile == true else { return }
        store.reconcile(card.id)
    }

    func cancelNative(_ card: Card) {
        guard nativeOperation(card)?.blocksNewChanges == true else { return }
        store.cancel(card.id)
    }

    func presentDevice(_ card: Card) {
        guard let context = cardContext(card), let receipt = card.receipt,
              context.cardTask == nil, case .device = receipt.details.target else { return }
        context.deviceHandoff = true
        pausePublication()
        do {
            context.ownDevice = try store.presentAssistantDevice(receipt, parent: window)
            context.deviceHandoff = false
        } catch {
            context.deviceHandoff = false
            context.issue = sanitize(error)
            issue = context.issue
            resumePublication()
        }
    }

    func presentSection(_ card: Card) {
        guard let navigation, let context = cardContext(card), let receipt = card.receipt,
              context.cardTask == nil, case .native(let request) = receipt.details.target,
              case .openSection = request.action else { return }
        context.navigating = true
        pausePublication()
        let completion = Completion<NativeSettingsSession.Outcome>()
        do {
            try store.presentAssistantSection(receipt, navigate: { [self, context, navigation] section in
                guard permitsPresentation(context), childIsLive(context) else { return .dismissed }
                let outcome = try await navigation.open(section)
                guard permitsPresentation(context), childIsLive(context) else { throw NativeSettingsSessionError.operationUncertain }
                return outcome
            }, completion: { completion.finish($0) })
            context.cardTask = Task { [self, context, completion] in
                let result = await completion.value()
                context.navigating = false
                if case .failure(let error) = result { context.issue = error }
                context.cardTask = nil
                if current === context, !context.retired { resumePublication() }
                releaseIfSettled(context)
            }
        } catch {
            context.navigating = false
            context.issue = sanitize(error)
            resumePublication()
        }
    }

    func canRetryOriginalClose(_ id: UUID) -> Bool {
        guard !isInvalidated, let context = contexts[id] else { return false }
        return context.retired && context.closeTask == nil && !context.knownClosed
            && store.isCurrentAssistantOwner(context.action)
    }

    func retryOriginalClose(_ id: UUID) {
        guard canRetryOriginalClose(id), let context = contexts[id] else { return }
        startClose(context, explicit: true)
    }

    func reconcileCleanup() {
        for context in Array(contexts.values) {
            if let observation = try? store.assistantCleanup(context.action) {
                context.parentRetired = observation.parentRetired
                if let state = observation.openState { accept(state, context: context) }
            }
            releaseIfSettled(context)
        }
    }

    func invalidate() {
        guard !isInvalidated else { return }
        isInvalidated = true
        expanded = false
        surfaceVisible = false
        applicationActive = false
        pausePublication()
        navigation?.retire()
        for context in contexts.values { retire(context) }
    }

    func drain() async -> [Cleanup] {
        if let drainTask { return await drainTask.value }
        invalidate()
        let pending = contexts.values.flatMap { [$0.openTask, $0.sendTask, $0.cardTask, $0.closeTask] }.compactMap { $0 }
        let polling = pollTask
        let task = Task { [self, pending, polling] in
            if let polling { await polling.value }
            for owned in pending { await owned.value }
            reconcileCleanup()
            return cleanup
        }
        drainTask = task
        return await task.value
    }

    private var surfaceEligible: Bool {
        guard !isInvalidated, expanded, surfaceVisible, applicationActive, !covered, windowAvailable,
              NSApplication.shared.isActive, !NSApplication.shared.isHidden, let window else { return false }
        return window.isVisible && !window.isMiniaturized && window.occlusionState.contains(.visible)
            && window.attachedSheet == nil && window.sheets.isEmpty
    }

    private func permitsPresentation(_ context: Context) -> Bool {
        !context.retired && current === context && context.connectionKey == connectionKey && surfaceEligible
            && store.sessionPresent && store.isCurrentAssistantOwner(context.action)
    }

    private func mayPublish(_ context: Context, captured: UUID) -> Bool {
        captured == publication && permitsPresentation(context) && childIsLive(context) && !context.deviceHandoff
            && context.ownDevice == nil && !context.navigating
    }

    private func childIsLive(_ context: Context) -> Bool {
        guard let chat = context.chat else { return true }
        return (try? chat.validateLifetime()) != nil
    }

    private func cardContext(_ card: Card) -> Context? {
        guard canPublish, let current, current.id == card.contextID, current.cards[card.id] === card,
              let chat = current.chat, (try? chat.validateLifetime()) != nil else { return nil }
        return current
    }

    private func startOpen(_ context: Context) {
        guard context.openTask == nil, !context.retired else { return }
        let captured = publication
        context.openTask = Task { [self, context] in
            if permitsPresentation(context) {
                let result = await callback { completion in try self.store.openAssistant(context.action, completion: completion) }
                switch result {
                case .success(.opened(let chat)):
                    context.chat = chat
                    context.knownChatID = chat.opened.chatID
                    context.phase = .ready
                    context.issue = nil
                case .success(.state(let state)): accept(state, context: context)
                case .success(.unavailable): accept(.unavailable, context: context)
                case .failure(let error): context.issue = error; context.phase = .unknown
                }
                if mayPublish(context, captured: captured) { phase = context.phase; issue = context.issue }
            } else if !context.retired {
                context.phase = .unknown
                context.issue = .inactive
            }
            context.openTask = nil
            if current === context, !context.retired { requestRefresh() }
            releaseIfSettled(context)
        }
    }

    private func requestRefresh() {
        guard canPublish, current?.chat != nil else { return }
        pollRequested = true
        guard pollTask == nil, let context = current else { return }
        let captured = publication
        pollContextID = context.id
        pollTask = Task { [self, context] in
            while !Task.isCancelled, captured == publication, permitsPresentation(context),
                  !context.deviceHandoff, context.ownDevice == nil, !context.navigating, let chat = context.chat {
                pollRequested = false
                guard childIsLive(context) else {
                    context.issue = .expired
                    retireCurrent()
                    break
                }
                if context.cardTask == nil {
                    let result = await callback { completion in try self.store.pollAssistant(chat, completion: completion) }
                    guard !Task.isCancelled, captured == publication, permitsPresentation(context),
                          !context.deviceHandoff, context.ownDevice == nil, !context.navigating else { break }
                    guard childIsLive(context) else {
                        context.issue = .expired
                        retireCurrent()
                        break
                    }
                    switch result {
                    case .success(let observation): receive(observation, context: context)
                    case .failure(let error): context.issue = error; context.phase = .unknown; issue = error; phase = .unknown
                    }
                    if case .failure = result { break }
                }
                do { try await Task.sleep(for: .seconds(1)) } catch { break }
            }
            pollTask = nil
            pollContextID = nil
            releaseIfSettled(context)
            if pollRequested, canPublish { requestRefresh() }
        }
    }

    private func receive(_ observation: NativeSettingsSession.AssistantObservation, context: Context) {
        let status = observation.status
        guard let chat = context.chat, chat.opened.chatID == status.chatID,
              observation.operations.count <= 8,
              Set(observation.operations.map(\.operationID)) == Set(status.operationIDs),
              observation.operations.allSatisfy({ $0.chat === chat && $0.turnID == status.turnID && context.turns[$0.turnID] != nil }),
              status.turnID.map({ context.turns[$0] != nil }) ?? true else {
            context.issue = .invalidResponse
            context.phase = .unknown
            issue = .invalidResponse
            phase = .unknown
            return
        }
        if let pending = context.pendingTurn, status.turnID != pending { return }
        for original in observation.operations {
            if let card = context.cards[original.operationID] {
                guard card.original === original else { issue = .invalidResponse; return }
            } else { context.cards[original.operationID] = Card(original: original, contextID: context.id) }
        }
        cards = context.cards.values.sorted { $0.id.uuidString < $1.id.uuidString }
        committed = status.assistantText
        provisional = status.provisional?.text ?? ""
        provisionalState = status.provisional?.state
        summary = Summary(context: status.context, attempts: status.modelAttempts, chargedTokens: status.chargedTokens,
            reservedOutputTokens: status.reservedOutputTokens, providerComplete: status.providerComplete, usageComplete: status.usageComplete)
        switch status.phase {
        case .ready: context.phase = .ready
        case .running: context.phase = .running
        case .completed:
            context.phase = .completed
            if let turn = status.turnID, context.turns[turn] != nil {
                if context.pendingTurn == turn { context.pendingTurn = nil }
                if !context.archivedTurns.contains(turn), case .submitted(let text, let originalTurn) = question, originalTurn == turn {
                    recent.append(RecentTurn(id: turn, question: text, answer: status.assistantText))
                    if recent.count > 4 { recent.removeFirst(recent.count - 4) }
                    context.archivedTurns.insert(turn)
                    question = .none
                }
            }
        case .closing: context.phase = .closing
        case .closed: context.phase = .closed
        case .unavailable: context.phase = .unavailable
        case .stoppedUncertain: context.phase = .stoppedUncertain
        case .stoppedUnusable: context.phase = .stoppedUnusable
        case .unknown: context.phase = .unknown
        }
        context.issue = nil
        issue = nil
        phase = context.phase
    }

    private func pausePublication() {
        publication = UUID()
        pollRequested = false
        pollTask?.cancel()
        clearPrivateCells()
        if current != nil { phase = .paused }
    }

    private func reconcileDevicePresentation() {
        guard !covered, let current, !current.retired, !current.deviceHandoff, let handle = current.ownDevice,
              store.deviceOperations.first(where: { $0.handle === handle })?.retired == true,
              window?.attachedSheet == nil, window?.sheets.isEmpty == true else { return }
        current.ownDevice = nil
    }

    private func resumePublication() {
        guard let current, permitsPresentation(current), current.ownDevice == nil, !current.deviceHandoff else { return }
        guard childIsLive(current) else {
            current.issue = .expired
            retireCurrent()
            return
        }
        phase = current.phase
        issue = current.issue
        requestRefresh()
    }

    private func retireCurrent() {
        pausePublication()
        navigation?.retire()
        guard let current else { return }
        retire(current)
    }

    private func retire(_ context: Context) {
        context.retired = true
        context.phase = .closing
        if current === context { phase = .closing; navigation?.retire() }
        if let handle = context.ownDevice { store.dismissDeviceCeremony(handle) }
        startClose(context, explicit: false)
    }

    private func startClose(_ context: Context, explicit: Bool) {
        guard !context.knownClosed, context.closeTask == nil, explicit || !context.closeAttempted else { return }
        context.closeAttempted = true
        let opening = context.openTask
        let sending = context.sendTask
        let reading = context.cardTask
        let polling = pollContextID == context.id ? pollTask : nil
        context.closeTask = Task { [self, context, opening, sending, reading, polling] in
            if let opening { await opening.value }
            if let sending { await sending.value }
            if let reading { await reading.value }
            if let polling { await polling.value }
            if store.isCurrentAssistantOwner(context.action) {
                let result = await callback { completion in try self.store.closeAssistantOpen(context.action, completion: completion) }
                switch result {
                case .success(let state): accept(state, context: context)
                case .failure(let error): context.issue = error; context.phase = .unknown
                }
            } else {
                context.parentRetired = true
                context.issue = .operationUncertain
                if let observation = try? store.assistantCleanup(context.action), let state = observation.openState { accept(state, context: context) }
            }
            context.closeTask = nil
            if current === context { phase = context.phase; issue = context.issue }
            releaseIfSettled(context)
        }
    }

    private func accept(_ state: NativeSettingsAssistantWire.OpenState, context: Context) {
        switch state {
        case .ready(let opened):
            if let known = context.knownChatID, known != opened.chatID { context.issue = .invalidResponse; return }
            context.knownChatID = opened.chatID
            context.phase = context.retired ? .closing : .ready
        case .closed(let id):
            guard context.knownChatID == nil || context.knownChatID == id else { context.issue = .invalidResponse; return }
            context.knownClosed = true
            context.phase = .closed
            context.issue = nil
        case .unavailable: context.knownClosed = true; context.phase = .unavailable; context.issue = nil
        case .closing(let id):
            if let known = context.knownChatID, known != id { context.issue = .invalidResponse; return }
            context.knownChatID = id
            context.phase = .closing
        case .unknown(let id?):
            if let known = context.knownChatID, known != id { context.issue = .invalidResponse; return }
            context.knownChatID = id
            context.phase = .unknown
            context.issue = .operationUncertain
        case .notAccepted, .unknown(nil): context.phase = .unknown; context.issue = .operationUncertain
        }
    }

    private func releaseIfSettled(_ context: Context) {
        guard context.knownClosed, context.openTask == nil, context.sendTask == nil,
              context.cardTask == nil, context.closeTask == nil, pollContextID != context.id else { return }
        contexts.removeValue(forKey: context.id)
        if current === context { current = nil; clearPrivateCells(); phase = context.phase }
    }

    private func clearPrivateCells() {
        composer = ""
        question = .none
        recent = []
        committed = ""
        provisional = ""
        provisionalState = nil
        cards = []
        summary = nil
        inputIssue = nil
    }

    private func callback<Value: Sendable>(_ start: (@escaping @MainActor @Sendable (Result<Value, NativeSettingsSessionError>) -> Void) throws -> Void) async -> Result<Value, NativeSettingsSessionError> {
        await withCheckedContinuation { continuation in
            do { try start { continuation.resume(returning: $0) } }
            catch { continuation.resume(returning: .failure(sanitize(error))) }
        }
    }

    private func sanitize(_ error: any Error) -> NativeSettingsSessionError {
        error as? NativeSettingsSessionError ?? .operationUncertain
    }
}

import AppKit
import Foundation
import Observation

@MainActor
@Observable
final class NativeSettingsStore {
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
        let lifecycle: NativeSettingsLifecycle
        var information: NativeSettingsSession.Information?
        var work: Task<Void, Never>?
        var retirement: Task<Cleanup, Never>?
        var report: NativeSettingsSession.DrainReport?

        init(lifecycle: NativeSettingsLifecycle, connectionKey: UUID) {
            self.lifecycle = lifecycle
            self.connectionKey = connectionKey
        }
    }

    private(set) var information: NativeSettingsSession.Information?
    private(set) var registry: SettingRegistry?
    private(set) var snapshot: SettingsSnapshot?
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
    }

    func setApplicationActive(_ active: Bool) {
        guard !isClosed else { return }
        if applicationActive != active { invalidateObservation() }
        applicationActive = active
        current?.lifecycle.setApplicationActive(active)
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
            let owner = Owner(lifecycle: lifecycle, connectionKey: connectionKey)
            clearPresentation()
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
            guard isCurrent(owner) else { owner.work = nil; return }
            if let previous = self.snapshot, snapshot.revision < previous.revision {
                throw NativeSettingsSessionError.invalidResponse
            }
            self.registry = registry
            self.snapshot = snapshot
            observedAt = Date()
            observationIsStale = generation != observationGeneration || !isForeground || snapshot.stale
            issue = nil
        } catch {
            if isCurrent(owner) {
                issue = sanitize(error)
                observationIsStale = true
            }
        }
        owner.work = nil
        if isCurrent(owner) { isOpening = false; isRefreshing = false }
    }

    private func isCurrent(_ owner: Owner) -> Bool {
        !isClosed && currentID == owner.id && connectionKey == owner.connectionKey
    }

    private func retireCurrent() {
        guard let owner = current else { return }
        owner.lifecycle.invalidate()
        currentID = nil
        sessionPresent = false
        isOpening = false
        isRefreshing = false
        invalidateObservation()
        let work = owner.work
        owner.retirement = Task { [self, owner, work] in
            if let work { await work.value }
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

    private func clearPresentation() {
        information = nil
        registry = nil
        snapshot = nil
        issue = nil
        observedAt = nil
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
}

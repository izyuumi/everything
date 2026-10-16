import Foundation

#if os(iOS)
import UIKit
#elseif os(macOS)
import AppKit
#endif

enum DeviceNotificationActionError: Error, Equatable, Sendable {
    case inactive
    case expired
    case invalidRequest
}

@MainActor
final class DeviceForegroundSession {
    let id = UUID()
    let deviceID: UUID
    private var generation: UInt64 = 1
    private var foreground = true
    private var closed = false

    init(deviceID: UUID) throws {
        guard deviceID != UUID(uuid: (0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0)) else {
            throw DeviceNotificationActionError.invalidRequest
        }
        self.deviceID = deviceID
    }

    func setForeground(_ active: Bool) {
        guard !closed else { return }
        if foreground != active {
            if generation == UInt64.max { closed = true }
            else { generation += 1 }
        }
        foreground = active
    }

    func close() {
        closed = true
        foreground = false
    }

    fileprivate func capture() throws -> UInt64 {
        guard foreground, !closed, applicationIsActive else { throw DeviceNotificationActionError.inactive }
        return generation
    }

    func validate(generation: UInt64, expiresAt: Date, deadline: ContinuousClock.Instant) throws {
        guard Date() < expiresAt, ContinuousClock.now < deadline else { throw DeviceNotificationActionError.expired }
        guard !closed, foreground, applicationIsActive, self.generation == generation else { throw DeviceNotificationActionError.inactive }
    }

    private var applicationIsActive: Bool {
        #if os(iOS)
        UIApplication.shared.applicationState == .active
        #elseif os(macOS)
        NSApplication.shared.isActive
        #else
        false
        #endif
    }
}

struct DeviceNotificationActionResult: Equatable, Sendable {
    let requestID: UUID
    let deviceID: UUID
    let outcome: Outcome

    enum Outcome: Equatable, Sendable {
        case completed(receiptID: UUID, observation: DeviceNotificationObservation, guidance: DeviceNotificationGuidance)
        case conflict(observedRevision: UInt64)
        case cancelled
        case expired
        case failed(observation: DeviceNotificationObservation?)
        case unavailable(observation: DeviceNotificationObservation)
        case capacity
        case requestReused
    }
}

@MainActor
final class DeviceNotificationActions {
    @MainActor
    final class Admission {
        fileprivate let requestID: UUID
        fileprivate let deviceID: UUID

        fileprivate init(requestID: UUID, deviceID: UUID) {
            self.requestID = requestID
            self.deviceID = deviceID
        }
    }

    enum PreparationError: Error, Equatable, Sendable {
        case capacity
        case requestReused
    }

    private struct Identity: Equatable {
        let deviceID: UUID
        let sessionID: UUID
        let expectedRevision: UInt64
        let expiresAt: Date
    }

    private struct Invocation {
        let identity: Identity
        let admission: Admission
        let initialAdmission: InitialAdmission
        let session: DeviceForegroundSession
        let preferences: DevicePreferenceStore
        let cancellation: InvocationCancellation
        var task: Task<DeviceNotificationActionResult, Never>?
    }

    private final class InvocationCancellation {
        var requested = false
    }

    private enum InitialAdmission {
        case eligible(generation: UInt64, deadline: ContinuousClock.Instant)
        case rejected(DeviceNotificationActionError)
    }

    private let authorization: NotificationAuthorizationService
    private var invocations: [UUID: Invocation] = [:]
    private var prompting: UUID?
    private static let maximumInvocations = 256

    init(authorization: NotificationAuthorizationService) {
        self.authorization = authorization
    }

    func prepare(requestID: UUID, expectedRevision: UInt64, expiresAt: Date, session: DeviceForegroundSession, preferences: DevicePreferenceStore) throws -> Admission {
        let identity = Identity(deviceID: session.deviceID, sessionID: session.id, expectedRevision: expectedRevision, expiresAt: expiresAt)
        if let previous = invocations[requestID] {
            guard previous.identity == identity, previous.session === session, previous.preferences === preferences else {
                throw PreparationError.requestReused
            }
            return previous.admission
        }
        guard invocations.count < Self.maximumInvocations else { throw PreparationError.capacity }
        let admission: InitialAdmission
        let lifetime = identity.expiresAt.timeIntervalSinceNow
        if requestID == UUID(uuid: (0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0))
            || identity.expectedRevision == 0 || identity.expectedRevision > UInt64(Int64.max) {
            admission = .rejected(.invalidRequest)
        } else if !lifetime.isFinite || lifetime <= 0 || lifetime > 300 {
            admission = .rejected(.expired)
        } else {
            do { admission = .eligible(generation: try session.capture(), deadline: ContinuousClock.now.advanced(by: .seconds(lifetime))) }
            catch { admission = .rejected(.inactive) }
        }
        let cancellation = InvocationCancellation()
        cancellation.requested = Task.isCancelled
        let ticket = Admission(requestID: requestID, deviceID: session.deviceID)
        invocations[requestID] = Invocation(identity: identity, admission: ticket, initialAdmission: admission, session: session, preferences: preferences, cancellation: cancellation, task: nil)
        return ticket
    }

    func authorize(_ admission: Admission) async -> DeviceNotificationActionResult {
        let requestID = admission.requestID
        guard let invocation = invocations[requestID], invocation.admission === admission else {
            return result(requestID, admission.deviceID, .requestReused)
        }
        if Task.isCancelled { invocation.cancellation.requested = true }
        if let task = invocation.task { return await wait(for: task, requestID: requestID) }
        let task = Task { @MainActor in
            await self.perform(requestID: requestID, identity: invocation.identity, admission: invocation.initialAdmission, cancellation: invocation.cancellation, session: invocation.session, preferences: invocation.preferences)
        }
        invocations[requestID]?.task = task
        return await wait(for: task, requestID: requestID)
    }

    func cancel(requestID: UUID) {
        invocations[requestID]?.cancellation.requested = true
    }

    private func wait(for task: Task<DeviceNotificationActionResult, Never>, requestID: UUID) async -> DeviceNotificationActionResult {
        await withTaskCancellationHandler {
            await task.value
        } onCancel: {
            Task { @MainActor in self.cancel(requestID: requestID) }
        }
    }

    private func perform(requestID: UUID, identity: Identity, admission: InitialAdmission, cancellation: InvocationCancellation, session: DeviceForegroundSession, preferences: DevicePreferenceStore) async -> DeviceNotificationActionResult {
        do {
            guard !cancellation.requested else { return result(requestID, identity.deviceID, .cancelled) }
            let generation: UInt64
            let deadline: ContinuousClock.Instant
            switch admission {
            case .eligible(let originalGeneration, let originalDeadline):
                generation = originalGeneration
                deadline = originalDeadline
            case .rejected(.invalidRequest): return result(requestID, identity.deviceID, .failed(observation: nil))
            case .rejected(.expired): return result(requestID, identity.deviceID, .expired)
            case .rejected(.inactive): return result(requestID, identity.deviceID, .cancelled)
            }
            try session.validate(generation: generation, expiresAt: identity.expiresAt, deadline: deadline)
            let current = try await preferences.snapshot()
            guard current.deviceID == identity.deviceID else { return result(requestID, identity.deviceID, .failed(observation: nil)) }
            guard current.revision == identity.expectedRevision else { return result(requestID, identity.deviceID, .conflict(observedRevision: current.revision)) }
            try session.validate(generation: generation, expiresAt: identity.expiresAt, deadline: deadline)
            guard !cancellation.requested else { return result(requestID, identity.deviceID, .cancelled) }
            let currentAuthorization = await authorization.observe()
            let latest = try await preferences.snapshot()
            guard latest.deviceID == identity.deviceID else { return result(requestID, identity.deviceID, .failed(observation: nil)) }
            guard latest.revision == identity.expectedRevision else { return result(requestID, identity.deviceID, .conflict(observedRevision: latest.revision)) }
            try session.validate(generation: generation, expiresAt: identity.expiresAt, deadline: deadline)
            guard !cancellation.requested else { return result(requestID, identity.deviceID, .cancelled) }
            switch currentAuthorization.authorization {
            case .notDetermined:
                guard prompting == nil else { return result(requestID, identity.deviceID, .cancelled) }
                prompting = requestID
                defer { prompting = nil }
                do {
                    let observation = try await authorization.request(session: session, generation: generation, expiresAt: identity.expiresAt, deadline: deadline)
                    return result(requestID, identity.deviceID, .completed(receiptID: UUID(), observation: observation, guidance: .none))
                } catch let error as DeviceNotificationActionError {
                    return result(requestID, identity.deviceID, error == .expired ? .expired : .cancelled)
                } catch {
                    return result(requestID, identity.deviceID, .failed(observation: await authorization.observe()))
                }
            case .denied:
                let guidance = try await authorization.openSystemSettings(session: session, generation: generation, expiresAt: identity.expiresAt, deadline: deadline)
                let observation = await authorization.observe()
                return result(requestID, identity.deviceID, .completed(receiptID: UUID(), observation: observation, guidance: guidance))
            case .unavailable:
                return result(requestID, identity.deviceID, .unavailable(observation: currentAuthorization))
            default:
                return result(requestID, identity.deviceID, .completed(receiptID: UUID(), observation: currentAuthorization, guidance: .none))
            }
        } catch let error as DeviceNotificationActionError {
            return result(requestID, identity.deviceID, error == .expired ? .expired : .cancelled)
        } catch {
            return result(requestID, identity.deviceID, .failed(observation: nil))
        }
    }

    private func result(_ requestID: UUID, _ deviceID: UUID, _ outcome: DeviceNotificationActionResult.Outcome) -> DeviceNotificationActionResult {
        DeviceNotificationActionResult(requestID: requestID, deviceID: deviceID, outcome: outcome)
    }
}

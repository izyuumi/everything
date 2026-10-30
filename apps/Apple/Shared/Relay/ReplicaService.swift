import Foundation
import os

let settingsClockSkewMilliseconds: UInt64 = 5_000

struct PairingSummary: Sendable {
    let deviceID: UUID
    let fingerprint: String
    let expires: UInt64
}

struct ReplicaViewState: Sendable {
    let snapshot: ReplicaSnapshot
    let identity: DevicePublicKeys
    let connection: String
    let settingsAccess: ReplicaSettingsAccessDisplayView?
    let settingsAccessRequest: ReplicaSettingsAccessRequestStatus?
    let settingsScope: ReplicaSettingsScopeToken?
    let settingsBinding: ReplicaSettingsBindingView?
    let settingsSnapshot: ReplicaSettingsSnapshotView?
    let settingsSnapshotRequest: ReplicaSettingsAccessRequestStatus?
    let settingsInspection: ReplicaSettingsInspectionView?
    let settingsInspectionRequest: ReplicaSettingsAccessRequestStatus?
}

final class ReplicaSettingsScopeToken: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    fileprivate init() {}

    var description: String { "ReplicaSettingsScopeToken([redacted])" }
    var debugDescription: String { description }
}

final class ReplicaSettingsAccessInput: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    fileprivate let service: ReplicaService
    fileprivate let scope: ReplicaSettingsScopeToken
    fileprivate let queryID: UUID
    fileprivate let query: RemoteSettingsQuery
    let wallFloor: UInt64
    let monotonicFloor: ContinuousClock.Instant
    let expiresAt: UInt64
    let deadline: ContinuousClock.Instant

    fileprivate init(service: ReplicaService, scope: ReplicaSettingsScopeToken, query: RemoteSettingsQuery,
                     wallFloor: UInt64, monotonicFloor: ContinuousClock.Instant, expiresAt: UInt64) {
        self.service = service
        self.scope = scope
        queryID = UUID()
        self.query = query
        self.wallFloor = wallFloor
        self.monotonicFloor = monotonicFloor
        self.expiresAt = expiresAt
        deadline = monotonicFloor.advanced(by: .milliseconds(Int64(expiresAt - wallFloor)))
    }

    var description: String { "ReplicaSettingsAccessInput([redacted])" }
    var debugDescription: String { description }
}

@MainActor
final class ReplicaSettingsAccessGesture {
    private var input: ReplicaSettingsAccessInput?

    init(service: ReplicaService, scope: ReplicaSettingsScopeToken, query: RemoteSettingsQuery = .accessStatus) throws {
        try query.validate()
        let monotonic = ContinuousClock.now
        let wall = try WireID.now()
        let expiry = wall.addingReportingOverflow(30_000)
        guard !expiry.overflow, expiry.partialValue <= UInt64(Int64.max) else { throw WireError.expired }
        input = ReplicaSettingsAccessInput(service: service, scope: scope, query: query, wallFloor: wall,
                                           monotonicFloor: monotonic, expiresAt: expiry.partialValue)
    }

    func take() throws -> ReplicaSettingsAccessInput {
        guard let captured = input else { throw WireError.conflict }
        input = nil
        return captured
    }
}

final class ReplicaSettingsBindInput: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    fileprivate let service: ReplicaService
    fileprivate let scope: ReplicaSettingsScopeToken
    fileprivate let accessHandle: ReplicaSettingsRequestHandle
    fileprivate let bindingID: UUID
    let wallFloor: UInt64
    let monotonicFloor: ContinuousClock.Instant
    let expiresAt: UInt64
    let parentMaximum: UInt64
    let deadline: ContinuousClock.Instant

    fileprivate init(service: ReplicaService, scope: ReplicaSettingsScopeToken,
                     accessHandle: ReplicaSettingsRequestHandle, wallFloor: UInt64,
                     monotonicFloor: ContinuousClock.Instant, expiresAt: UInt64, parentMaximum: UInt64) {
        self.service = service
        self.scope = scope
        self.accessHandle = accessHandle
        bindingID = UUID()
        self.wallFloor = wallFloor
        self.monotonicFloor = monotonicFloor
        self.expiresAt = expiresAt
        self.parentMaximum = parentMaximum
        deadline = monotonicFloor.advanced(by: .milliseconds(Int64(expiresAt - wallFloor)))
    }

    var description: String { "ReplicaSettingsBindInput([redacted])" }
    var debugDescription: String { description }
}

@MainActor
final class ReplicaSettingsBindGesture {
    private var input: ReplicaSettingsBindInput?

    init(service: ReplicaService, scope: ReplicaSettingsScopeToken, accessHandle: ReplicaSettingsRequestHandle) throws {
        let monotonic = ContinuousClock.now
        let wall = try WireID.now()
        let expiry = wall.addingReportingOverflow(30_000)
        let parent = wall.addingReportingOverflow(300_000)
        guard !expiry.overflow, !parent.overflow, parent.partialValue <= UInt64(Int64.max) else { throw WireError.expired }
        input = ReplicaSettingsBindInput(service: service, scope: scope, accessHandle: accessHandle,
                                        wallFloor: wall, monotonicFloor: monotonic,
                                        expiresAt: expiry.partialValue, parentMaximum: parent.partialValue)
    }

    func take() throws -> ReplicaSettingsBindInput {
        guard let captured = input else { throw WireError.conflict }
        input = nil
        return captured
    }
}

final class ReplicaSettingsSnapshotInput: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    private let admission = OSAllocatedUnfairLock(initialState: false)
    fileprivate let service: ReplicaService
    fileprivate let scope: ReplicaSettingsScopeToken
    fileprivate let bindingHandle: ReplicaSettingsRequestHandle
    fileprivate let queryID: UUID
    let wallFloor: UInt64
    let monotonicFloor: ContinuousClock.Instant
    let expiresAt: UInt64
    let deadline: ContinuousClock.Instant

    fileprivate init(service: ReplicaService, scope: ReplicaSettingsScopeToken,
                     bindingHandle: ReplicaSettingsRequestHandle, wallFloor: UInt64, monotonicFloor: ContinuousClock.Instant, expiresAt: UInt64) {
        self.service = service
        self.scope = scope
        self.bindingHandle = bindingHandle
        queryID = UUID()
        self.wallFloor = wallFloor
        self.monotonicFloor = monotonicFloor
        self.expiresAt = expiresAt
        deadline = monotonicFloor.advanced(by: .milliseconds(Int64(expiresAt - wallFloor)))
    }

    fileprivate func consumeAdmission() throws {
        try admission.withLock { consumed in
            guard !consumed else { throw WireError.conflict }
            consumed = true
        }
    }

    var description: String { "ReplicaSettingsSnapshotInput([redacted])" }
    var debugDescription: String { description }
}

final class ReplicaSettingsInspectionInput: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    private let admission = OSAllocatedUnfairLock(initialState: false)
    fileprivate let service: ReplicaService
    fileprivate let scope: ReplicaSettingsScopeToken
    fileprivate let bindingHandle: ReplicaSettingsRequestHandle
    fileprivate let queryID: UUID
    fileprivate let requestID: UUID
    fileprivate let operationID: UUID?
    let wallFloor: UInt64
    let monotonicFloor: ContinuousClock.Instant
    let expiresAt: UInt64
    let deadline: ContinuousClock.Instant

    fileprivate init(service: ReplicaService, scope: ReplicaSettingsScopeToken,
                     bindingHandle: ReplicaSettingsRequestHandle, requestID: UUID, operationID: UUID?, queryID: UUID,
                     wallFloor: UInt64, monotonicFloor: ContinuousClock.Instant, expiresAt: UInt64) {
        self.service = service
        self.scope = scope
        self.bindingHandle = bindingHandle
        self.queryID = queryID
        self.requestID = requestID
        self.operationID = operationID
        self.wallFloor = wallFloor
        self.monotonicFloor = monotonicFloor
        self.expiresAt = expiresAt
        deadline = monotonicFloor.advanced(by: .milliseconds(Int64(expiresAt - wallFloor)))
    }

    fileprivate func consumeAdmission() throws {
        try admission.withLock { consumed in
            guard !consumed else { throw WireError.conflict }
            consumed = true
        }
    }

    var description: String { "ReplicaSettingsInspectionInput([redacted])" }
    var debugDescription: String { description }
}

@MainActor
final class ReplicaSettingsSnapshotGesture {
    private var input: ReplicaSettingsSnapshotInput?

    init(service: ReplicaService, scope: ReplicaSettingsScopeToken, bindingHandle: ReplicaSettingsRequestHandle) throws {
        let monotonic = ContinuousClock.now
        let wall = try WireID.now()
        let expiry = wall.addingReportingOverflow(30_000)
        guard !expiry.overflow, expiry.partialValue <= UInt64(Int64.max) else { throw WireError.expired }
        input = ReplicaSettingsSnapshotInput(service: service, scope: scope, bindingHandle: bindingHandle, wallFloor: wall,
                                           monotonicFloor: monotonic, expiresAt: expiry.partialValue)
    }

    func take() throws -> ReplicaSettingsSnapshotInput {
        guard let captured = input else { throw WireError.conflict }
        input = nil
        return captured
    }
}

@MainActor
final class ReplicaSettingsInspectionGesture {
    private var input: ReplicaSettingsInspectionInput?

    init(service: ReplicaService, scope: ReplicaSettingsScopeToken, bindingHandle: ReplicaSettingsRequestHandle,
         requestID: UUID, operationID: UUID?) throws {
        let monotonic = ContinuousClock.now
        let wall = try WireID.now()
        let queryID = UUID()
        guard requestID != WireID.zero, operationID != WireID.zero, queryID != requestID else { throw WireError.invalid }
        let expiry = wall.addingReportingOverflow(30_000)
        guard !expiry.overflow, expiry.partialValue <= UInt64(Int64.max) else { throw WireError.expired }
        input = ReplicaSettingsInspectionInput(service: service, scope: scope, bindingHandle: bindingHandle,
                                               requestID: requestID, operationID: operationID, queryID: queryID,
                                               wallFloor: wall, monotonicFloor: monotonic, expiresAt: expiry.partialValue)
    }

    func take() throws -> ReplicaSettingsInspectionInput {
        guard let captured = input else { throw WireError.conflict }
        input = nil
        return captured
    }
}

enum ReplicaSettingsBindingStage: Sendable {
    case preparing
    case awaitingReply
    case settling
    case observed
    case retained

    var blocksNewRequest: Bool {
        switch self {
        case .preparing, .awaitingReply, .settling: true
        case .observed, .retained: false
        }
    }
}

struct ReplicaSettingsBindingView: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    let stage: ReplicaSettingsBindingStage
    let observedAt: UInt64?
    let expiresAt: UInt64?
    let freshness: ReplicaSettingsAccessDisplayFreshness
    let requestHandle: ReplicaSettingsRequestHandle

    fileprivate init(stage: ReplicaSettingsBindingStage, observedAt: UInt64?, expiresAt: UInt64?,
                     freshness: ReplicaSettingsAccessDisplayFreshness, requestHandle: ReplicaSettingsRequestHandle) {
        self.stage = stage
        self.observedAt = observedAt
        self.expiresAt = expiresAt
        self.freshness = freshness
        self.requestHandle = requestHandle
    }

    var description: String { "ReplicaSettingsBindingView([redacted])" }
    var debugDescription: String { description }
}

enum ReplicaSettingsAccessRequestStatus: Sendable, Equatable {
    case preparing
    case awaitingReply
    case settling
    case received
    case cancelled
    case expired
    case interrupted

    var blocksNewRequest: Bool {
        switch self {
        case .preparing, .awaitingReply, .settling: true
        case .received, .cancelled, .expired, .interrupted: false
        }
    }
}

enum ReplicaSettingsAccessDisplayFreshness: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    case currentConnectionObservation
    case stale

    var description: String { "ReplicaSettingsAccessDisplayFreshness([redacted])" }
    var debugDescription: String { description }
}

struct ReplicaSettingsAccessDisplayView: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    let display: ReplicaSettingsAccessDisplay
    let observedAt: UInt64
    let expiresAt: UInt64
    let freshness: ReplicaSettingsAccessDisplayFreshness
    let requestHandle: ReplicaSettingsRequestHandle
    let requestsAvailable: Bool

    fileprivate init(display: ReplicaSettingsAccessDisplay, observedAt: UInt64,
                     expiresAt: UInt64, freshness: ReplicaSettingsAccessDisplayFreshness,
                     requestHandle: ReplicaSettingsRequestHandle, requestsAvailable: Bool) {
        self.display = display
        self.observedAt = observedAt
        self.expiresAt = expiresAt
        self.freshness = freshness
        self.requestHandle = requestHandle
        self.requestsAvailable = requestsAvailable
    }

    var description: String { "ReplicaSettingsAccessDisplayView([redacted])" }
    var debugDescription: String { description }
}

struct ReplicaSettingsSnapshotView: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    let display: ReplicaSettingsSnapshotDisplay
    let observedAt: UInt64
    let expiresAt: UInt64
    let freshness: ReplicaSettingsAccessDisplayFreshness
    let requestHandle: ReplicaSettingsRequestHandle

    fileprivate init(display: ReplicaSettingsSnapshotDisplay, observedAt: UInt64,
                     expiresAt: UInt64, freshness: ReplicaSettingsAccessDisplayFreshness,
                     requestHandle: ReplicaSettingsRequestHandle) {
        self.display = display
        self.observedAt = observedAt
        self.expiresAt = expiresAt
        self.freshness = freshness
        self.requestHandle = requestHandle
    }

    var description: String { "ReplicaSettingsSnapshotView([redacted])" }
    var debugDescription: String { description }
}

struct ReplicaSettingsInspectionView: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    let display: ReplicaSettingsInspectionDisplay
    let observedAt: UInt64
    let expiresAt: UInt64
    let freshness: ReplicaSettingsAccessDisplayFreshness
    let requestHandle: ReplicaSettingsRequestHandle

    fileprivate init(display: ReplicaSettingsInspectionDisplay, observedAt: UInt64,
                     expiresAt: UInt64, freshness: ReplicaSettingsAccessDisplayFreshness,
                     requestHandle: ReplicaSettingsRequestHandle) {
        self.display = display
        self.observedAt = observedAt
        self.expiresAt = expiresAt
        self.freshness = freshness
        self.requestHandle = requestHandle
    }

    var description: String { "ReplicaSettingsInspectionView([redacted])" }
    var debugDescription: String { description }
}

final class ReplicaSettingsRequestHandle: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    fileprivate init() {}

    var description: String { "ReplicaSettingsRequestHandle([redacted])" }
    var debugDescription: String { description }
}

enum ReplicaSettingsQueryOutcome: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    case pending
    case accessObserved
    case received(ReplicaSettingsQueryResult, observedAt: UInt64)
    case ended(ReplicaSettingsAccessRequestStatus)

    var description: String { "ReplicaSettingsQueryOutcome([redacted])" }
    var debugDescription: String { description }
}

final class ReplicaSettingsAccessQualification: CustomStringConvertible, CustomDebugStringConvertible {
    fileprivate let attempt: ReplicaService.SettingsAccessReceiveAttempt
    fileprivate let committed: CommittedReplicaSettingsReply

    fileprivate init(attempt: ReplicaService.SettingsAccessReceiveAttempt,
                     committed: CommittedReplicaSettingsReply) {
        self.attempt = attempt
        self.committed = committed
    }

    func checkCurrent(for committed: CommittedReplicaSettingsReply,
                      owner: isolated ReplicaService) throws {
        try owner.checkSettingsAccessQualification(self, committed: committed)
    }

    func consumeForAccessProjection(for committed: CommittedReplicaSettingsReply,
                                    owner: isolated ReplicaService) throws {
        try owner.consumeSettingsAccessQualification(self, committed: committed)
    }

    var description: String { "ReplicaSettingsAccessQualification([redacted])" }
    var debugDescription: String { description }
}

final class ReplicaSettingsSnapshotQualification: CustomStringConvertible, CustomDebugStringConvertible {
    fileprivate let attempt: ReplicaService.SettingsSnapshotReceiveAttempt
    fileprivate let committed: CommittedReplicaSettingsReply

    fileprivate init(attempt: ReplicaService.SettingsSnapshotReceiveAttempt,
                     committed: CommittedReplicaSettingsReply) {
        self.attempt = attempt
        self.committed = committed
    }

    func checkCurrent(for committed: CommittedReplicaSettingsReply,
                      owner: isolated ReplicaService) throws {
        try owner.checkSettingsSnapshotQualification(self, committed: committed)
    }

    func consumeForSnapshotProjection(for committed: CommittedReplicaSettingsReply,
                                    owner: isolated ReplicaService) throws {
        try owner.consumeSettingsSnapshotQualification(self, committed: committed)
    }

    var description: String { "ReplicaSettingsSnapshotQualification([redacted])" }
    var debugDescription: String { description }
}

final class ReplicaSettingsInspectionQualification: CustomStringConvertible, CustomDebugStringConvertible {
    fileprivate let attempt: ReplicaService.SettingsInspectionReceiveAttempt
    fileprivate let committed: CommittedReplicaSettingsReply

    fileprivate init(attempt: ReplicaService.SettingsInspectionReceiveAttempt,
                     committed: CommittedReplicaSettingsReply) {
        self.attempt = attempt
        self.committed = committed
    }

    func checkCurrent(for committed: CommittedReplicaSettingsReply,
                      owner: isolated ReplicaService) throws {
        try owner.checkSettingsInspectionQualification(self, committed: committed)
    }

    func consumeForInspectionProjection(for committed: CommittedReplicaSettingsReply,
                                    owner: isolated ReplicaService) throws {
        try owner.consumeSettingsInspectionQualification(self, committed: committed)
    }

    var description: String { "ReplicaSettingsInspectionQualification([redacted])" }
    var debugDescription: String { description }
}

struct ReplicaSettingsBindRequestInput: CustomStringConvertible, CustomDebugStringConvertible {
    let hostID: UUID
    let bindingRequestID: UUID
    let time: RemoteSettingsRequestTime
    let requestedParentExpiresAt: UInt64
    let grantExpiresAt: UInt64

    fileprivate init(hostID: UUID, bindingRequestID: UUID, time: RemoteSettingsRequestTime,
                     requestedParentExpiresAt: UInt64, grantExpiresAt: UInt64) {
        self.hostID = hostID
        self.bindingRequestID = bindingRequestID
        self.time = time
        self.requestedParentExpiresAt = requestedParentExpiresAt
        self.grantExpiresAt = grantExpiresAt
    }

    var description: String { "ReplicaSettingsBindRequestInput([redacted])" }
    var debugDescription: String { description }
}

struct ReplicaSettingsBindProjectionInput: CustomStringConvertible, CustomDebugStringConvertible {
    let request: RemoteSettingsBindRequest
    let requestMessageID: UUID
    let wallFloor: UInt64
    let grantExpiresAt: UInt64

    fileprivate init(request: RemoteSettingsBindRequest, requestMessageID: UUID,
                     wallFloor: UInt64, grantExpiresAt: UInt64) {
        self.request = request
        self.requestMessageID = requestMessageID
        self.wallFloor = wallFloor
        self.grantExpiresAt = grantExpiresAt
    }

    var description: String { "ReplicaSettingsBindProjectionInput([redacted])" }
    var debugDescription: String { description }
}

final class ReplicaSettingsBindQualification: CustomStringConvertible, CustomDebugStringConvertible {
    fileprivate let attempt: ReplicaService.SettingsBindReceiveAttempt
    fileprivate let committed: CommittedReplicaSettingsReply

    fileprivate init(attempt: ReplicaService.SettingsBindReceiveAttempt,
                     committed: CommittedReplicaSettingsReply) {
        self.attempt = attempt
        self.committed = committed
    }

    func checkCurrent(for committed: CommittedReplicaSettingsReply,
                      owner: isolated ReplicaService) throws {
        try owner.checkSettingsBindQualification(self, committed: committed)
    }

    func consumeForBoundParent(for committed: CommittedReplicaSettingsReply,
                              owner: isolated ReplicaService) throws {
        try owner.consumeSettingsBindQualification(self, committed: committed)
    }

    var description: String { "ReplicaSettingsBindQualification([redacted])" }
    var debugDescription: String { description }
}

struct ReplicaSettingsSnapshotRequestInput: CustomStringConvertible, CustomDebugStringConvertible {
    let hostID: UUID
    let queryID: UUID
    let keys: [SettingKey]
    let time: RemoteSettingsRequestTime

    fileprivate init(hostID: UUID, queryID: UUID, keys: [SettingKey], time: RemoteSettingsRequestTime) {
        self.hostID = hostID
        self.queryID = queryID
        self.keys = keys
        self.time = time
    }

    var description: String { "ReplicaSettingsSnapshotRequestInput([redacted])" }
    var debugDescription: String { description }
}

struct ReplicaSettingsInspectionRequestInput: CustomStringConvertible, CustomDebugStringConvertible {
    let hostID: UUID
    let queryID: UUID
    let requestID: UUID
    let operationID: UUID?
    let time: RemoteSettingsRequestTime

    fileprivate init(hostID: UUID, queryID: UUID, requestID: UUID, operationID: UUID?, time: RemoteSettingsRequestTime) {
        self.hostID = hostID
        self.queryID = queryID
        self.requestID = requestID
        self.operationID = operationID
        self.time = time
    }

    var description: String { "ReplicaSettingsInspectionRequestInput([redacted])" }
    var debugDescription: String { description }
}

final class ReplicaSettingsUpdateInput: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    private let admission = OSAllocatedUnfairLock(initialState: false)
    fileprivate let service: ReplicaService
    fileprivate let scope: ReplicaSettingsScopeToken
    fileprivate let bindingHandle: ReplicaSettingsRequestHandle
    fileprivate let requestID: UUID
    fileprivate let expectedSettingsRevision: UInt64
    fileprivate let enabled: Bool?
    fileprivate let dimensions: RemoteSettingsUpdateDimensions?
    let wallFloor: UInt64
    let monotonicFloor: ContinuousClock.Instant
    let expiresAt: UInt64
    let deadline: ContinuousClock.Instant

    fileprivate init(service: ReplicaService, scope: ReplicaSettingsScopeToken,
                     bindingHandle: ReplicaSettingsRequestHandle, requestID: UUID,
                     expectedSettingsRevision: UInt64, enabled: Bool?, dimensions: RemoteSettingsUpdateDimensions?,
                     wallFloor: UInt64, monotonicFloor: ContinuousClock.Instant, expiresAt: UInt64) {
        self.service = service
        self.scope = scope
        self.bindingHandle = bindingHandle
        self.requestID = requestID
        self.expectedSettingsRevision = expectedSettingsRevision
        self.enabled = enabled
        self.dimensions = dimensions
        self.wallFloor = wallFloor
        self.monotonicFloor = monotonicFloor
        self.expiresAt = expiresAt
        deadline = monotonicFloor.advanced(by: .milliseconds(Int64(expiresAt - wallFloor)))
    }

    fileprivate func consumeAdmission() throws {
        try admission.withLock { consumed in
            guard !consumed else { throw WireError.conflict }
            consumed = true
        }
    }

    var description: String { "ReplicaSettingsUpdateInput([redacted])" }
    var debugDescription: String { description }
}

@MainActor
final class ReplicaSettingsUpdateGesture {
    private var input: ReplicaSettingsUpdateInput?

    init(service: ReplicaService, scope: ReplicaSettingsScopeToken, bindingHandle: ReplicaSettingsRequestHandle,
         expectedSettingsRevision: UInt64, enabled: Bool?, dimensions: RemoteSettingsUpdateDimensions?) throws {
        let monotonic = ContinuousClock.now
        let wall = try WireID.now()
        let requestID = UUID()
        guard requestID != WireID.zero, expectedSettingsRevision > 0,
              expectedSettingsRevision <= UInt64(Int64.max), enabled != nil || dimensions != nil else { throw WireError.invalid }
        if case .some(.selected(let value)) = dimensions {
            guard value > 0, value <= 65_536 else { throw WireError.invalid }
        }
        let expiry = wall.addingReportingOverflow(30_000)
        guard wall > 0, !expiry.overflow, expiry.partialValue <= UInt64(Int64.max) else { throw WireError.expired }
        input = ReplicaSettingsUpdateInput(service: service, scope: scope, bindingHandle: bindingHandle,
                                           requestID: requestID, expectedSettingsRevision: expectedSettingsRevision,
                                           enabled: enabled, dimensions: dimensions, wallFloor: wall,
                                           monotonicFloor: monotonic, expiresAt: expiry.partialValue)
    }

    func take() throws -> ReplicaSettingsUpdateInput {
        guard let captured = input else { throw WireError.conflict }
        input = nil
        return captured
    }
}

struct ReplicaSettingsUpdateRequestInput: CustomStringConvertible, CustomDebugStringConvertible {
    let hostID: UUID
    let requestID: UUID
    let expectedSettingsRevision: UInt64
    let enabled: Bool?
    let dimensions: RemoteSettingsUpdateDimensions?
    let time: RemoteSettingsRequestTime

    fileprivate init(hostID: UUID, requestID: UUID,
                     expectedSettingsRevision: UInt64, enabled: Bool?, dimensions: RemoteSettingsUpdateDimensions?,
                     time: RemoteSettingsRequestTime) {
        self.hostID = hostID
        self.requestID = requestID
        self.expectedSettingsRevision = expectedSettingsRevision
        self.enabled = enabled
        self.dimensions = dimensions
        self.time = time
    }

    var description: String { "ReplicaSettingsUpdateRequestInput([redacted])" }
    var debugDescription: String { description }
}

struct ReplicaSettingsUpdateReplyExpectation: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    let request: RemoteSettingsUpdateRequest
    let encryptedRequestID: UUID

    fileprivate init(request: RemoteSettingsUpdateRequest, encryptedRequestID: UUID) {
        self.request = request
        self.encryptedRequestID = encryptedRequestID
    }

    var description: String { "ReplicaSettingsUpdateReplyExpectation([redacted])" }
    var debugDescription: String { description }
}

final class ReplicaSettingsUpdateQualification: CustomStringConvertible, CustomDebugStringConvertible {
    fileprivate let attempt: ReplicaService.SettingsUpdateReceiveAttempt
    fileprivate let committed: CommittedReplicaSettingsReply

    fileprivate init(attempt: ReplicaService.SettingsUpdateReceiveAttempt, committed: CommittedReplicaSettingsReply) {
        self.attempt = attempt
        self.committed = committed
    }

    func checkCurrent(for committed: CommittedReplicaSettingsReply, owner: isolated ReplicaService) throws {
        try owner.checkSettingsUpdateQualification(self, committed: committed)
    }

    func consumeForUpdateProjection(for committed: CommittedReplicaSettingsReply, owner: isolated ReplicaService) throws {
        try owner.consumeSettingsUpdateQualification(self, committed: committed)
    }

    var description: String { "ReplicaSettingsUpdateQualification([redacted])" }
    var debugDescription: String { description }
}

actor ReplicaService {
    private let store: ReplicaStore
    private let identity: DeviceIdentity
    private var runner: Task<Void, Never>?
    private var starting: UUID?
    private var stopping: (id: UUID, task: Task<Void, Never>)?
    private var disabling = false
    private var reconnectForCapacity = false
    private var connection: RelayConnection?
    private var epoch = UUID()
    private var status = "Disconnected"
    private var selectedConversation: UUID?
    private var preferSelectedSync = true
    private var syncPosition = 0
    private var notificationPosition = 0
    private var claimingNotifications = false
    private var pendingInvitation: PairingInvitation?
    private var pendingCompletion: PairingInvitation?
    private var pairingEpoch = UUID()
    private var processingInbox = false
    private var outbound: [OutboundID: OutboundSend] = [:]
    private var outboundSocket: RelayConnection?
    private var settingsAccessOutcome: ReplicaSettingsAccessRequestStatus?
    private var settingsAccessHandle: ReplicaSettingsRequestHandle?
    private var settingsQueryResultCache: (handle: ReplicaSettingsRequestHandle, result: ReplicaSettingsQueryResult, observedAt: UInt64)?
    private var settingsSnapshotOutcome: ReplicaSettingsAccessRequestStatus?
    private var settingsInspectionOutcome: ReplicaSettingsAccessRequestStatus?
    private var lastSettingsInput: SettingsInputCapture?
    private var abandonedSettingsMessages: Set<UUID> = []

    private enum SettingsInputCapture: CustomStringConvertible, CustomDebugStringConvertible {
        case access(ReplicaSettingsAccessInput)
        case snapshot(ReplicaSettingsSnapshotInput)
        case inspection(ReplicaSettingsInspectionInput)
        case update(ReplicaSettingsUpdateInput)

        var description: String { "SettingsInputCapture([redacted])" }
        var debugDescription: String { description }
    }

    private enum OutboundID: Hashable {
        case ordinary(UUID)
        case access(UUID)
        case bind(UUID)
        case snapshot(UUID)
        case inspection(UUID)
        case update(UUID)
    }

    private enum OutboundSend {
        case ordinary(OrdinarySend)
        case access(SettingsAccessSend)
        case bind(SettingsBindSend)
        case snapshot(SettingsSnapshotSend)
        case inspection(SettingsInspectionSend)
        case update(SettingsUpdateSend)
    }

    private final class OrdinarySend {
        var started: ContinuousClock.Instant?
        var sendReturned = false
        var accepted = false
        var failed = false
    }

    private final class SettingsAccessSend {
        let original: PendingSettingsAccess
        var envelope: EncryptedEnvelope?
        var sendStarted = false
        var sendReturned = false
        var accepted = false
        var replyReturned = false
        var failed = false

        init(original: PendingSettingsAccess) {
            self.original = original
        }
    }
    private final class SettingsBindSend {
        let original: PendingSettingsBind
        var envelope: EncryptedEnvelope?
        var sendStarted = false
        var sendReturned = false
        var accepted = false
        var replyReturned = false
        var failed = false

        init(original: PendingSettingsBind) {
            self.original = original
        }
    }
    private final class SettingsSnapshotSend {
        let original: PendingSettingsSnapshot
        var envelope: EncryptedEnvelope?
        var sendStarted = false
        var sendReturned = false
        var accepted = false
        var replyReturned = false
        var failed = false

        init(original: PendingSettingsSnapshot) {
            self.original = original
        }
    }

    private final class SettingsInspectionSend {
        let original: PendingSettingsInspection
        var envelope: EncryptedEnvelope?
        var sendStarted = false
        var sendReturned = false
        var accepted = false
        var replyReturned = false
        var failed = false

        init(original: PendingSettingsInspection) {
            self.original = original
        }
    }

    private final class SettingsUpdateSend {
        let original: PendingSettingsUpdate
        var envelope: EncryptedEnvelope?
        var sendStarted = false
        var sendReturned = false
        var accepted = false
        var replyReturned = false
        var failed = false

        init(original: PendingSettingsUpdate) { self.original = original }
    }

    fileprivate final class SettingsUpdateOriginalRecord: CustomStringConvertible, CustomDebugStringConvertible {
        weak var scope: SettingsConnectionScope?
        let host: DevicePublicKeys
        let localID: UUID
        let generation: RemoteSettingsGeneration
        let handle: ReplicaSettingsRequestHandle
        let input: ReplicaSettingsUpdateRequestInput
        let wallFloor: UInt64
        let monotonicFloor: ContinuousClock.Instant
        let deadline: ContinuousClock.Instant
        var request: RemoteSettingsUpdateRequest?
        var encryptedRequestID: UUID?
        var outcome: RemoteSettingsOriginalView?
        var status: ReplicaSettingsAccessRequestStatus?

        init(scope: SettingsConnectionScope, handle: ReplicaSettingsRequestHandle,
             generation: RemoteSettingsGeneration, input: ReplicaSettingsUpdateRequestInput, wallFloor: UInt64,
             monotonicFloor: ContinuousClock.Instant, deadline: ContinuousClock.Instant) {
            self.scope = scope
            host = scope.host
            localID = scope.localID
            self.generation = generation
            self.handle = handle
            self.input = input
            self.wallFloor = wallFloor
            self.monotonicFloor = monotonicFloor
            self.deadline = deadline
        }

        var description: String { "SettingsUpdateOriginalRecord([redacted])" }
        var debugDescription: String { description }
    }

    fileprivate final class PendingSettingsUpdate: CustomStringConvertible, CustomDebugStringConvertible {
        let handle: ReplicaSettingsRequestHandle
        let capture: ReplicaSettingsUpdateInput
        let scope: SettingsConnectionScope
        let association: SettingsBindingAssociation
        let input: ReplicaSettingsUpdateRequestInput
        let record: SettingsUpdateOriginalRecord
        let deadline: ContinuousClock.Instant
        var requestCaptured = false
        var phase = SettingsRequestPreparation.waiting
        var retired = false
        var consumed = false

        var request: RemoteSettingsUpdateRequest? { record.request }
        var wallFloor: UInt64 { capture.wallFloor }
        var monotonicFloor: ContinuousClock.Instant { capture.monotonicFloor }

        init(handle: ReplicaSettingsRequestHandle, capture: ReplicaSettingsUpdateInput,
             scope: SettingsConnectionScope, association: SettingsBindingAssociation,
             input: ReplicaSettingsUpdateRequestInput, record: SettingsUpdateOriginalRecord,
             deadline: ContinuousClock.Instant) {
            self.handle = handle
            self.capture = capture
            self.scope = scope
            self.association = association
            self.input = input
            self.record = record
            self.deadline = deadline
        }

        var description: String { "PendingSettingsUpdate([redacted])" }
        var debugDescription: String { description }
    }

    fileprivate enum SettingsUpdateOriginal: CustomStringConvertible, CustomDebugStringConvertible {
        case absent
        case prepared(PendingSettingsUpdate, EncryptedEnvelope)

        var description: String { "SettingsUpdateOriginal([redacted])" }
        var debugDescription: String { description }
    }

    fileprivate final class SettingsUpdateReceiveAttempt: CustomStringConvertible, CustomDebugStringConvertible {
        let issuer: ObjectIdentifier
        let ticket: SettingsReceiveTicket
        let received: ReceivedRelayFrame
        let original: SettingsUpdateOriginal
        var classified: ReplicaClassifiedDelivery?
        var committed: CommittedReplicaSettingsReply?
        var qualification = SettingsAccessQualificationState.unissued

        init(issuer: ObjectIdentifier, ticket: SettingsReceiveTicket, received: ReceivedRelayFrame,
             original: SettingsUpdateOriginal) {
            self.issuer = issuer
            self.ticket = ticket
            self.received = received
            self.original = original
        }

        var description: String { "SettingsUpdateReceiveAttempt([redacted])" }
        var debugDescription: String { description }
    }

    private enum SettingsUpdateReceiveResult: CustomStringConvertible, CustomDebugStringConvertible {
        case durable(ReplicaClassifiedDelivery)
        case qualified(ReplicaSettingsUpdateQualification)
        case duplicate(CommittedReplicaSettingsReply)
        case refused(CommittedReplicaSettingsReply)

        var description: String { "SettingsUpdateReceiveResult([redacted])" }
        var debugDescription: String { description }
    }

    private var settingsUpdateOriginal: SettingsUpdateOriginalRecord?
    private var pendingSettingsUpdate: PendingSettingsUpdate? {
        guard case .update(let pending) = pendingSettingsRequest else { return nil }
        return pending
    }

    private var acknowledgements = Set<UUID>()
    private var settingsScope: SettingsConnectionScope?
    private var pendingSettingsRequest: PendingSettingsRequest?

    private var pendingSettingsAccess: PendingSettingsAccess? {
        guard case .access(let pending) = pendingSettingsRequest else { return nil }
        return pending
    }

    private var pendingSettingsBind: PendingSettingsBind? {
        guard case .bind(let pending) = pendingSettingsRequest else { return nil }
        return pending
    }
    private var pendingSettingsSnapshot: PendingSettingsSnapshot? {
        guard case .snapshot(let pending) = pendingSettingsRequest else { return nil }
        return pending
    }

    private var pendingSettingsInspection: PendingSettingsInspection? {
        guard case .inspection(let pending) = pendingSettingsRequest else { return nil }
        return pending
    }
    private var settingsSnapshotDisplayCache: SettingsSnapshotDisplayCache?
    private var settingsInspectionDisplayCache: SettingsInspectionDisplayCache?
    private var settingsReceive: SettingsReceiveOwner?
    private var settingsAccessDisplayCache: SettingsAccessDisplayCache?
    private var settingsBindingAssociation: SettingsBindingAssociation?

    fileprivate final class SettingsAccessDisplayCache: CustomStringConvertible, CustomDebugStringConvertible {
        weak var scope: SettingsConnectionScope?
        let host: DevicePublicKeys
        let localID: UUID
        let projection: ReplicaSettingsAccessProjection
        let requestHandle: ReplicaSettingsRequestHandle
        let wallFloor: UInt64
        let monotonicFloor: ContinuousClock.Instant
        let queryExpiresAt: UInt64
        let queryDeadline: ContinuousClock.Instant
        let packetExpiresAt: UInt64
        let grantExpiresAt: UInt64?
        let expiresAt: UInt64
        let deadline: ContinuousClock.Instant
        let observedAt: UInt64

        init(scope: SettingsConnectionScope, projection: ReplicaSettingsAccessProjection,
             requestHandle: ReplicaSettingsRequestHandle,
             wallFloor: UInt64, monotonicFloor: ContinuousClock.Instant,
             queryExpiresAt: UInt64, queryDeadline: ContinuousClock.Instant,
             packetExpiresAt: UInt64, grantExpiresAt: UInt64?, expiresAt: UInt64,
             deadline: ContinuousClock.Instant, observedAt: UInt64) {
            self.scope = scope
            host = scope.host
            localID = scope.localID
            self.projection = projection
            self.requestHandle = requestHandle
            self.wallFloor = wallFloor
            self.monotonicFloor = monotonicFloor
            self.queryExpiresAt = queryExpiresAt
            self.queryDeadline = queryDeadline
            self.packetExpiresAt = packetExpiresAt
            self.grantExpiresAt = grantExpiresAt
            self.expiresAt = expiresAt
            self.deadline = deadline
            self.observedAt = observedAt
        }

        var description: String { "SettingsAccessDisplayCache([redacted])" }
        var debugDescription: String { description }
    }

    private final class SettingsSnapshotDisplayCache: CustomStringConvertible, CustomDebugStringConvertible {
        weak var scope: SettingsConnectionScope?
        let host: DevicePublicKeys
        let localID: UUID
        let projection: ReplicaSettingsSnapshotProjection
        let requestHandle: ReplicaSettingsRequestHandle
        let bindingHandle: ReplicaSettingsRequestHandle
        let wallFloor: UInt64
        let monotonicFloor: ContinuousClock.Instant
        let expiresAt: UInt64
        let deadline: ContinuousClock.Instant
        let observedAt: UInt64

        init(scope: SettingsConnectionScope, projection: ReplicaSettingsSnapshotProjection,
             requestHandle: ReplicaSettingsRequestHandle, bindingHandle: ReplicaSettingsRequestHandle,
             wallFloor: UInt64, monotonicFloor: ContinuousClock.Instant,
             expiresAt: UInt64, deadline: ContinuousClock.Instant, observedAt: UInt64) {
            self.scope = scope
            host = scope.host
            localID = scope.localID
            self.projection = projection
            self.requestHandle = requestHandle
            self.bindingHandle = bindingHandle
            self.wallFloor = wallFloor
            self.monotonicFloor = monotonicFloor
            self.expiresAt = expiresAt
            self.deadline = deadline
            self.observedAt = observedAt
        }

        var description: String { "SettingsSnapshotDisplayCache([redacted])" }
        var debugDescription: String { description }
    }

    private final class SettingsInspectionDisplayCache: CustomStringConvertible, CustomDebugStringConvertible {
        weak var scope: SettingsConnectionScope?
        let host: DevicePublicKeys
        let localID: UUID
        let projection: ReplicaSettingsInspectionProjection
        let requestHandle: ReplicaSettingsRequestHandle
        let bindingHandle: ReplicaSettingsRequestHandle
        let wallFloor: UInt64
        let monotonicFloor: ContinuousClock.Instant
        let expiresAt: UInt64
        let deadline: ContinuousClock.Instant
        let observedAt: UInt64

        init(scope: SettingsConnectionScope, projection: ReplicaSettingsInspectionProjection,
             requestHandle: ReplicaSettingsRequestHandle, bindingHandle: ReplicaSettingsRequestHandle,
             wallFloor: UInt64, monotonicFloor: ContinuousClock.Instant,
             expiresAt: UInt64, deadline: ContinuousClock.Instant, observedAt: UInt64) {
            self.scope = scope
            host = scope.host
            localID = scope.localID
            self.projection = projection
            self.requestHandle = requestHandle
            self.bindingHandle = bindingHandle
            self.wallFloor = wallFloor
            self.monotonicFloor = monotonicFloor
            self.expiresAt = expiresAt
            self.deadline = deadline
            self.observedAt = observedAt
        }

        var description: String { "SettingsInspectionDisplayCache([redacted])" }
        var debugDescription: String { description }
    }

    fileprivate final class SettingsConnectionScope: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
        let socket: RelayConnection
        let token = ReplicaSettingsScopeToken()
        let host: DevicePublicKeys
        let localID: UUID
        let epoch: UUID

        init(socket: RelayConnection, host: DevicePublicKeys, localID: UUID, epoch: UUID) {
            self.socket = socket
            self.host = host
            self.localID = localID
            self.epoch = epoch
        }

        var description: String { "SettingsConnectionScope([redacted])" }
        var debugDescription: String { description }
    }

    fileprivate enum SettingsRequestPreparation: CustomStringConvertible, CustomDebugStringConvertible {
        case waiting
        case preparing
        case prepared(EncryptedEnvelope)
        case receiving(EncryptedEnvelope)

        var description: String { "SettingsRequestPreparation([redacted])" }
        var debugDescription: String { description }
    }

    private enum PendingSettingsRequest: CustomStringConvertible, CustomDebugStringConvertible {
        case access(PendingSettingsAccess)
        case bind(PendingSettingsBind)
        case snapshot(PendingSettingsSnapshot)
        case inspection(PendingSettingsInspection)
        case update(PendingSettingsUpdate)

        var description: String { "PendingSettingsRequest([redacted])" }
        var debugDescription: String { description }
    }

    fileprivate final class PendingSettingsAccess: CustomStringConvertible, CustomDebugStringConvertible {
        let handle: ReplicaSettingsRequestHandle
        let input: ReplicaSettingsAccessInput
        let scope: SettingsConnectionScope
        let request: RemoteSettingsAccessRequest
        let wallFloor: UInt64
        let monotonicFloor: ContinuousClock.Instant
        let deadline: ContinuousClock.Instant
        var phase = SettingsRequestPreparation.waiting
        var retired = false
        var consumed = false

        init(handle: ReplicaSettingsRequestHandle, input: ReplicaSettingsAccessInput, scope: SettingsConnectionScope,
             request: RemoteSettingsAccessRequest, wallFloor: UInt64,
             monotonicFloor: ContinuousClock.Instant, deadline: ContinuousClock.Instant) {
            self.handle = handle
            self.input = input
            self.scope = scope
            self.request = request
            self.wallFloor = wallFloor
            self.monotonicFloor = monotonicFloor
            self.deadline = deadline
        }

        var description: String { "PendingSettingsAccess([redacted])" }
        var debugDescription: String { description }
    }

    fileprivate final class SettingsBindingAssociation: CustomStringConvertible, CustomDebugStringConvertible {
        weak var scope: SettingsConnectionScope?
        weak var capture: ReplicaSettingsBindInput?
        let accessHandle: ReplicaSettingsRequestHandle
        let handle: ReplicaSettingsRequestHandle
        let input: ReplicaSettingsBindRequestInput
        let wallFloor: UInt64
        let monotonicFloor: ContinuousClock.Instant
        let deadline: ContinuousClock.Instant
        let requestedParentDeadline: ContinuousClock.Instant
        let readableKeys: [SettingKey]
        let writableKeys: [SettingKey]
        var request: RemoteSettingsBindRequest?
        var requestCaptured = false
        var retired = false
        var parent: ReplicaSettingsBoundParentProjection?
        var parentDeadline: ContinuousClock.Instant?
        var observedAt: UInt64?

        init(scope: SettingsConnectionScope, capture: ReplicaSettingsBindInput, handle: ReplicaSettingsRequestHandle,
             input: ReplicaSettingsBindRequestInput, wallFloor: UInt64,
             monotonicFloor: ContinuousClock.Instant, deadline: ContinuousClock.Instant,
             requestedParentDeadline: ContinuousClock.Instant, readableKeys: [SettingKey], writableKeys: [SettingKey]) {
            self.scope = scope
            self.capture = capture
            accessHandle = capture.accessHandle
            self.handle = handle
            self.input = input
            self.wallFloor = wallFloor
            self.monotonicFloor = monotonicFloor
            self.deadline = deadline
            self.requestedParentDeadline = requestedParentDeadline
            self.readableKeys = readableKeys
            self.writableKeys = writableKeys
        }

        var description: String { "SettingsBindingAssociation([redacted])" }
        var debugDescription: String { description }
    }

    fileprivate final class PendingSettingsBind: CustomStringConvertible, CustomDebugStringConvertible {
        let capture: ReplicaSettingsBindInput
        let scope: SettingsConnectionScope
        let cache: SettingsAccessDisplayCache
        let association: SettingsBindingAssociation
        var phase = SettingsRequestPreparation.waiting
        var retired = false
        var consumed = false

        init(capture: ReplicaSettingsBindInput, scope: SettingsConnectionScope, cache: SettingsAccessDisplayCache,
             association: SettingsBindingAssociation) {
            self.capture = capture
            self.scope = scope
            self.cache = cache
            self.association = association
        }

        var description: String { "PendingSettingsBind([redacted])" }
        var debugDescription: String { description }
    }

    fileprivate final class PendingSettingsSnapshot: CustomStringConvertible, CustomDebugStringConvertible {
        let handle: ReplicaSettingsRequestHandle
        let capture: ReplicaSettingsSnapshotInput
        let scope: SettingsConnectionScope
        let association: SettingsBindingAssociation
        let input: ReplicaSettingsSnapshotRequestInput
        let deadline: ContinuousClock.Instant
        var request: RemoteSettingsSnapshotReadRequest?
        var requestCaptured = false
        var phase = SettingsRequestPreparation.waiting
        var retired = false
        var consumed = false

        var wallFloor: UInt64 { capture.wallFloor }
        var monotonicFloor: ContinuousClock.Instant { capture.monotonicFloor }

        init(handle: ReplicaSettingsRequestHandle, capture: ReplicaSettingsSnapshotInput,
             scope: SettingsConnectionScope, association: SettingsBindingAssociation,
             input: ReplicaSettingsSnapshotRequestInput, deadline: ContinuousClock.Instant) {
            self.handle = handle
            self.capture = capture
            self.scope = scope
            self.association = association
            self.input = input
            self.deadline = deadline
        }

        var description: String { "PendingSettingsSnapshot([redacted])" }
        var debugDescription: String { description }
    }

    fileprivate final class PendingSettingsInspection: CustomStringConvertible, CustomDebugStringConvertible {
        let handle: ReplicaSettingsRequestHandle
        let capture: ReplicaSettingsInspectionInput
        let scope: SettingsConnectionScope
        let association: SettingsBindingAssociation
        let input: ReplicaSettingsInspectionRequestInput
        let deadline: ContinuousClock.Instant
        var request: RemoteSettingsOriginalInspectionRequest?
        var requestCaptured = false
        var phase = SettingsRequestPreparation.waiting
        var retired = false
        var consumed = false

        var wallFloor: UInt64 { capture.wallFloor }
        var monotonicFloor: ContinuousClock.Instant { capture.monotonicFloor }

        init(handle: ReplicaSettingsRequestHandle, capture: ReplicaSettingsInspectionInput,
             scope: SettingsConnectionScope, association: SettingsBindingAssociation,
             input: ReplicaSettingsInspectionRequestInput, deadline: ContinuousClock.Instant) {
            self.handle = handle
            self.capture = capture
            self.scope = scope
            self.association = association
            self.input = input
            self.deadline = deadline
        }

        var description: String { "PendingSettingsInspection([redacted])" }
        var debugDescription: String { description }
    }

    fileprivate final class SettingsReceiveTicket: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
        let scope: SettingsConnectionScope

        init(scope: SettingsConnectionScope) { self.scope = scope }

        var description: String { "SettingsReceiveTicket([redacted])" }
        var debugDescription: String { description }
    }

    private final class SettingsReceiveOwner: CustomStringConvertible, CustomDebugStringConvertible {
        let ticket: SettingsReceiveTicket
        var attempt: SettingsReceiveAttempt?

        init(ticket: SettingsReceiveTicket) { self.ticket = ticket }

        var description: String { "SettingsReceiveOwner([redacted])" }
        var debugDescription: String { description }
    }

    private enum SettingsReceiveAttempt: CustomStringConvertible, CustomDebugStringConvertible {
        case access(SettingsAccessReceiveAttempt)
        case bind(SettingsBindReceiveAttempt)
        case snapshot(SettingsSnapshotReceiveAttempt)
        case inspection(SettingsInspectionReceiveAttempt)
        case update(SettingsUpdateReceiveAttempt)

        var description: String { "SettingsReceiveAttempt([redacted])" }
        var debugDescription: String { description }
    }

    fileprivate enum SettingsAccessOriginal: CustomStringConvertible, CustomDebugStringConvertible {
        case absent
        case prepared(PendingSettingsAccess, EncryptedEnvelope)

        var description: String { "SettingsAccessOriginal([redacted])" }
        var debugDescription: String { description }
    }

    fileprivate enum SettingsSnapshotOriginal: CustomStringConvertible, CustomDebugStringConvertible {
        case absent
        case prepared(PendingSettingsSnapshot, EncryptedEnvelope)

        var description: String { "SettingsSnapshotOriginal([redacted])" }
        var debugDescription: String { description }
    }

    fileprivate enum SettingsInspectionOriginal: CustomStringConvertible, CustomDebugStringConvertible {
        case absent
        case prepared(PendingSettingsInspection, EncryptedEnvelope)

        var description: String { "SettingsInspectionOriginal([redacted])" }
        var debugDescription: String { description }
    }

    fileprivate enum SettingsAccessQualificationState: CustomStringConvertible, CustomDebugStringConvertible {
        case unissued
        case issued(ObjectIdentifier)
        case consumed

        var description: String { "SettingsAccessQualificationState([redacted])" }
        var debugDescription: String { description }
    }

    fileprivate final class SettingsAccessReceiveAttempt: CustomStringConvertible, CustomDebugStringConvertible {
        let issuer: ObjectIdentifier
        let ticket: SettingsReceiveTicket
        let received: ReceivedRelayFrame
        let original: SettingsAccessOriginal
        var classified: ReplicaClassifiedDelivery?
        var committed: CommittedReplicaSettingsReply?
        var qualification = SettingsAccessQualificationState.unissued

        init(issuer: ObjectIdentifier, ticket: SettingsReceiveTicket, received: ReceivedRelayFrame,
             original: SettingsAccessOriginal) {
            self.issuer = issuer
            self.ticket = ticket
            self.received = received
            self.original = original
        }

        var description: String { "SettingsAccessReceiveAttempt([redacted])" }
        var debugDescription: String { description }
    }

    fileprivate enum SettingsBindOriginal: CustomStringConvertible, CustomDebugStringConvertible {
        case absent
        case prepared(PendingSettingsBind, EncryptedEnvelope)

        var description: String { "SettingsBindOriginal([redacted])" }
        var debugDescription: String { description }
    }

    fileprivate final class SettingsBindReceiveAttempt: CustomStringConvertible, CustomDebugStringConvertible {
        let issuer: ObjectIdentifier
        let ticket: SettingsReceiveTicket
        let received: ReceivedRelayFrame
        let original: SettingsBindOriginal
        var classified: ReplicaClassifiedDelivery?
        var committed: CommittedReplicaSettingsReply?
        var qualification = SettingsAccessQualificationState.unissued

        init(issuer: ObjectIdentifier, ticket: SettingsReceiveTicket, received: ReceivedRelayFrame,
             original: SettingsBindOriginal) {
            self.issuer = issuer
            self.ticket = ticket
            self.received = received
            self.original = original
        }

        var description: String { "SettingsBindReceiveAttempt([redacted])" }
        var debugDescription: String { description }
    }

    fileprivate final class SettingsSnapshotReceiveAttempt: CustomStringConvertible, CustomDebugStringConvertible {
        let issuer: ObjectIdentifier
        let ticket: SettingsReceiveTicket
        let received: ReceivedRelayFrame
        let original: SettingsSnapshotOriginal
        var classified: ReplicaClassifiedDelivery?
        var committed: CommittedReplicaSettingsReply?
        var qualification = SettingsAccessQualificationState.unissued

        init(issuer: ObjectIdentifier, ticket: SettingsReceiveTicket, received: ReceivedRelayFrame,
             original: SettingsSnapshotOriginal) {
            self.issuer = issuer
            self.ticket = ticket
            self.received = received
            self.original = original
        }

        var description: String { "SettingsSnapshotReceiveAttempt([redacted])" }
        var debugDescription: String { description }
    }

    fileprivate final class SettingsInspectionReceiveAttempt: CustomStringConvertible, CustomDebugStringConvertible {
        let issuer: ObjectIdentifier
        let ticket: SettingsReceiveTicket
        let received: ReceivedRelayFrame
        let original: SettingsInspectionOriginal
        var classified: ReplicaClassifiedDelivery?
        var committed: CommittedReplicaSettingsReply?
        var qualification = SettingsAccessQualificationState.unissued

        init(issuer: ObjectIdentifier, ticket: SettingsReceiveTicket, received: ReceivedRelayFrame,
             original: SettingsInspectionOriginal) {
            self.issuer = issuer
            self.ticket = ticket
            self.received = received
            self.original = original
        }

        var description: String { "SettingsInspectionReceiveAttempt([redacted])" }
        var debugDescription: String { description }
    }

    private enum SettingsBindReceiveResult: CustomStringConvertible, CustomDebugStringConvertible {
        case durable(ReplicaClassifiedDelivery)
        case qualified(ReplicaSettingsBindQualification)
        case duplicate(CommittedReplicaSettingsReply)
        case refused(CommittedReplicaSettingsReply)

        var description: String { "SettingsBindReceiveResult([redacted])" }
        var debugDescription: String { description }
    }

    private enum SettingsAccessReceiveResult: CustomStringConvertible, CustomDebugStringConvertible {
        case durable(ReplicaClassifiedDelivery)
        case qualified(ReplicaSettingsAccessQualification)
        case duplicate(CommittedReplicaSettingsReply)
        case refused(CommittedReplicaSettingsReply)

        var description: String { "SettingsAccessReceiveResult([redacted])" }
        var debugDescription: String { description }
    }

    private enum SettingsSnapshotReceiveResult: CustomStringConvertible, CustomDebugStringConvertible {
        case durable(ReplicaClassifiedDelivery)
        case qualified(ReplicaSettingsSnapshotQualification)
        case duplicate(CommittedReplicaSettingsReply)
        case refused(CommittedReplicaSettingsReply)

        var description: String { "SettingsSnapshotReceiveResult([redacted])" }
        var debugDescription: String { description }
    }

    private enum SettingsInspectionReceiveResult: CustomStringConvertible, CustomDebugStringConvertible {
        case durable(ReplicaClassifiedDelivery)
        case qualified(ReplicaSettingsInspectionQualification)
        case duplicate(CommittedReplicaSettingsReply)
        case refused(CommittedReplicaSettingsReply)

        var description: String { "SettingsInspectionReceiveResult([redacted])" }
        var debugDescription: String { description }
    }

    private struct SettingsReceiveFailure: Error, LocalizedError, CustomStringConvertible, CustomDebugStringConvertible {
        let receive: any Error
        let settlement: any Error

        var errorDescription: String? { "The received operation and its ownership settlement failed." }
        var description: String { "SettingsReceiveFailure([redacted])" }
        var debugDescription: String { description }
    }

    private init(store: ReplicaStore, identity: DeviceIdentity) {
        self.store = store
        self.identity = identity
    }

    static func open() async throws -> ReplicaService {
        guard let support = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first else { throw WireError.storage }
        let store = try ReplicaStore(directory: support.appendingPathComponent("Everything", isDirectory: true))
        let identity = try KeychainIdentity.loadOrCreate(expectedID: await store.identityID())
        let own = identity.publicKeys
        try await store.bindIdentity(own.deviceID)
        return ReplicaService(store: store, identity: identity)
    }

    func viewState() async throws -> ReplicaViewState {
        let scope = settingsScope
        let cache = settingsAccessDisplayCache
        let snapshotCache = settingsSnapshotDisplayCache
        let inspectionCache = settingsInspectionDisplayCache
        let inspectionInitiallyCurrent = try inspectionCache.map { try isCurrentSettingsInspectionDisplay($0) } ?? false
        let snapshotInitiallyCurrent = try snapshotCache.map { try isCurrentSettingsSnapshotDisplay($0) } ?? false
        let initiallyCurrent: Bool
        if let cache { initiallyCurrent = try isCurrentSettingsAccessDisplay(cache) }
        else { initiallyCurrent = false }
        let snapshot = try await store.snapshot()
        guard settingsAccessDisplayCache === cache else { throw WireError.unavailable }
        let inspectionView = try await settingsInspectionDisplayView(inspectionCache, inspection: snapshot,
                                                                    initiallyCurrent: inspectionInitiallyCurrent)
        guard settingsAccessDisplayCache === cache else { throw WireError.unavailable }
        let snapshotView = try await settingsSnapshotDisplayView(snapshotCache, snapshot: snapshot,
                                                                initiallyCurrent: snapshotInitiallyCurrent)
        guard settingsAccessDisplayCache === cache else { throw WireError.unavailable }
        let settingsAccess = try await settingsAccessDisplayView(cache, snapshot: snapshot, initiallyCurrent: initiallyCurrent)
        guard settingsSnapshotDisplayCache === snapshotCache else { throw WireError.unavailable }
        let currentSnapshotView: ReplicaSettingsSnapshotView?
        if let snapshotView, let snapshotCache {
            let current = try isCurrentSettingsSnapshotDisplay(snapshotCache)
            let remainedCurrent: Bool
            if case .currentConnectionObservation = snapshotView.freshness { remainedCurrent = true }
            else { remainedCurrent = false }
            currentSnapshotView = ReplicaSettingsSnapshotView(
                display: snapshotView.display, observedAt: snapshotView.observedAt, expiresAt: snapshotView.expiresAt,
                freshness: current && remainedCurrent ? .currentConnectionObservation : .stale,
                requestHandle: snapshotView.requestHandle
            )
        } else { currentSnapshotView = nil }
        guard settingsInspectionDisplayCache === inspectionCache else { throw WireError.unavailable }
        let currentInspectionView: ReplicaSettingsInspectionView?
        if let inspectionView, let inspectionCache {
            let current = try isCurrentSettingsInspectionDisplay(inspectionCache)
            let remainedCurrent: Bool
            if case .currentConnectionObservation = inspectionView.freshness { remainedCurrent = true }
            else { remainedCurrent = false }
            currentInspectionView = ReplicaSettingsInspectionView(
                display: inspectionView.display, observedAt: inspectionView.observedAt, expiresAt: inspectionView.expiresAt,
                freshness: current && remainedCurrent ? .currentConnectionObservation : .stale,
                requestHandle: inspectionView.requestHandle
            )
        } else { currentInspectionView = nil }
        let scopeToken: ReplicaSettingsScopeToken?
        if let scope, settingsScope === scope, isCurrentSettingsScope(scope), !snapshot.revoked,
           snapshot.host == scope.host, identity.publicKeys.deviceID == scope.localID {
            scopeToken = scope.token
        } else {
            scopeToken = nil
        }
        return ReplicaViewState(snapshot: snapshot, identity: identity.publicKeys,
                                connection: status, settingsAccess: settingsAccess,
                                settingsAccessRequest: settingsAccessRequestStatus(), settingsScope: scopeToken,
                                settingsBinding: try settingsBindingView(snapshot: snapshot),
                                settingsSnapshot: currentSnapshotView,
                                settingsSnapshotRequest: settingsSnapshotRequestStatus(),
                                settingsInspection: currentInspectionView,
                                settingsInspectionRequest: settingsInspectionRequestStatus())
    }

    private func matchesLastSettingsAccessInput(_ input: ReplicaSettingsAccessInput) -> Bool {
        guard case .access(let captured) = lastSettingsInput else { return false }
        return captured === input
    }

    private func matchesLastSettingsSnapshotInput(_ input: ReplicaSettingsSnapshotInput) -> Bool {
        guard case .snapshot(let captured) = lastSettingsInput else { return false }
        return captured === input
    }

    func requestSettingsSnapshot(_ capture: ReplicaSettingsSnapshotInput) throws -> ReplicaSettingsRequestHandle {
        try Task.checkCancellation()
        guard capture.service === self, let scope = settingsScope, isCurrentSettingsScope(scope),
              scope.token === capture.scope, let association = settingsBindingAssociation,
              association.handle === capture.bindingHandle, association.scope === scope,
              let parent = association.parent, let parentDeadline = association.parentDeadline,
              try isCurrentSettingsParent(association, scope: scope) else { throw WireError.unavailable }
        if let pending = pendingSettingsSnapshot, pending.capture === capture { return pending.handle }
        guard pendingSettingsRequest == nil, !hasPrivateSettingsSend, outbound.count < 16,
              outboundSocket === scope.socket, !matchesLastSettingsSnapshotInput(capture),
              !association.readableKeys.isEmpty else { throw WireError.queueFull }
        let monotonic = ContinuousClock.now
        let wall = try WireID.now()
        guard capture.wallFloor >= association.wallFloor, capture.monotonicFloor >= association.monotonicFloor,
              wall >= capture.wallFloor, wall < capture.expiresAt,
              monotonic >= capture.monotonicFloor, monotonic < capture.deadline else { throw WireError.expired }
        let expiresAt = min(capture.expiresAt, parent.expiresAt)
        guard expiresAt > capture.wallFloor else { throw WireError.expired }
        let time = try RemoteSettingsRequestTime(issuedAt: capture.wallFloor, expiresAt: expiresAt)
        let deadline = min(capture.monotonicFloor.advanced(by: .milliseconds(Int64(expiresAt - capture.wallFloor))), parentDeadline)
        guard monotonic < deadline else { throw WireError.expired }
        let key = OutboundID.snapshot(capture.queryID)
        guard outbound[key] == nil else { throw WireError.conflict }
        try capture.consumeAdmission()
        let input = ReplicaSettingsSnapshotRequestInput(hostID: scope.host.deviceID, queryID: capture.queryID,
                                                       keys: association.readableKeys, time: time)
        let handle = ReplicaSettingsRequestHandle()
        let pending = PendingSettingsSnapshot(handle: handle, capture: capture, scope: scope,
                                              association: association, input: input, deadline: deadline)
        pendingSettingsRequest = .snapshot(pending)
        outbound[key] = .snapshot(SettingsSnapshotSend(original: pending))
        lastSettingsInput = .snapshot(capture)
        settingsSnapshotOutcome = nil
        do {
            let request = try parent.prepareSnapshotRequest(owner: self)
            guard pendingSettingsSnapshot === pending, pending.request == nil,
                  request.hostID == input.hostID, request.queryID == input.queryID,
                  request.keys == input.keys, request.time == input.time else { throw WireError.conflict }
            pending.request = request
            return handle
        } catch {
            retirePendingSettingsSnapshot(pending)
            throw error
        }
    }

    func captureSettingsSnapshotRequestInput(_ projection: ReplicaSettingsBoundParentProjection) throws -> ReplicaSettingsSnapshotRequestInput {
        try Task.checkCancellation()
        guard let pending = pendingSettingsSnapshot, try isCurrentSettingsSnapshotOriginal(pending),
              pending.association.parent === projection, !pending.requestCaptured,
              pending.request == nil, case .waiting = pending.phase,
              case .snapshot(let sent) = outbound[.snapshot(pending.input.queryID)],
              sent.original === pending, !sent.sendStarted else { throw WireError.unavailable }
        pending.requestCaptured = true
        return pending.input
    }

    private func matchesLastSettingsInspectionInput(_ input: ReplicaSettingsInspectionInput) -> Bool {
        guard case .inspection(let captured) = lastSettingsInput else { return false }
        return captured === input
    }

    func requestSettingsInspection(_ capture: ReplicaSettingsInspectionInput) throws -> ReplicaSettingsRequestHandle {
        try Task.checkCancellation()
        guard capture.service === self, let scope = settingsScope, isCurrentSettingsScope(scope),
              scope.token === capture.scope, let association = settingsBindingAssociation,
              association.handle === capture.bindingHandle, association.scope === scope,
              let parent = association.parent, let parentDeadline = association.parentDeadline,
              try isCurrentSettingsParent(association, scope: scope) else { throw WireError.unavailable }
        if let pending = pendingSettingsInspection, pending.capture === capture { return pending.handle }
        guard pendingSettingsRequest == nil, !hasPrivateSettingsSend, outbound.count < 16,
              outboundSocket === scope.socket, !matchesLastSettingsInspectionInput(capture) else { throw WireError.queueFull }
        let monotonic = ContinuousClock.now
        let wall = try WireID.now()
        guard capture.wallFloor >= association.wallFloor, capture.monotonicFloor >= association.monotonicFloor,
              wall >= capture.wallFloor, wall < capture.expiresAt,
              monotonic >= capture.monotonicFloor, monotonic < capture.deadline else { throw WireError.expired }
        let expiresAt = min(capture.expiresAt, parent.expiresAt)
        guard expiresAt > capture.wallFloor else { throw WireError.expired }
        let time = try RemoteSettingsRequestTime(issuedAt: capture.wallFloor, expiresAt: expiresAt)
        let deadline = min(capture.monotonicFloor.advanced(by: .milliseconds(Int64(expiresAt - capture.wallFloor))), parentDeadline)
        guard monotonic < deadline else { throw WireError.expired }
        let key = OutboundID.inspection(capture.queryID)
        guard outbound[key] == nil else { throw WireError.conflict }
        try capture.consumeAdmission()
        let input = ReplicaSettingsInspectionRequestInput(hostID: scope.host.deviceID, queryID: capture.queryID,
                                                       requestID: capture.requestID, operationID: capture.operationID, time: time)
        let handle = ReplicaSettingsRequestHandle()
        let pending = PendingSettingsInspection(handle: handle, capture: capture, scope: scope,
                                              association: association, input: input, deadline: deadline)
        pendingSettingsRequest = .inspection(pending)
        outbound[key] = .inspection(SettingsInspectionSend(original: pending))
        lastSettingsInput = .inspection(capture)
        settingsInspectionOutcome = nil
        do {
            let request = try parent.prepareInspectionRequest(owner: self)
            guard pendingSettingsInspection === pending, pending.request == nil,
                  request.hostID == input.hostID, request.queryID == input.queryID,
                  request.requestID == input.requestID, request.operationID == input.operationID, request.time == input.time else { throw WireError.conflict }
            pending.request = request
            return handle
        } catch {
            retirePendingSettingsInspection(pending)
            throw error
        }
    }

    func captureSettingsInspectionRequestInput(_ projection: ReplicaSettingsBoundParentProjection) throws -> ReplicaSettingsInspectionRequestInput {
        try Task.checkCancellation()
        guard let pending = pendingSettingsInspection, try isCurrentSettingsInspectionOriginal(pending),
              pending.association.parent === projection, !pending.requestCaptured,
              pending.request == nil, case .waiting = pending.phase,
              case .inspection(let sent) = outbound[.inspection(pending.input.queryID)],
              sent.original === pending, !sent.sendStarted else { throw WireError.unavailable }
        pending.requestCaptured = true
        return pending.input
    }

    func requestSettingsAccess(_ input: ReplicaSettingsAccessInput) throws -> ReplicaSettingsRequestHandle {
        try Task.checkCancellation()
        guard input.service === self, let scope = settingsScope, scope.token === input.scope,
              isCurrentSettingsScope(scope) else { throw WireError.unavailable }
        if let pending = pendingSettingsAccess, pending.input === input { return pending.handle }
        for entry in outbound.values {
            if case .access(let sent) = entry, sent.original.input === input { return sent.original.handle }
        }
        guard pendingSettingsRequest == nil, !hasPrivateSettingsSend, outbound.count < 16,
              outboundSocket === scope.socket, !matchesLastSettingsAccessInput(input) else { throw WireError.queueFull }
        let monotonic = ContinuousClock.now
        let wall = try WireID.now()
        guard monotonic >= input.monotonicFloor, monotonic < input.deadline,
              wall >= input.wallFloor, wall < input.expiresAt else { throw WireError.expired }
        let generation = try settingsQueryGeneration(for: input.query, scope: scope, wall: wall)
        let time = try RemoteSettingsRequestTime(issuedAt: input.wallFloor, expiresAt: input.expiresAt)
        let request = try RemoteSettingsAccessRequest(hostID: scope.host.deviceID, queryID: input.queryID, time: time,
                                                      query: input.query, generation: generation)
        let handle = ReplicaSettingsRequestHandle()
        let pending = PendingSettingsAccess(
            handle: handle, input: input, scope: scope, request: request, wallFloor: input.wallFloor,
            monotonicFloor: input.monotonicFloor, deadline: input.deadline
        )
        let key = OutboundID.access(request.queryID)
        guard outbound[key] == nil else { throw WireError.conflict }
        outbound[key] = .access(SettingsAccessSend(original: pending))
        pendingSettingsRequest = .access(pending)
        lastSettingsInput = .access(input)
        settingsAccessOutcome = nil
        settingsAccessHandle = handle
        return handle
    }

    private func settingsQueryGeneration(for query: RemoteSettingsQuery, scope: SettingsConnectionScope,
                                         wall: UInt64) throws -> RemoteSettingsGeneration? {
        guard query.requiresGeneration else { return nil }
        guard let cache = settingsAccessDisplayCache, cache.scope === scope,
              cache.host == scope.host, cache.localID == scope.localID,
              case .available(let summary) = cache.projection.display.status,
              let generation = cache.projection.generation, let grantExpiresAt = cache.grantExpiresAt,
              wall < grantExpiresAt else { throw WireError.unavailable }
        switch query {
        case .snapshot(let keys):
            guard keys.allSatisfy(summary.readableKeys.contains) else { throw WireError.unavailable }
        case .update(_, _, let changes):
            guard changes.allSatisfy({ summary.writableKeys.contains($0.key) }) else { throw WireError.unavailable }
        case .sessionOpen, .sessionSend:
            guard case .configuredProvider = summary.sessionConsent else { throw WireError.unavailable }
        default: break
        }
        return generation
    }

    func settingsQueryOutcome(_ handle: ReplicaSettingsRequestHandle) -> ReplicaSettingsQueryOutcome {
        if pendingSettingsAccess?.handle === handle { return .pending }
        for entry in outbound.values {
            if case .access(let sent) = entry, sent.original.handle === handle { return .pending }
        }
        if let cache = settingsQueryResultCache, cache.handle === handle {
            return .received(cache.result, observedAt: cache.observedAt)
        }
        guard settingsAccessHandle === handle, let outcome = settingsAccessOutcome else { return .ended(.interrupted) }
        return outcome == .received ? .accessObserved : .ended(outcome)
    }

    func requestSettingsBind(_ capture: ReplicaSettingsBindInput) throws -> ReplicaSettingsRequestHandle {
        try Task.checkCancellation()
        guard capture.service === self, let scope = settingsScope, isCurrentSettingsScope(scope),
              scope.token === capture.scope, let cache = settingsAccessDisplayCache, cache.scope === scope,
              cache.requestHandle === capture.accessHandle else { throw WireError.unavailable }
        releaseSettingsAssociations()
        if let association = settingsBindingAssociation {
            guard association.capture === capture else { throw WireError.queueFull }
            return association.handle
        }
        guard pendingSettingsRequest == nil, !hasPrivateSettingsSend, outbound.count < 16,
              outboundSocket === scope.socket else { throw WireError.queueFull }
        guard case .available(let summary) = cache.projection.display.status,
              let grantExpiresAt = cache.grantExpiresAt else { throw WireError.unavailable }
        let monotonic = ContinuousClock.now
        let wall = try WireID.now()
        guard try isCurrentSettingsAccessDisplay(cache), wall >= capture.wallFloor,
              wall < capture.expiresAt, monotonic >= capture.monotonicFloor, monotonic < capture.deadline,
              capture.wallFloor >= cache.wallFloor, capture.monotonicFloor >= cache.monotonicFloor else { throw WireError.expired }
        let expiresAt = min(capture.expiresAt, cache.expiresAt)
        let parentExpiresAt = min(capture.parentMaximum, grantExpiresAt)
        guard expiresAt > capture.wallFloor, parentExpiresAt > capture.wallFloor,
              parentExpiresAt <= UInt64(Int64.max), parentExpiresAt - cache.wallFloor <= 330_000 else { throw WireError.expired }
        let time = try RemoteSettingsRequestTime(issuedAt: capture.wallFloor, expiresAt: expiresAt)
        let deadline = min(capture.monotonicFloor.advanced(by: .milliseconds(Int64(expiresAt - capture.wallFloor))), cache.deadline)
        let parentDeadline = min(
            capture.monotonicFloor.advanced(by: .milliseconds(Int64(parentExpiresAt - capture.wallFloor))),
            cache.monotonicFloor.advanced(by: .milliseconds(Int64(parentExpiresAt - cache.wallFloor)))
        )
        guard monotonic < deadline, monotonic < parentDeadline else { throw WireError.expired }
        let handle = ReplicaSettingsRequestHandle()
        let input = ReplicaSettingsBindRequestInput(hostID: scope.host.deviceID, bindingRequestID: capture.bindingID,
                                                   time: time, requestedParentExpiresAt: parentExpiresAt,
                                                   grantExpiresAt: grantExpiresAt)
        let association = SettingsBindingAssociation(
            scope: scope, capture: capture, handle: handle, input: input, wallFloor: capture.wallFloor,
            monotonicFloor: capture.monotonicFloor, deadline: deadline, requestedParentDeadline: parentDeadline, readableKeys: summary.readableKeys,
            writableKeys: summary.writableKeys
        )
        let pending = PendingSettingsBind(capture: capture, scope: scope, cache: cache, association: association)
        let key = OutboundID.bind(input.bindingRequestID)
        guard outbound[key] == nil else { throw WireError.conflict }
        settingsBindingAssociation = association
        pendingSettingsRequest = .bind(pending)
        outbound[key] = .bind(SettingsBindSend(original: pending))
        do {
            let request = try cache.projection.prepareBindRequest(owner: self)
            guard pendingSettingsBind === pending, association.request == nil,
                  request.hostID == input.hostID, request.bindingRequestID == input.bindingRequestID,
                  request.time == input.time, request.requestedParentExpiresAt == input.requestedParentExpiresAt else { throw WireError.conflict }
            association.request = request
            return handle
        } catch {
            retirePendingSettingsBind(pending)
            throw error
        }
    }

    func cancelSettings(_ handle: ReplicaSettingsRequestHandle) {
        if let pending = pendingSettingsAccess, pending.handle === handle {
            settingsAccessOutcome = .cancelled
            retirePendingSettingsAccess(pending)
        } else if let pending = pendingSettingsSnapshot, pending.handle === handle {
            settingsSnapshotOutcome = .cancelled
            retirePendingSettingsSnapshot(pending)
        } else if let pending = pendingSettingsInspection, pending.handle === handle {
            settingsInspectionOutcome = .cancelled
            retirePendingSettingsInspection(pending)
        } else if let pending = pendingSettingsUpdate, pending.handle === handle {
            pending.record.status = .cancelled
            retirePendingSettingsUpdate(pending)
        } else if let pending = pendingSettingsBind, pending.association.handle === handle {
            retirePendingSettingsBind(pending)
        }
        for entry in outbound.values {
            switch entry {
            case .access(let sent) where sent.original.handle === handle:
                settingsAccessOutcome = .cancelled
                retirePendingSettingsAccess(sent.original)
            case .snapshot(let sent) where sent.original.handle === handle:
                settingsSnapshotOutcome = .cancelled
                retirePendingSettingsSnapshot(sent.original)
            case .inspection(let sent) where sent.original.handle === handle:
                settingsInspectionOutcome = .cancelled
                retirePendingSettingsInspection(sent.original)
            case .update(let sent) where sent.original.handle === handle:
                sent.original.record.status = .cancelled
                retirePendingSettingsUpdate(sent.original)
            case .bind(let sent) where sent.original.association.handle === handle:
                sent.original.association.retired = true
                retirePendingSettingsBind(sent.original)
            default: break
            }
        }
        if let association = settingsBindingAssociation, association.handle === handle {
            association.retired = true
        }
        releaseSettingsAssociations()
    }

    func captureSettingsBindRequestInput(_ projection: ReplicaSettingsAccessProjection) throws -> ReplicaSettingsBindRequestInput {
        try Task.checkCancellation()
        guard let pending = pendingSettingsBind, !pending.retired, !pending.consumed,
              case .waiting = pending.phase, let cache = settingsAccessDisplayCache,
              cache === pending.cache, cache.projection === projection,
              cache.scope === pending.scope, try isCurrentSettingsAccessDisplay(cache),
              settingsBindingAssociation === pending.association,
              pending.association.scope === pending.scope, !pending.association.retired,
              !pending.association.requestCaptured, pending.association.request == nil,
              isCurrentSettingsScope(pending.scope) else { throw WireError.unavailable }
        let association = pending.association
        let monotonic = ContinuousClock.now
        let wall = try WireID.now()
        guard wall >= association.wallFloor, wall < association.input.time.expiresAt,
              monotonic >= association.monotonicFloor, monotonic < association.deadline else { throw WireError.expired }
        association.requestCaptured = true
        return association.input
    }

    func configureRelay(_ value: String) async throws {
        guard runner == nil, starting == nil, stopping == nil, !disabling else { throw WireError.unavailable }
        try await store.configure(origin: RelayOrigin(value))
    }

    func inspectInvitation(_ text: String) async throws -> PairingSummary {
        try await store.checkActive()
        guard pendingCompletion == nil else { throw WireError.pairing }
        let invitation = try PairingInvitation(Data(text.utf8), now: WireID.now())
        if let host = try await store.host(), host != invitation.inviter { throw WireError.identity }
        pendingInvitation = invitation
        pairingEpoch = UUID()
        return PairingSummary(deviceID: invitation.inviter.deviceID, fingerprint: invitation.inviter.fingerprint, expires: invitation.expires)
    }

    func createPairingRequest(confirmedFingerprint: String) async throws -> String {
        guard let invitation = pendingInvitation else { throw WireError.pairing }
        pendingInvitation = nil
        let capturedEpoch = pairingEpoch
        let request = try await invitation.request(identity: identity, confirmedFingerprint: confirmedFingerprint, now: WireID.now())
        guard capturedEpoch == pairingEpoch else { throw WireError.pairing }
        pendingCompletion = invitation
        let data = try request.encoded(maximum: 4_096)
        guard let text = String(data: data, encoding: .utf8) else { throw WireError.invalid }
        return text
    }

    func finishPairing(_ text: String) async throws {
        guard let invitation = pendingCompletion else { throw WireError.pairing }
        pendingCompletion = nil
        let own = identity.publicKeys
        let host = try invitation.finish(Data(text.utf8), responder: own, now: WireID.now())
        try await store.pin(host, pairingID: invitation.id)
    }

    func cancelPairing() {
        pendingInvitation = nil
        pendingCompletion = nil
        pairingEpoch = UUID()
    }

    func addConversation(_ id: UUID, title: String) async throws {
        try await store.addConversation(id, title: title)
        selectedConversation = id
        preferSelectedSync = true
    }

    func selectConversation(_ id: UUID?) {
        if selectedConversation != id {
            selectedConversation = id
            preferSelectedSync = true
        }
    }

    func events(conversation: UUID, before: UInt64? = nil) async throws -> [WireEvent] {
        try await store.events(conversation: conversation, before: before)
    }

    func claimNotifications() async throws -> ReplicaNotificationPage {
        guard !claimingNotifications, !disabling, stopping == nil else { throw WireError.unavailable }
        claimingNotifications = true
        defer { claimingNotifications = false }
        let captured = epoch
        try await store.checkActive()
        guard let host = try await store.host() else { throw WireError.identity }
        let conversations = try await store.synchronizationConversations()
        guard !conversations.isEmpty else { return ReplicaNotificationPage(notices: [], hasMore: false) }
        let conversation = conversations[notificationPosition % conversations.count]
        notificationPosition = (notificationPosition + 1) % conversations.count
        guard captured == epoch, !disabling, stopping == nil, !Task.isCancelled else { throw CancellationError() }
        let result = try await store.claimNotifications(conversation: conversation, host: host,
            localID: identity.publicKeys.deviceID, now: WireID.now())
        guard captured == epoch, !disabling, stopping == nil, !Task.isCancelled else { throw CancellationError() }
        return result
    }

    func validateNotificationScope(host: DevicePublicKeys, localID: UUID) async throws {
        guard !disabling, stopping == nil, identity.publicKeys.deviceID == localID else { throw WireError.identity }
        try await store.checkActive(host: host)
        guard !disabling, stopping == nil, try await store.identityID() == localID else { throw WireError.identity }
    }

    func sendMessage(conversation: UUID, text: String) async throws {
        guard !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty, !text.contains("\0"), text.utf8.count <= 32_768 else { throw WireError.size }
        let now = try WireID.now()
        let message = WireValue.object([
            "id": .id(UUID()), "role": .string("user"), "created_at": .unsigned(now),
            "content": .array([.object(["kind": .string("text"), "text": .string(text)])])
        ])
        let payload = WireValue.object(["kind": .string("send_message"), "message": message])
        try await submit(conversation: conversation, payload: payload, now: now)
    }

    func steer(conversation: UUID, task: UUID, instruction: String) async throws {
        guard !instruction.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty, instruction.utf8.count <= 32_768, !instruction.contains("\0") else { throw WireError.size }
        try await submit(conversation: conversation, payload: .object(["kind": .string("steer_task"), "task_id": .id(task), "instruction": .string(instruction)]), now: WireID.now())
    }

    func cancel(conversation: UUID, task: UUID) async throws {
        try await submit(conversation: conversation, payload: .object(["kind": .string("cancel_task"), "task_id": .id(task)]), now: WireID.now())
    }

    func resolve(_ approval: ReplicaApproval, decision: String) async throws {
        try await submit(conversation: approval.conversationID, payload: .object(["kind": .string("resolve_permission"), "operation_id": .id(approval.id), "decision": .string(decision)]), now: WireID.now(), permission: approval)
    }

    private func submit(conversation: UUID, payload: WireValue, now: UInt64, permission: ReplicaApproval? = nil) async throws {
        try await store.checkActive()
        guard let host = try await store.host(), try await store.cursor(conversation) != nil else { throw WireError.identity }
        let own = identity.publicKeys
        let command = DevicePackets.command(conversation, device: own.deviceID, payload: payload, now: now, id: UUID())
        let packet = DevicePackets.packet(.object(["kind": .string("command"), "command": command]))
        let envelope = try await identity.seal(packet.encoded(), recipient: host, now: now)
        try await store.queueCommand(command, envelope: envelope, host: host, permission: permission)
    }

    func synchronize(_ conversation: UUID) async throws {
        try await store.checkActive()
        guard let host = try await store.host(), let cursor = try await store.cursor(conversation) else { throw WireError.identity }
        let now = try WireID.now()
        let envelope = try await identity.seal(DevicePackets.sync(conversation, after: cursor).encoded(), recipient: host, now: now)
        try await store.queueSync(envelope, conversation: conversation, host: host, now: now)
    }

    func start() async throws {
        let requestedEpoch = epoch
        if let stopping {
            await stopping.task.value
            finishStop(stopping.id)
        }
        try Task.checkCancellation()
        guard epoch == requestedEpoch else { throw CancellationError() }
        guard !disabling else { throw WireError.unavailable }
        guard runner == nil, starting == nil else { return }
        let captured = UUID()
        epoch = captured
        starting = captured
        defer { if starting == captured { starting = nil } }
        try await store.checkActive()
        let snapshot = try await store.snapshot()
        guard snapshot.host != nil, let configured = snapshot.origin else { throw WireError.pairing }
        let origin = try RelayOrigin(configured)
        guard epoch == captured, !Task.isCancelled else { throw CancellationError() }
        runner = Task { [weak self] in
            guard let self else { return }
            await self.run(origin: origin, epoch: captured)
        }
    }

    func stop() async {
        retireSettingsScope()
        epoch = UUID()
        starting = nil
        if let stopping {
            await stopping.task.value
            finishStop(stopping.id)
            return
        }
        let running = runner
        let activeConnection = connection
        let stopping = (id: UUID(), task: Task {
            running?.cancel()
            await activeConnection?.close()
            await running?.value
        })
        self.stopping = stopping
        await stopping.task.value
        finishStop(stopping.id)
    }

    private func finishStop(_ id: UUID) {
        guard stopping?.id == id else { return }
        runner = nil
        connection = nil
        outbound.removeAll()
        outboundSocket = nil
        lastSettingsInput = nil
        abandonedSettingsMessages.removeAll()
        releaseSettingsAssociations()
        acknowledgements.removeAll()
        reconnectForCapacity = false
        status = "Disconnected"
        self.stopping = nil
    }

    func disable() async throws {
        settingsAccessDisplayCache = nil
        settingsQueryResultCache = nil
        settingsSnapshotDisplayCache = nil
        settingsInspectionDisplayCache = nil
        retireSettingsScope()
        disabling = true
        cancelPairing()
        await stop()
        try await store.disable()
        status = "Device disabled"
    }

    private func run(origin: RelayOrigin, epoch captured: UUID) async {
        defer { retireSettingsScope() }
        var attempt = 0
        while !Task.isCancelled && epoch == captured {
            do {
                _ = try await drainInbox()
                status = "Connecting"
                let socket = try await RelayConnection.connect(origin: origin, identity: identity)
                guard epoch == captured, !Task.isCancelled else {
                    retireSettingsScope(socket: socket)
                    await socket.close()
                    return
                }
                connection = socket
                try await installSettingsScope(socket: socket, epoch: captured)
                reconnectForCapacity = false
                status = "Connected"
                try await runConnection(socket)
                attempt = 0
            } catch is CancellationError {
                break
            } catch let error as WireError {
                if Task.isCancelled || epoch != captured { break }
                if error != .network && error != .timeout {
                    retireSettingsScope()
                    await connection?.close()
                    connection = nil
                    status = error.localizedDescription
                    runner = nil
                    return
                }
                status = "Connection interrupted; retrying"
            } catch is URLError {
                if Task.isCancelled || epoch != captured { break }
                status = "Connection interrupted; retrying"
            } catch {
                retireSettingsScope()
                await connection?.close()
                connection = nil
                status = "Device operation failed; inspection is required"
                runner = nil
                return
            }
            retireSettingsScope()
            await connection?.close()
            connection = nil
            attempt = min(attempt + 1, 5)
            do { try await Task.sleep(for: .seconds(1 << attempt)) } catch { break }
        }
        if epoch == captured { status = "Disconnected"; runner = nil }
    }

    private func runConnection(_ socket: RelayConnection) async throws {
        guard outbound.isEmpty, outboundSocket == nil else { throw WireError.conflict }
        outboundSocket = socket
        acknowledgements.removeAll()
        defer { finishOutbound(socket: socket) }
        try await withThrowingTaskGroup(of: Void.self) { group in
            group.addTask { [weak self] in
                while !Task.isCancelled {
                    guard let self else { throw CancellationError() }
                    let ticket = try await self.claimSettingsReceive(socket: socket)
                    do {
                        try await self.receiveSettingsOwnedFrame(socket: socket, ticket: ticket)
                    } catch {
                        await self.retireSettingsScope(socket: socket)
                        do { try await self.finishSettingsReceive(ticket) }
                        catch let settlement { throw SettingsReceiveFailure(receive: error, settlement: settlement) }
                        throw error
                    }
                    try await self.finishSettingsReceive(ticket)
                }
            }
            group.addTask { [weak self] in
                var seconds = 0
                while !Task.isCancelled {
                    guard let self else { throw CancellationError() }
                    try await self.pump(socket: socket, synchronize: seconds % 5 == 0)
                    if seconds % 30 == 0 { try await socket.send(.object(["type": .string("ping"), "schema_version": .unsigned(1)])) }
                    seconds += 1
                    try await Task.sleep(for: .seconds(1))
                }
            }
            do {
                _ = try await group.next()
                retireSettingsScope(socket: socket)
                group.cancelAll()
                await socket.close()
            } catch {
                retireSettingsScope(socket: socket)
                group.cancelAll()
                await socket.close()
                throw error
            }
        }
    }

    private func finishOutbound(socket: RelayConnection) {
        guard outboundSocket === socket else { return }
        if hasSettingsAccessSend, settingsAccessOutcome == nil { settingsAccessOutcome = .interrupted }
        if outbound.values.contains(where: { if case .snapshot = $0 { true } else { false } }),
           settingsSnapshotOutcome == nil { settingsSnapshotOutcome = .interrupted }
        if outbound.values.contains(where: { if case .inspection = $0 { true } else { false } }),
           settingsInspectionOutcome == nil { settingsInspectionOutcome = .interrupted }
        if let record = settingsUpdateOriginal, record.status == nil { record.status = .interrupted }
        outbound.removeAll()
        outboundSocket = nil
        lastSettingsInput = nil
        abandonedSettingsMessages.removeAll()
        releaseSettingsAssociations()
    }

    private func releaseSettingsAssociations() {
        if let association = settingsBindingAssociation,
           association.retired || !(association.scope.map(isCurrentSettingsScope) ?? false)
               || association.parentDeadline.map({ ContinuousClock.now >= $0 }) == true,
           pendingSettingsBind?.association !== association,
           !outbound.values.contains(where: { entry in
               if case .bind(let sent) = entry { sent.original.association === association } else { false }
           }) {
            settingsBindingAssociation = nil
        }
        if let record = settingsUpdateOriginal,
           record.status != nil || !(record.scope.map(isCurrentSettingsScope) ?? false),
           pendingSettingsUpdate?.record !== record,
           !outbound.values.contains(where: { entry in
               if case .update(let sent) = entry { sent.original.record === record } else { false }
           }) {
            settingsUpdateOriginal = nil
        }
    }

    private func releaseSettingsSend(_ key: OutboundID, messageID: UUID?, started: Bool, accepted: Bool) throws {
        outbound.removeValue(forKey: key)
        guard started, !accepted, let messageID else { return }
        guard abandonedSettingsMessages.count < 16 else { throw WireError.timeout }
        abandonedSettingsMessages.insert(messageID)
    }

    private var hasSettingsAccessSend: Bool {
        outbound.values.contains { if case .access = $0 { true } else { false } }
    }

    private var hasPrivateSettingsSend: Bool {
        outbound.values.contains { entry in
            switch entry {
            case .ordinary: false
            case .access, .bind, .snapshot, .inspection, .update: true
            }
        }
    }

    private func settingsBindingView(snapshot: ReplicaSnapshot) throws -> ReplicaSettingsBindingView? {
        guard let association = settingsBindingAssociation else { return nil }
        let sent: SettingsBindSend?
        if case .bind(let original) = outbound[.bind(association.input.bindingRequestID)] { sent = original }
        else { sent = nil }
        let stage: ReplicaSettingsBindingStage
        if let sent {
            if association.retired || sent.original.consumed { stage = .settling }
            else { stage = sent.sendStarted ? .awaitingReply : .preparing }
        } else {
            stage = association.parent == nil ? .retained : .observed
        }
        var current = false
        if let scope = association.scope, !association.retired, isCurrentSettingsScope(scope),
           snapshot.host == scope.host, !snapshot.revoked, identity.publicKeys.deviceID == scope.localID,
           let parent = association.parent, let deadline = association.parentDeadline {
            let monotonic = ContinuousClock.now
            let wall = try WireID.now()
            current = wall >= association.wallFloor && wall < parent.expiresAt
                && monotonic >= association.monotonicFloor && monotonic < deadline
        }
        return ReplicaSettingsBindingView(stage: stage, observedAt: association.observedAt,
                                           expiresAt: association.parent?.expiresAt,
                                           freshness: current ? .currentConnectionObservation : .stale,
                                           requestHandle: association.handle)
    }

    private func settingsAccessRequestStatus() -> ReplicaSettingsAccessRequestStatus? {
        if let sent = outbound.values.compactMap({ entry -> SettingsAccessSend? in
            if case .access(let sent) = entry { return sent }
            return nil
        }).first {
            if !sent.sendStarted, !sent.original.retired { return .preparing }
            if !sent.original.retired { return .awaitingReply }
            return .settling
        }
        if let pending = pendingSettingsAccess {
            return pending.retired ? settingsAccessOutcome ?? .interrupted : .preparing
        }
        return settingsAccessOutcome
    }

    private func completeSettingsAccessSend(_ sent: SettingsAccessSend) throws {
        let key = OutboundID.access(sent.original.request.queryID)
        guard case .access(let registered) = outbound[key], registered === sent,
              outboundSocket === sent.original.scope.socket else { throw WireError.conflict }
        guard !sent.failed, sent.sendReturned, sent.accepted, sent.replyReturned else { return }
        outbound.removeValue(forKey: key)
    }

    private func completeOrdinarySend(_ sent: OrdinarySend, id: UUID, socket: RelayConnection) throws {
        guard outboundSocket === socket, case .ordinary(let registered) = outbound[.ordinary(id)],
              registered === sent else { throw WireError.conflict }
        guard !sent.failed, sent.sendReturned, sent.accepted else { return }
        outbound.removeValue(forKey: .ordinary(id))
    }

    private func hasOutboundMessage(_ id: UUID) -> Bool {
        if outbound[.ordinary(id)] != nil { return true }
        return outbound.values.contains {
            switch $0 {
            case .access(let sent): sent.envelope?.header.messageID == id
            case .bind(let sent): sent.envelope?.header.messageID == id
            case .snapshot(let sent): sent.envelope?.header.messageID == id
            case .inspection(let sent): sent.envelope?.header.messageID == id
            case .update(let sent): sent.envelope?.header.messageID == id
            case .ordinary: false
            }
        }
    }

    private func sendPendingSettingsAccess(socket: RelayConnection) async throws {
        guard let pending = pendingSettingsAccess, pending.scope.socket === socket,
              try isCurrentSettingsOriginal(pending), case .prepared(let envelope) = pending.phase,
              case .access(let sent) = outbound[.access(pending.request.queryID)], sent.original === pending,
              !sent.sendStarted else { return }
        guard try await checkSettingsStore(pending), try isCurrentSettingsOriginal(pending),
              case .prepared(let retained) = pending.phase, sameSettingsEnvelope(retained, envelope) else { return }
        guard outboundSocket === socket, case .access(let registered) = outbound[.access(pending.request.queryID)],
              registered === sent, let bound = sent.envelope, sameSettingsEnvelope(bound, envelope),
              !sent.sendStarted, outbound[.ordinary(envelope.header.messageID)] == nil else { throw WireError.conflict }
        sent.sendStarted = true
        do {
            try await socket.send(.object(["type": .string("send"), "schema_version": .unsigned(1), "envelope": envelope.wire]))
            sent.sendReturned = true
            try completeSettingsAccessSend(sent)
        } catch {
            sent.sendReturned = true
            sent.failed = true
            retireSettingsScope(socket: socket)
            throw error
        }
    }

    private func completeSettingsBindSend(_ sent: SettingsBindSend) throws {
        let key = OutboundID.bind(sent.original.association.input.bindingRequestID)
        guard case .bind(let registered) = outbound[key], registered === sent,
              outboundSocket === sent.original.scope.socket else { throw WireError.conflict }
        guard !sent.failed, sent.sendReturned, sent.accepted, sent.replyReturned else { return }
        outbound.removeValue(forKey: key)
    }

    private func sendPendingSettingsBind(socket: RelayConnection) async throws {
        guard let pending = pendingSettingsBind, pending.scope.socket === socket,
              try isCurrentSettingsBindOriginal(pending), case .prepared(let envelope) = pending.phase,
              case .bind(let sent) = outbound[.bind(pending.association.input.bindingRequestID)],
              sent.original === pending, !sent.sendStarted else { return }
        guard try await checkSettingsBindStore(pending), try isCurrentSettingsBindOriginal(pending),
              case .prepared(let retained) = pending.phase, sameSettingsEnvelope(retained, envelope) else { return }
        guard outboundSocket === socket,
              case .bind(let registered) = outbound[.bind(pending.association.input.bindingRequestID)],
              registered === sent, let bound = sent.envelope, sameSettingsEnvelope(bound, envelope),
              !sent.sendStarted, outbound[.ordinary(envelope.header.messageID)] == nil else { throw WireError.conflict }
        sent.sendStarted = true
        do {
            try await socket.send(.object(["type": .string("send"), "schema_version": .unsigned(1), "envelope": envelope.wire]))
            sent.sendReturned = true
            try completeSettingsBindSend(sent)
        } catch {
            sent.sendReturned = true
            sent.failed = true
            retireSettingsScope(socket: socket)
            throw error
        }
    }

    private func pump(socket: RelayConnection, synchronize refresh: Bool) async throws {
        try Task.checkCancellation()
        try await preparePendingSettingsAccess(socket: socket)
        try await preparePendingSettingsBind(socket: socket)
        try await preparePendingSettingsSnapshot(socket: socket)
        try await preparePendingSettingsInspection(socket: socket)
        try await preparePendingSettingsUpdate(socket: socket)
        try await store.checkActive()
        let inboxDrained = try await drainInbox()
        if refresh, inboxDrained {
            do { try await synchronizeOne() }
            catch WireError.queueFull { status = "Local queue is full; preserving pending work" }
        }
        for entry in outbound.values {
            switch entry {
            case .ordinary(let ordinary):
                if let started = ordinary.started, started.duration(to: .now) > .seconds(15) { throw WireError.timeout }
            case .access(let sent):
                let live = try isCurrentSettingsOriginal(sent.original) || sent.original.consumed
                guard !sent.sendStarted || sent.sendReturned,
                      !live || ContinuousClock.now >= sent.original.deadline else { continue }
                if settingsAccessOutcome == nil { settingsAccessOutcome = .expired }
                retirePendingSettingsAccess(sent.original)
                try releaseSettingsSend(.access(sent.original.request.queryID), messageID: sent.envelope?.header.messageID,
                                        started: sent.sendStarted, accepted: sent.accepted)
            case .bind(let sent):
                let association = sent.original.association
                let live = try isCurrentSettingsBindOriginal(sent.original) || sent.original.consumed
                guard !sent.sendStarted || sent.sendReturned,
                      association.retired || !live || ContinuousClock.now >= association.deadline else { continue }
                if !sent.original.consumed { association.retired = true }
                retirePendingSettingsBind(sent.original)
                try releaseSettingsSend(.bind(association.input.bindingRequestID), messageID: sent.envelope?.header.messageID,
                                        started: sent.sendStarted, accepted: sent.accepted)
            case .snapshot(let sent):
                let live = try isCurrentSettingsSnapshotOriginal(sent.original) || sent.original.consumed
                guard !sent.sendStarted || sent.sendReturned,
                      !live || ContinuousClock.now >= sent.original.deadline else { continue }
                if settingsSnapshotOutcome == nil { settingsSnapshotOutcome = .expired }
                retirePendingSettingsSnapshot(sent.original)
                try releaseSettingsSend(.snapshot(sent.original.input.queryID), messageID: sent.envelope?.header.messageID,
                                        started: sent.sendStarted, accepted: sent.accepted)
            case .inspection(let sent):
                let live = try isCurrentSettingsInspectionOriginal(sent.original) || sent.original.consumed
                guard !sent.sendStarted || sent.sendReturned,
                      !live || ContinuousClock.now >= sent.original.deadline else { continue }
                if settingsInspectionOutcome == nil { settingsInspectionOutcome = .expired }
                retirePendingSettingsInspection(sent.original)
                try releaseSettingsSend(.inspection(sent.original.input.queryID), messageID: sent.envelope?.header.messageID,
                                        started: sent.sendStarted, accepted: sent.accepted)
            case .update(let sent):
                let live = try isCurrentSettingsUpdateOriginal(sent.original) || sent.original.consumed
                guard !sent.sendStarted || sent.sendReturned,
                      !live || ContinuousClock.now >= sent.original.deadline else { continue }
                if sent.original.record.status == nil { sent.original.record.status = .expired }
                retirePendingSettingsUpdate(sent.original)
                try releaseSettingsSend(.update(sent.original.input.requestID), messageID: sent.envelope?.header.messageID,
                                        started: sent.sendStarted, accepted: sent.accepted)
            }
        }
        releaseSettingsAssociations()
        try await sendPendingSettingsAccess(socket: socket)
        try await sendPendingSettingsBind(socket: socket)
        try await sendPendingSettingsSnapshot(socket: socket)
        try await sendPendingSettingsInspection(socket: socket)
        try await sendPendingSettingsUpdate(socket: socket)
        let now = try WireID.now()
        let ordinaryIDs = Set(outbound.keys.compactMap { key -> UUID? in
            if case .ordinary(let id) = key { return id }
            return nil
        })
        let rows = try await store.outbound(now: now, excluding: ordinaryIDs)
        for row in rows.prefix(max(0, 16 - outbound.count)) {
            try Task.checkCancellation()
            try await store.checkActive()
            try row.envelope.header.validate(now: WireID.now())
            guard outbound.count < 16 else { break }
            guard outboundSocket === socket, !hasOutboundMessage(row.id) else { throw WireError.conflict }
            let ordinary = OrdinarySend()
            outbound[.ordinary(row.id)] = .ordinary(ordinary)
            try await store.attempted(row.id, now: now)
            guard outboundSocket === socket, case .ordinary(let registered) = outbound[.ordinary(row.id)],
                  registered === ordinary else { throw WireError.conflict }
            ordinary.started = .now
            do {
                try await socket.send(.object(["type": .string("send"), "schema_version": .unsigned(1), "envelope": row.envelope.wire]))
                ordinary.sendReturned = true
                try completeOrdinarySend(ordinary, id: row.id, socket: socket)
            } catch {
                ordinary.sendReturned = true
                ordinary.failed = true
                retireSettingsScope(socket: socket)
                throw error
            }
        }
        if reconnectForCapacity && outbound.isEmpty && inboxDrained && rows.isEmpty { throw WireError.network }
    }

    private func synchronizeOne() async throws {
        let known = try await store.synchronizationConversations()
        guard !known.isEmpty else { return }
        let others = known.filter { $0 != selectedConversation }
        let requested: UUID
        if let selectedConversation, known.contains(selectedConversation), preferSelectedSync || others.isEmpty {
            requested = selectedConversation
            preferSelectedSync = false
        } else {
            let candidates = others.isEmpty ? known : others
            requested = candidates[syncPosition % candidates.count]
            syncPosition = (syncPosition + 1) % candidates.count
            preferSelectedSync = true
        }
        try await synchronize(requested)
    }

    private func handle(_ frame: RelayFrame, socket: RelayConnection) async throws {
        switch frame {
        case .accepted(let id):
            guard outboundSocket === socket else { throw WireError.conflict }
            if case .ordinary(let ordinary) = outbound[.ordinary(id)] {
                guard ordinary.started != nil else { throw WireError.conflict }
                try await store.relayAccepted(id)
                ordinary.accepted = true
                try completeOrdinarySend(ordinary, id: id, socket: socket)
            } else if abandonedSettingsMessages.remove(id) != nil {
                return
            } else {
                if let access = outbound.values.compactMap({ entry -> SettingsAccessSend? in
                    if case .access(let sent) = entry, sent.envelope?.header.messageID == id { return sent }
                    return nil
                }).first {
                    guard access.sendStarted, !access.accepted, access.original.scope.socket === socket else { throw WireError.conflict }
                    access.accepted = true
                    try completeSettingsAccessSend(access)
                } else if let bind = outbound.values.compactMap({ entry -> SettingsBindSend? in
                    if case .bind(let sent) = entry, sent.envelope?.header.messageID == id { return sent }
                    return nil
                }).first {
                    guard bind.sendStarted, !bind.accepted,
                          bind.original.scope.socket === socket else { throw WireError.conflict }
                    bind.accepted = true
                    try completeSettingsBindSend(bind)
                } else if let snapshot = outbound.values.compactMap({ entry -> SettingsSnapshotSend? in
                    if case .snapshot(let sent) = entry, sent.envelope?.header.messageID == id { return sent }
                    return nil
                }).first {
                    guard snapshot.sendStarted, !snapshot.accepted,
                          snapshot.original.scope.socket === socket else { throw WireError.conflict }
                    snapshot.accepted = true
                    try completeSettingsSnapshotSend(snapshot)
                } else if let inspection = outbound.values.compactMap({ entry -> SettingsInspectionSend? in
                    if case .inspection(let sent) = entry, sent.envelope?.header.messageID == id { return sent }
                    return nil
                }).first {
                    guard inspection.sendStarted, !inspection.accepted,
                          inspection.original.scope.socket === socket else { throw WireError.conflict }
                    inspection.accepted = true
                    try completeSettingsInspectionSend(inspection)
                } else {
                    guard let update = outbound.values.compactMap({ entry -> SettingsUpdateSend? in
                        if case .update(let sent) = entry, sent.envelope?.header.messageID == id { return sent }
                        return nil
                    }).first, update.sendStarted, !update.accepted,
                          update.original.scope.socket === socket else { throw WireError.conflict }
                    update.accepted = true
                    try completeSettingsUpdateSend(update)
                }
            }
        case .acknowledged(let id):
            guard acknowledgements.remove(id) != nil else { throw WireError.conflict }
        case .pong: break
        case .delivery(let envelope):
            try await store.checkActive()
            guard let host = try await store.host(), acknowledgements.count < 128 else { throw WireError.identity }
            do { _ = try await identity.receive(envelope, host: host, into: store, now: WireID.now()) }
            catch WireError.queueFull {
                status = "Local inbox is full; delivery remains on the relay"
                reconnectForCapacity = true
                return
            }
            acknowledgements.insert(envelope.header.messageID)
            try await socket.send(.object(["type": .string("acknowledge"), "schema_version": .unsigned(1), "message_id": .id(envelope.header.messageID)]))
            _ = try await drainInbox()
        }
    }

    private func drainInbox() async throws -> Bool {
        do { try await applyInbox(); return true }
        catch WireError.queueFull {
            status = "Local queue is full; preserving pending work"
            return false
        }
    }

    private func applyInbox() async throws {
        guard !processingInbox else { return }
        processingInbox = true
        defer { processingInbox = false }
        guard let host = try await store.host() else { return }
        while !Task.isCancelled {
            let records = try await store.pendingInbox()
            if records.isEmpty { return }
            for record in records {
                let packet = try WireValue.parse(record.payload).object(keys: ["schema_version", "payload"], required: ["schema_version", "payload"])
                guard try packet.required("schema_version").integer == 1 else { throw WireError.invalid }
                let payload = try packet.required("payload")
                switch try payload.field("kind").text {
                case "command_result":
                    let fields = try payload.object(keys: ["kind", "command_id", "state", "result"], required: ["kind", "command_id", "state"])
                    try await store.completeResult(record.id, commandID: fields.required("command_id").identifier, state: fields.required("state").text, result: fields["result"], host: host)
                case "sync_batch":
                    let fields = try payload.object(keys: ["kind", "batch"], required: ["kind", "batch"])
                    let batch = try WireSyncBatch(fields.required("batch"))
                    let now = try WireID.now()
                    let ack = DevicePackets.packet(.object(["kind": .string("sync_acknowledgement"), "batch_message_id": .id(record.id), "cursor": DevicePackets.cursor(batch.conversationID, batch.through), "last_event_id": batch.events.last.map { .id($0.id) } ?? .null]))
                    var replies = [try await identity.seal(ack.encoded(), recipient: host, now: now)]
                    if batch.hasMore {
                        let durable = try await store.cursor(batch.conversationID) ?? 0
                        replies.append(try await identity.seal(DevicePackets.sync(batch.conversationID, after: max(durable, batch.through)).encoded(), recipient: host, now: now))
                    }
                    do { try await store.apply(batch, messageID: record.id, replies: replies, host: host) }
                    catch WireError.gap {
                        let cursor = try await store.cursor(batch.conversationID) ?? 0
                        let request = try await identity.seal(DevicePackets.sync(batch.conversationID, after: cursor).encoded(), recipient: host, now: now)
                        try await store.rejectGap(record.id, conversation: batch.conversationID, request: request, host: host)
                    }
                case "revoked":
                    let fields = try payload.object(keys: ["kind", "device_id", "revoked_at"], required: ["kind", "device_id", "revoked_at"])
                    let own = identity.publicKeys
                    guard try fields.required("device_id").identifier == own.deviceID else { throw WireError.identity }
                    _ = try WireID.timestamp(fields.required("revoked_at"))
                    settingsAccessDisplayCache = nil
                    settingsQueryResultCache = nil
                    settingsSnapshotDisplayCache = nil
                    settingsInspectionDisplayCache = nil
                    retireSettingsScope()
                    try await store.disable(messageID: record.id, host: host)
                    throw WireError.revoked
                default: throw WireError.invalid
                }
            }
        }
    }

    private func checkSettingsConnection(socket: RelayConnection, epoch captured: UUID) throws {
        try Task.checkCancellation()
        guard epoch == captured, connection === socket, stopping == nil, !disabling else { throw WireError.unavailable }
    }

    private func isCurrentSettingsScope(_ scope: SettingsConnectionScope) -> Bool {
        settingsScope === scope && connection === scope.socket && epoch == scope.epoch
            && stopping == nil && !disabling && !Task.isCancelled
    }

    private func installSettingsScope(socket: RelayConnection, epoch captured: UUID) async throws {
        try checkSettingsConnection(socket: socket, epoch: captured)
        let host = try await store.host()
        try checkSettingsConnection(socket: socket, epoch: captured)
        guard let host else { throw WireError.identity }
        try await store.checkActive(host: host)
        try checkSettingsConnection(socket: socket, epoch: captured)
        let localID = try await store.identityID()
        try checkSettingsConnection(socket: socket, epoch: captured)
        guard localID == identity.publicKeys.deviceID else { throw WireError.identity }
        try await store.checkActive(host: host)
        try checkSettingsConnection(socket: socket, epoch: captured)
        guard settingsScope == nil, pendingSettingsRequest == nil, settingsReceive == nil else { throw WireError.queueFull }
        settingsScope = SettingsConnectionScope(socket: socket, host: host, localID: identity.publicKeys.deviceID, epoch: captured)
    }

    private func retirePendingSettingsAccess(_ pending: PendingSettingsAccess) {
        pending.retired = true
        switch pending.phase {
        case .preparing, .receiving: return
        case .waiting, .prepared: break
        }
        if case .access(let sent) = outbound[.access(pending.request.queryID)], sent.original === pending,
           !sent.sendStarted {
            outbound.removeValue(forKey: .access(pending.request.queryID))
        }
        if pendingSettingsAccess === pending { pendingSettingsRequest = nil }
    }

    private func retirePendingSettingsBind(_ pending: PendingSettingsBind) {
        pending.retired = true
        if !pending.consumed { pending.association.retired = true }
        switch pending.phase {
        case .preparing, .receiving: return
        case .waiting, .prepared: break
        }
        if case .bind(let sent) = outbound[.bind(pending.association.input.bindingRequestID)],
           sent.original === pending, !sent.sendStarted {
            outbound.removeValue(forKey: .bind(pending.association.input.bindingRequestID))
        }
        if pendingSettingsBind === pending { pendingSettingsRequest = nil }
    }

    private func retireSettingsScope(socket: RelayConnection? = nil) {
        if let scope = settingsScope, socket == nil || scope.socket === socket {
            settingsScope = nil
        }
        if let pending = pendingSettingsAccess, socket == nil || pending.scope.socket === socket {
            retirePendingSettingsAccess(pending)
        }
        if let pending = pendingSettingsBind, socket == nil || pending.scope.socket === socket {
            retirePendingSettingsBind(pending)
        }
        if let pending = pendingSettingsSnapshot, socket == nil || pending.scope.socket === socket {
            retirePendingSettingsSnapshot(pending)
        }
        if let pending = pendingSettingsInspection, socket == nil || pending.scope.socket === socket {
            retirePendingSettingsInspection(pending)
        }
        if let pending = pendingSettingsUpdate, socket == nil || pending.scope.socket === socket {
            retirePendingSettingsUpdate(pending)
        }
        if let association = settingsBindingAssociation,
           socket == nil || association.scope?.socket === socket {
            association.retired = true
        }
        releaseSettingsAssociations()
    }

    private func isCurrentSettingsOriginal(_ pending: PendingSettingsAccess) throws -> Bool {
        guard pendingSettingsAccess === pending, !pending.retired, !pending.consumed,
              isCurrentSettingsScope(pending.scope) else { return false }
        let monotonic = ContinuousClock.now
        let wall = try WireID.now()
        return wall >= pending.wallFloor && wall < pending.request.time.expiresAt
            && monotonic >= pending.monotonicFloor && monotonic < pending.deadline
    }

    private func checkSettingsStore(_ pending: PendingSettingsAccess) async throws -> Bool {
        guard try isCurrentSettingsOriginal(pending) else { return false }
        let scope = pending.scope
        try await store.checkActive(host: scope.host)
        guard try isCurrentSettingsOriginal(pending) else { return false }
        let localID = try await store.identityID()
        guard try isCurrentSettingsOriginal(pending) else { return false }
        guard localID == scope.localID else { throw WireError.identity }
        return true
    }

    private func prepareSettingsAccessEnvelope(_ pending: PendingSettingsAccess) async throws -> EncryptedEnvelope? {
        guard try isCurrentSettingsOriginal(pending) else { return nil }
        guard try await checkSettingsStore(pending), try isCurrentSettingsOriginal(pending) else { return nil }
        let envelope = try await identity.sealSettings(pending.request, recipient: pending.scope.host)
        guard try isCurrentSettingsOriginal(pending) else { return nil }
        guard try await checkSettingsStore(pending), try isCurrentSettingsOriginal(pending) else { return nil }
        guard envelope.header.sender == pending.scope.localID,
              envelope.header.recipient == pending.request.hostID,
              envelope.header.expires == pending.request.time.expiresAt,
              envelope.header.created >= pending.wallFloor,
              envelope.header.expires > envelope.header.created,
              envelope.header.expires - envelope.header.created <= 30_000 else { throw WireError.identity }
        return envelope
    }

    private func preparePendingSettingsAccess(socket: RelayConnection) async throws {
        guard let pending = pendingSettingsAccess, pending.scope.socket === socket else { return }
        guard try isCurrentSettingsOriginal(pending) else {
            if settingsAccessOutcome == nil { settingsAccessOutcome = .expired }
            retirePendingSettingsAccess(pending)
            return
        }
        guard case .waiting = pending.phase else { return }
        pending.phase = .preparing
        do {
            do {
                let envelope = try await prepareSettingsAccessEnvelope(pending)
                if let envelope, try isCurrentSettingsOriginal(pending) {
                    guard case .access(let sent) = outbound[.access(pending.request.queryID)],
                          sent.original === pending, sent.envelope == nil, !sent.sendStarted,
                          envelope.header.messageID != pending.request.queryID,
                          !hasOutboundMessage(envelope.header.messageID),
                          !acknowledgements.contains(envelope.header.messageID) else { throw WireError.conflict }
                    sent.envelope = envelope
                    pending.phase = .prepared(envelope)
                }
            }
            if case .preparing = pending.phase {
                pending.phase = .waiting
                retirePendingSettingsAccess(pending)
            }
        } catch {
            pending.phase = .waiting
            settingsAccessOutcome = .interrupted
            retirePendingSettingsAccess(pending)
            retireSettingsScope(socket: pending.scope.socket)
            throw error
        }
    }

    private func isCurrentSettingsBindOriginal(_ pending: PendingSettingsBind) throws -> Bool {
        let association = pending.association
        guard pendingSettingsBind === pending, !pending.retired, !pending.consumed,
              settingsBindingAssociation === association, !association.retired,
              association.scope === pending.scope, association.requestCaptured,
              association.request != nil, isCurrentSettingsScope(pending.scope),
              settingsAccessDisplayCache === pending.cache, pending.cache.scope === pending.scope,
              try isCurrentSettingsAccessDisplay(pending.cache) else { return false }
        let monotonic = ContinuousClock.now
        let wall = try WireID.now()
        return wall >= association.wallFloor && wall < association.input.time.expiresAt
            && monotonic >= association.monotonicFloor && monotonic < association.deadline
    }

    private func checkSettingsBindStore(_ pending: PendingSettingsBind) async throws -> Bool {
        guard try isCurrentSettingsBindOriginal(pending) else { return false }
        let scope = pending.scope
        try await store.checkActive(host: scope.host)
        guard try isCurrentSettingsBindOriginal(pending) else { return false }
        let localID = try await store.identityID()
        guard try isCurrentSettingsBindOriginal(pending) else { return false }
        guard localID == scope.localID else { throw WireError.identity }
        return true
    }

    private func prepareSettingsBindEnvelope(_ pending: PendingSettingsBind) async throws -> EncryptedEnvelope? {
        guard try isCurrentSettingsBindOriginal(pending) else { return nil }
        guard try await checkSettingsBindStore(pending), try isCurrentSettingsBindOriginal(pending),
              let request = pending.association.request else { return nil }
        let envelope = try await identity.sealSettings(request, recipient: pending.scope.host)
        guard try isCurrentSettingsBindOriginal(pending) else { return nil }
        guard try await checkSettingsBindStore(pending), try isCurrentSettingsBindOriginal(pending) else { return nil }
        guard envelope.header.sender == pending.scope.localID,
              envelope.header.recipient == request.hostID,
              envelope.header.expires == request.time.expiresAt,
              envelope.header.created >= pending.association.wallFloor,
              envelope.header.expires > envelope.header.created,
              envelope.header.expires - envelope.header.created <= 30_000 else { throw WireError.identity }
        return envelope
    }

    private func preparePendingSettingsBind(socket: RelayConnection) async throws {
        guard let pending = pendingSettingsBind, pending.scope.socket === socket else { return }
        guard try isCurrentSettingsBindOriginal(pending) else {
            retirePendingSettingsBind(pending)
            return
        }
        guard case .waiting = pending.phase else { return }
        pending.phase = .preparing
        do {
            do {
                let envelope = try await prepareSettingsBindEnvelope(pending)
                if let envelope, try isCurrentSettingsBindOriginal(pending) {
                    guard case .bind(let sent) = outbound[.bind(pending.association.input.bindingRequestID)],
                          sent.original === pending, sent.envelope == nil, !sent.sendStarted,
                          envelope.header.messageID != pending.association.input.bindingRequestID,
                          !hasOutboundMessage(envelope.header.messageID),
                          !acknowledgements.contains(envelope.header.messageID) else { throw WireError.conflict }
                    sent.envelope = envelope
                    pending.phase = .prepared(envelope)
                }
            }
            if case .preparing = pending.phase {
                pending.phase = .waiting
                retirePendingSettingsBind(pending)
            }
        } catch {
            pending.phase = .waiting
            retirePendingSettingsBind(pending)
            retireSettingsScope(socket: socket)
            throw error
        }
    }

    private func claimSettingsReceive(socket: RelayConnection) throws -> SettingsReceiveTicket {
        try Task.checkCancellation()
        guard let scope = settingsScope, scope.socket === socket, isCurrentSettingsScope(scope) else { throw WireError.unavailable }
        guard settingsReceive == nil else { throw WireError.queueFull }
        let ticket = SettingsReceiveTicket(scope: scope)
        settingsReceive = SettingsReceiveOwner(ticket: ticket)
        return ticket
    }

    private func receiveSettingsOwnedFrame(socket: RelayConnection, ticket: SettingsReceiveTicket) async throws {
        let received = try await socket.receive()
        guard settingsReceive?.ticket === ticket, ticket.scope.socket === socket,
              isCurrentSettingsScope(ticket.scope) else { throw WireError.unavailable }
        guard case .delivery(let envelope) = received.frame else {
            try await handle(received.frame, socket: socket)
            return
        }
        guard acknowledgements.count < 128 else { throw WireError.identity }
        do {
            if pendingSettingsUpdate != nil {
                let result = try await classifySettingsUpdateReceive(received, ticket: ticket)
                switch result {
                case .qualified(let qualification): try await adoptSettingsUpdateOutcome(qualification)
                case .durable, .duplicate, .refused: break
                }
            } else if pendingSettingsInspection != nil {
                let result = try await classifySettingsInspectionReceive(received, ticket: ticket)
                switch result {
                case .qualified(let qualification): try await adoptSettingsInspectionDisplay(qualification)
                case .durable, .duplicate, .refused: break
                }
            } else if pendingSettingsSnapshot != nil {
                let result = try await classifySettingsSnapshotReceive(received, ticket: ticket)
                switch result {
                case .qualified(let qualification): try await adoptSettingsSnapshotDisplay(qualification)
                case .durable, .duplicate, .refused: break
                }
            } else if pendingSettingsBind != nil {
                let result = try await classifySettingsBindReceive(received, ticket: ticket)
                switch result {
                case .qualified(let qualification): try await adoptSettingsBoundParent(qualification)
                case .durable, .duplicate, .refused: break
                }
            } else {
                let result = try await classifySettingsAccessReceive(received, ticket: ticket)
                switch result {
                case .qualified(let qualification): try await adoptSettingsAccessDisplay(qualification)
                case .durable, .duplicate, .refused: break
                }
            }
        } catch WireError.queueFull {
            status = "Local inbox is full; delivery remains on the relay"
            reconnectForCapacity = true
            return
        } catch let error as WireError where [.identity, .size, .expired, .invalid, .signature, .conflict, .unavailable].contains(error) {
            try await refuseSettingsReply(ticket: ticket, socket: socket, failure: error)
        }
        acknowledgements.insert(envelope.header.messageID)
        try await socket.send(.object(["type": .string("acknowledge"), "schema_version": .unsigned(1), "message_id": .id(envelope.header.messageID)]))
        _ = try await drainInbox()
    }

    private func refuseSettingsReply(ticket: SettingsReceiveTicket, socket: RelayConnection, failure: WireError) async throws {
        guard settingsReceive?.ticket === ticket, ticket.scope.socket === socket,
              isCurrentSettingsScope(ticket.scope) else { throw failure }
        try await store.checkActive(host: ticket.scope.host)
        let localID = try await store.identityID()
        guard localID == ticket.scope.localID, localID == identity.publicKeys.deviceID else { throw WireError.identity }
        guard settingsReceive?.ticket === ticket, isCurrentSettingsScope(ticket.scope) else { throw failure }
    }

    private func sameSettingsEnvelope(_ lhs: EncryptedEnvelope, _ rhs: EncryptedEnvelope) -> Bool {
        lhs.header.canonical == rhs.header.canonical
            && lhs.ciphertext == rhs.ciphertext && lhs.signature == rhs.signature
    }

    private func beginSettingsAccessReceive(_ received: ReceivedRelayFrame,
                                           ticket: SettingsReceiveTicket) throws -> SettingsAccessReceiveAttempt {
        try Task.checkCancellation()
        guard let owner = settingsReceive, owner.ticket === ticket,
              isCurrentSettingsScope(ticket.scope) else { throw WireError.unavailable }
        guard owner.attempt == nil else { throw WireError.conflict }
        guard case .delivery = received.frame else { throw WireError.invalid }
        let original: SettingsAccessOriginal
        if let pending = pendingSettingsAccess {
            guard pending.scope === ticket.scope else { throw WireError.conflict }
            let current = try isCurrentSettingsOriginal(pending)
            if case .prepared(let prepared) = pending.phase, current,
               case .access(let sent) = outbound[.access(pending.request.queryID)], sent.original === pending,
               sent.sendStarted, let bound = sent.envelope, sameSettingsEnvelope(bound, prepared) {
                pending.phase = .receiving(prepared)
                original = .prepared(pending, prepared)
            } else {
                if !current { retirePendingSettingsAccess(pending) }
                original = .absent
            }
        } else {
            original = .absent
        }
        let attempt = SettingsAccessReceiveAttempt(issuer: ObjectIdentifier(self), ticket: ticket,
                                                   received: received, original: original)
        owner.attempt = .access(attempt)
        return attempt
    }

    private func checkSettingsAccessAttempt(_ attempt: SettingsAccessReceiveAttempt) throws {
        try Task.checkCancellation()
        guard attempt.issuer == ObjectIdentifier(self),
              let owner = settingsReceive, owner.ticket === attempt.ticket,
              case .access(let registered) = owner.attempt, registered === attempt,
              isCurrentSettingsScope(attempt.ticket.scope) else {
            throw WireError.unavailable
        }
    }

    private func currentSettingsAccessOriginal(_ attempt: SettingsAccessReceiveAttempt) throws -> PendingSettingsAccess? {
        guard case .prepared(let pending, let prepared) = attempt.original else { return nil }
        guard pendingSettingsAccess === pending, pending.scope === attempt.ticket.scope,
              case .receiving(let retained) = pending.phase,
              sameSettingsEnvelope(retained, prepared) else {
            retireSettingsScope(socket: attempt.ticket.scope.socket)
            throw WireError.conflict
        }
        guard try isCurrentSettingsOriginal(pending) else {
            retirePendingSettingsAccess(pending)
            return nil
        }
        return pending
    }

    private func checkSettingsAccessReceiveStore(_ attempt: SettingsAccessReceiveAttempt) async throws {
        try checkSettingsAccessAttempt(attempt)
        _ = try currentSettingsAccessOriginal(attempt)
        let scope = attempt.ticket.scope
        try await store.checkActive(host: scope.host)
        try checkSettingsAccessAttempt(attempt)
        _ = try currentSettingsAccessOriginal(attempt)
        let localID = try await store.identityID()
        try checkSettingsAccessAttempt(attempt)
        _ = try currentSettingsAccessOriginal(attempt)
        guard localID == scope.localID else { throw WireError.identity }
    }

    private func matchesSettingsAccessOriginal(_ attempt: SettingsAccessReceiveAttempt,
                                              metadata: ReplicaSettingsReplyMetadata) throws -> Bool {
        guard let pending = try currentSettingsAccessOriginal(attempt),
              case .prepared(_, let prepared) = attempt.original,
              case .query(let queryID) = metadata.correlation else { return false }
        let scope = attempt.ticket.scope
        return metadata.inReplyTo == prepared.header.messageID && queryID == pending.request.queryID
            && pending.request.hostID == scope.host.deviceID
            && metadata.header.sender == scope.host.deviceID && metadata.header.recipient == scope.localID
            && prepared.header.sender == scope.localID && prepared.header.recipient == scope.host.deviceID
            && prepared.header.expires == pending.request.time.expiresAt
            && prepared.header.created >= pending.wallFloor
            && prepared.header.expires > prepared.header.created
            && prepared.header.expires - prepared.header.created <= 30_000
    }

    private func classifySettingsAccessReceive(_ received: ReceivedRelayFrame,
                                              ticket: SettingsReceiveTicket) async throws -> SettingsAccessReceiveResult {
        let attempt = try beginSettingsAccessReceive(received, ticket: ticket)
        try await checkSettingsAccessReceiveStore(attempt)
        try checkSettingsAccessAttempt(attempt)
        let classified = try await identity.receiveClassified(received, host: ticket.scope.host, into: store)
        attempt.classified = classified
        try checkSettingsAccessAttempt(attempt)
        _ = try currentSettingsAccessOriginal(attempt)
        guard classified.matchesReceivedFrame(received) else { throw WireError.identity }
        try await checkSettingsAccessReceiveStore(attempt)
        try checkSettingsAccessAttempt(attempt)
        let verified: VerifiedReplicaSettingsReply
        switch classified.result {
        case .ordinary, .refused:
            return .durable(classified)
        case .settingsAccess(let proof), .settingsBind(let proof), .settingsSnapshot(let proof), .settingsInspection(let proof), .settingsUpdate(let proof):
            verified = proof
        }
        guard verified.matchesVerifiedPeer(ticket.scope.host, localID: ticket.scope.localID) else {
            throw WireError.identity
        }
        let committed = try await verified.commitMetadata(into: store)
        attempt.committed = committed
        try checkSettingsAccessAttempt(attempt)
        _ = try currentSettingsAccessOriginal(attempt)
        try await checkSettingsAccessReceiveStore(attempt)
        try checkSettingsAccessAttempt(attempt)
        guard committed.matchesVerifiedPeer(ticket.scope.host, localID: ticket.scope.localID) else {
            throw WireError.identity
        }
        guard case .settingsAccess = classified.result,
              try matchesSettingsAccessOriginal(attempt, metadata: verified.metadata),
              try matchesSettingsAccessOriginal(attempt, metadata: committed.metadata) else {
            return .refused(committed)
        }
        guard case .fresh = committed.disposition else { return .duplicate(committed) }
        guard case .unissued = attempt.qualification else { throw WireError.conflict }
        let qualification = ReplicaSettingsAccessQualification(attempt: attempt, committed: committed)
        attempt.qualification = .issued(ObjectIdentifier(qualification))
        return .qualified(qualification)
    }

    fileprivate func checkSettingsAccessQualification(_ qualification: ReplicaSettingsAccessQualification,
                                                     committed: CommittedReplicaSettingsReply) throws {
        let attempt = qualification.attempt
        try checkSettingsAccessAttempt(attempt)
        guard qualification.committed === committed, attempt.committed === committed,
              case .issued(let identifier) = attempt.qualification,
              identifier == ObjectIdentifier(qualification),
              case .fresh = committed.disposition,
              let classified = attempt.classified, classified.matchesReceivedFrame(attempt.received),
              case .settingsAccess(let verified) = classified.result,
              verified.matchesVerifiedPeer(attempt.ticket.scope.host, localID: attempt.ticket.scope.localID),
              committed.matchesVerifiedPeer(attempt.ticket.scope.host, localID: attempt.ticket.scope.localID),
              try matchesSettingsAccessOriginal(attempt, metadata: verified.metadata),
              try matchesSettingsAccessOriginal(attempt, metadata: committed.metadata) else {
            throw WireError.unavailable
        }
    }

    fileprivate func consumeSettingsAccessQualification(_ qualification: ReplicaSettingsAccessQualification,
                                                       committed: CommittedReplicaSettingsReply) throws {
        try checkSettingsAccessQualification(qualification, committed: committed)
        guard let pending = try currentSettingsAccessOriginal(qualification.attempt) else {
            throw WireError.unavailable
        }
        qualification.attempt.qualification = .consumed
        pending.consumed = true
        retirePendingSettingsAccess(pending)
    }

    private func recheckSettingsAccessQualification(_ qualification: ReplicaSettingsAccessQualification) async throws {
        try checkSettingsAccessQualification(qualification, committed: qualification.committed)
        try await checkSettingsAccessReceiveStore(qualification.attempt)
        try checkSettingsAccessQualification(qualification, committed: qualification.committed)
    }

    @discardableResult
    func checkSettingsAccessDisplayProjection(_ qualification: ReplicaSettingsAccessQualification,
                                             committed: CommittedReplicaSettingsReply) throws -> RemoteSettingsAccessRequest {
        try checkSettingsAccessQualification(qualification, committed: committed)
        guard let pending = try currentSettingsAccessOriginal(qualification.attempt) else {
            throw WireError.unavailable
        }
        let header = committed.metadata.header
        guard header.created + settingsClockSkewMilliseconds >= pending.wallFloor,
              header.expires <= pending.request.time.expiresAt else {
            throw WireError.invalid
        }
        return pending.request
    }

    private func adoptSettingsAccessDisplay(_ qualification: ReplicaSettingsAccessQualification) async throws {
        try await recheckSettingsAccessQualification(qualification)
        try checkSettingsAccessDisplayProjection(qualification, committed: qualification.committed)
        guard let pending = try currentSettingsAccessOriginal(qualification.attempt) else {
            throw WireError.unavailable
        }
        let scope = qualification.attempt.ticket.scope
        let wallFloor = pending.wallFloor
        let monotonicFloor = pending.monotonicFloor
        let queryExpiresAt = pending.request.time.expiresAt
        let queryDeadline = pending.deadline
        let header = qualification.committed.metadata.header
        let projected = try qualification.committed.projectAccessDisplay(qualifiedBy: qualification, owner: self)
        let projection: ReplicaSettingsAccessProjection
        switch projected {
        case .access(let access): projection = access
        case .result(let result):
            let wall = try WireID.now()
            let monotonic = ContinuousClock.now
            guard isCurrentSettingsScope(scope), wall >= wallFloor, wall < queryExpiresAt,
                  monotonic >= monotonicFloor, monotonic < queryDeadline else { throw WireError.expired }
            if case .denied(let reason) = result, reason == .grantUnavailable || reason == .grantChanged {
                settingsAccessDisplayCache = nil
            }
            settingsQueryResultCache = (pending.handle, result, header.created)
            return
        }
        let packetExpiry = min(queryExpiresAt, header.expires)
        let expiresAt = projection.grantExpiresAt.map { min(packetExpiry, $0) } ?? packetExpiry
        guard queryExpiresAt > wallFloor, queryExpiresAt - wallFloor <= 30_000,
              queryDeadline == monotonicFloor.advanced(by: .milliseconds(Int64(queryExpiresAt - wallFloor))),
              expiresAt > wallFloor, expiresAt <= queryExpiresAt else { throw WireError.expired }
        let clip = queryExpiresAt - expiresAt
        guard clip <= 30_000 else { throw WireError.expired }
        let deadline = queryDeadline.advanced(by: .milliseconds(-Int64(clip)))
        let wall = try WireID.now()
        let monotonic = ContinuousClock.now
        guard isCurrentSettingsScope(scope), wall >= wallFloor, wall < expiresAt,
              monotonic >= monotonicFloor, monotonic < deadline else { throw WireError.expired }
        if let association = settingsBindingAssociation, association.accessHandle !== pending.handle {
            association.retired = true
        }
        settingsSnapshotDisplayCache = nil
        settingsInspectionDisplayCache = nil
        settingsAccessDisplayCache = SettingsAccessDisplayCache(
            scope: scope, projection: projection, requestHandle: pending.handle,
            wallFloor: wallFloor, monotonicFloor: monotonicFloor,
            queryExpiresAt: queryExpiresAt, queryDeadline: queryDeadline,
            packetExpiresAt: header.expires, grantExpiresAt: projection.grantExpiresAt,
            expiresAt: expiresAt, deadline: deadline, observedAt: header.created
        )
    }

    private func isCurrentSettingsAccessDisplay(_ cache: SettingsAccessDisplayCache) throws -> Bool {
        guard let scope = cache.scope, isCurrentSettingsScope(scope),
              scope.host == cache.host, scope.localID == cache.localID else { return false }
        let wall = try WireID.now()
        let monotonic = ContinuousClock.now
        return wall >= cache.wallFloor && wall < cache.expiresAt
            && monotonic >= cache.monotonicFloor && monotonic < cache.deadline
    }

    private func settingsAccessDisplayView(_ cache: SettingsAccessDisplayCache?, snapshot: ReplicaSnapshot,
                                          initiallyCurrent: Bool) async throws -> ReplicaSettingsAccessDisplayView? {
        guard settingsAccessDisplayCache === cache else { throw WireError.unavailable }
        guard let cache else { return nil }
        guard !snapshot.revoked, snapshot.host == cache.host,
              identity.publicKeys.deviceID == cache.localID else {
            settingsAccessDisplayCache = nil
            settingsSnapshotDisplayCache = nil
            settingsInspectionDisplayCache = nil
            return nil
        }
        var current = try isCurrentSettingsAccessDisplay(cache) && initiallyCurrent
        do {
            try await store.checkActive(host: cache.host)
        } catch let error as WireError where error == .identity || error == .revoked {
            if settingsAccessDisplayCache === cache {
                settingsAccessDisplayCache = nil
                settingsSnapshotDisplayCache = nil
                settingsInspectionDisplayCache = nil
            }
            throw error
        }
        guard settingsAccessDisplayCache === cache else { throw WireError.unavailable }
        let afterHostCheck = try isCurrentSettingsAccessDisplay(cache)
        current = current && afterHostCheck
        let localID = try await store.identityID()
        guard settingsAccessDisplayCache === cache else { throw WireError.unavailable }
        let afterIdentityCheck = try isCurrentSettingsAccessDisplay(cache)
        current = current && afterIdentityCheck
        guard localID == cache.localID else {
            settingsAccessDisplayCache = nil
            settingsSnapshotDisplayCache = nil
            settingsInspectionDisplayCache = nil
            throw WireError.identity
        }
        var requestsAvailable = false
        if let scope = cache.scope, isCurrentSettingsScope(scope), scope.host == cache.host,
           scope.localID == cache.localID, cache.projection.generation != nil,
           let grantExpiresAt = cache.grantExpiresAt {
            requestsAvailable = try WireID.now() < grantExpiresAt
        }
        return ReplicaSettingsAccessDisplayView(display: cache.projection.display, observedAt: cache.observedAt,
                                               expiresAt: cache.expiresAt,
                                               freshness: current ? .currentConnectionObservation : .stale,
                                               requestHandle: cache.requestHandle,
                                               requestsAvailable: requestsAvailable)
    }

    private func isCurrentSettingsParent(_ association: SettingsBindingAssociation,
                                         scope: SettingsConnectionScope) throws -> Bool {
        guard settingsBindingAssociation === association, association.scope === scope, !association.retired,
              isCurrentSettingsScope(scope), let parent = association.parent,
              let deadline = association.parentDeadline else { return false }
        let monotonic = ContinuousClock.now
        let wall = try WireID.now()
        return wall >= association.wallFloor && wall < parent.expiresAt
            && monotonic >= association.monotonicFloor && monotonic < deadline
    }

    private func isCurrentSettingsSnapshotOriginal(_ pending: PendingSettingsSnapshot) throws -> Bool {
        guard pendingSettingsSnapshot === pending, !pending.retired, !pending.consumed,
              isCurrentSettingsScope(pending.scope),
              try isCurrentSettingsParent(pending.association, scope: pending.scope) else { return false }
        let monotonic = ContinuousClock.now
        let wall = try WireID.now()
        return wall >= pending.wallFloor && wall < pending.input.time.expiresAt
            && monotonic >= pending.monotonicFloor && monotonic < pending.deadline
    }

    private func checkSettingsSnapshotStore(_ pending: PendingSettingsSnapshot) async throws -> Bool {
        guard try isCurrentSettingsSnapshotOriginal(pending) else { return false }
        let scope = pending.scope
        try await store.checkActive(host: scope.host)
        guard try isCurrentSettingsSnapshotOriginal(pending) else { return false }
        let localID = try await store.identityID()
        guard try isCurrentSettingsSnapshotOriginal(pending) else { return false }
        guard localID == scope.localID else { throw WireError.identity }
        return true
    }

    private func prepareSettingsSnapshotEnvelope(_ pending: PendingSettingsSnapshot) async throws -> EncryptedEnvelope? {
        guard try isCurrentSettingsSnapshotOriginal(pending) else { return nil }
        guard try await checkSettingsSnapshotStore(pending), try isCurrentSettingsSnapshotOriginal(pending) else { return nil }
        guard let request = pending.request else { throw WireError.unavailable }
        let envelope = try await identity.sealSettings(request, recipient: pending.scope.host)
        guard try isCurrentSettingsSnapshotOriginal(pending) else { return nil }
        guard try await checkSettingsSnapshotStore(pending), try isCurrentSettingsSnapshotOriginal(pending) else { return nil }
        guard envelope.header.sender == pending.scope.localID,
              envelope.header.recipient == pending.input.hostID,
              envelope.header.expires == pending.input.time.expiresAt,
              envelope.header.created >= pending.wallFloor,
              envelope.header.expires > envelope.header.created,
              envelope.header.expires - envelope.header.created <= 30_000 else { throw WireError.identity }
        return envelope
    }

    private func preparePendingSettingsSnapshot(socket: RelayConnection) async throws {
        guard let pending = pendingSettingsSnapshot, pending.scope.socket === socket else { return }
        guard try isCurrentSettingsSnapshotOriginal(pending) else {
            if settingsSnapshotOutcome == nil { settingsSnapshotOutcome = .expired }
            retirePendingSettingsSnapshot(pending)
            return
        }
        guard case .waiting = pending.phase else { return }
        pending.phase = .preparing
        do {
            do {
                let envelope = try await prepareSettingsSnapshotEnvelope(pending)
                if let envelope, try isCurrentSettingsSnapshotOriginal(pending) {
                    guard case .snapshot(let sent) = outbound[.snapshot(pending.input.queryID)],
                          sent.original === pending, sent.envelope == nil, !sent.sendStarted,
                          envelope.header.messageID != pending.input.queryID,
                          !hasOutboundMessage(envelope.header.messageID),
                          !acknowledgements.contains(envelope.header.messageID) else { throw WireError.conflict }
                    sent.envelope = envelope
                    pending.phase = .prepared(envelope)
                }
            }
            if case .preparing = pending.phase {
                pending.phase = .waiting
                retirePendingSettingsSnapshot(pending)
            }
        } catch {
            pending.phase = .waiting
            settingsSnapshotOutcome = .interrupted
            retirePendingSettingsSnapshot(pending)
            retireSettingsScope(socket: pending.scope.socket)
            throw error
        }
    }

    private func retirePendingSettingsSnapshot(_ pending: PendingSettingsSnapshot) {
        pending.retired = true
        switch pending.phase {
        case .preparing, .receiving: return
        case .waiting, .prepared: break
        }
        if case .snapshot(let sent) = outbound[.snapshot(pending.input.queryID)], sent.original === pending,
           !sent.sendStarted {
            outbound.removeValue(forKey: .snapshot(pending.input.queryID))
        }
        if pendingSettingsSnapshot === pending { pendingSettingsRequest = nil }
    }

    private func completeSettingsSnapshotSend(_ sent: SettingsSnapshotSend) throws {
        let key = OutboundID.snapshot(sent.original.input.queryID)
        guard case .snapshot(let registered) = outbound[key], registered === sent,
              outboundSocket === sent.original.scope.socket else { throw WireError.conflict }
        guard !sent.failed, isCurrentSettingsScope(sent.original.scope),
              sent.sendReturned, sent.accepted, sent.replyReturned else { return }
        outbound.removeValue(forKey: key)
    }

    private func sendPendingSettingsSnapshot(socket: RelayConnection) async throws {
        guard let pending = pendingSettingsSnapshot, pending.scope.socket === socket,
              try isCurrentSettingsSnapshotOriginal(pending), case .prepared(let envelope) = pending.phase,
              case .snapshot(let sent) = outbound[.snapshot(pending.input.queryID)], sent.original === pending,
              !sent.sendStarted else { return }
        guard try await checkSettingsSnapshotStore(pending), try isCurrentSettingsSnapshotOriginal(pending),
              case .prepared(let retained) = pending.phase, sameSettingsEnvelope(retained, envelope) else { return }
        guard outboundSocket === socket, case .snapshot(let registered) = outbound[.snapshot(pending.input.queryID)],
              registered === sent, let bound = sent.envelope, sameSettingsEnvelope(bound, envelope),
              !sent.sendStarted, outbound[.ordinary(envelope.header.messageID)] == nil else { throw WireError.conflict }
        sent.sendStarted = true
        do {
            try await socket.send(.object(["type": .string("send"), "schema_version": .unsigned(1), "envelope": envelope.wire]))
            sent.sendReturned = true
            try completeSettingsSnapshotSend(sent)
        } catch {
            sent.sendReturned = true
            sent.failed = true
            retireSettingsScope(socket: socket)
            throw error
        }
    }

    private func beginSettingsSnapshotReceive(_ received: ReceivedRelayFrame,
                                           ticket: SettingsReceiveTicket) throws -> SettingsSnapshotReceiveAttempt {
        try Task.checkCancellation()
        guard let owner = settingsReceive, owner.ticket === ticket,
              isCurrentSettingsScope(ticket.scope) else { throw WireError.unavailable }
        guard owner.attempt == nil else { throw WireError.conflict }
        guard case .delivery = received.frame else { throw WireError.invalid }
        let original: SettingsSnapshotOriginal
        if let pending = pendingSettingsSnapshot {
            guard pending.scope === ticket.scope else { throw WireError.conflict }
            let current = try isCurrentSettingsSnapshotOriginal(pending)
            if case .prepared(let prepared) = pending.phase, current,
               case .snapshot(let sent) = outbound[.snapshot(pending.input.queryID)], sent.original === pending,
               sent.sendStarted, let bound = sent.envelope, sameSettingsEnvelope(bound, prepared) {
                pending.phase = .receiving(prepared)
                original = .prepared(pending, prepared)
            } else {
                if !current { retirePendingSettingsSnapshot(pending) }
                original = .absent
            }
        } else {
            original = .absent
        }
        let attempt = SettingsSnapshotReceiveAttempt(issuer: ObjectIdentifier(self), ticket: ticket,
                                                   received: received, original: original)
        owner.attempt = .snapshot(attempt)
        return attempt
    }

    private func checkSettingsSnapshotAttempt(_ attempt: SettingsSnapshotReceiveAttempt) throws {
        try Task.checkCancellation()
        guard attempt.issuer == ObjectIdentifier(self),
              let owner = settingsReceive, owner.ticket === attempt.ticket,
              case .snapshot(let registered) = owner.attempt, registered === attempt,
              isCurrentSettingsScope(attempt.ticket.scope) else {
            throw WireError.unavailable
        }
    }

    private func currentSettingsSnapshotOriginal(_ attempt: SettingsSnapshotReceiveAttempt) throws -> PendingSettingsSnapshot? {
        guard case .prepared(let pending, let prepared) = attempt.original else { return nil }
        guard pendingSettingsSnapshot === pending, pending.scope === attempt.ticket.scope,
              case .receiving(let retained) = pending.phase,
              sameSettingsEnvelope(retained, prepared) else {
            retireSettingsScope(socket: attempt.ticket.scope.socket)
            throw WireError.conflict
        }
        guard try isCurrentSettingsSnapshotOriginal(pending) else {
            retirePendingSettingsSnapshot(pending)
            return nil
        }
        return pending
    }

    private func checkSettingsSnapshotReceiveStore(_ attempt: SettingsSnapshotReceiveAttempt) async throws {
        try checkSettingsSnapshotAttempt(attempt)
        _ = try currentSettingsSnapshotOriginal(attempt)
        let scope = attempt.ticket.scope
        try await store.checkActive(host: scope.host)
        try checkSettingsSnapshotAttempt(attempt)
        _ = try currentSettingsSnapshotOriginal(attempt)
        let localID = try await store.identityID()
        try checkSettingsSnapshotAttempt(attempt)
        _ = try currentSettingsSnapshotOriginal(attempt)
        guard localID == scope.localID else { throw WireError.identity }
    }

    private func matchesSettingsSnapshotOriginal(_ attempt: SettingsSnapshotReceiveAttempt,
                                              metadata: ReplicaSettingsReplyMetadata) throws -> Bool {
        guard let pending = try currentSettingsSnapshotOriginal(attempt),
              case .prepared(_, let prepared) = attempt.original,
              case .query(let queryID) = metadata.correlation, let request = pending.request else { return false }
        let scope = attempt.ticket.scope
        return metadata.inReplyTo == prepared.header.messageID && queryID == pending.input.queryID
            && metadata.header.messageID != queryID && metadata.header.messageID != prepared.header.messageID
            && (metadata.generation == nil || metadata.generation == request.parent.generation)
            && pending.input.hostID == scope.host.deviceID
            && metadata.header.sender == scope.host.deviceID && metadata.header.recipient == scope.localID
            && prepared.header.sender == scope.localID && prepared.header.recipient == scope.host.deviceID
            && prepared.header.expires == pending.input.time.expiresAt
            && prepared.header.created >= pending.wallFloor
            && prepared.header.expires > prepared.header.created
            && prepared.header.expires - prepared.header.created <= 30_000
    }

    private func classifySettingsSnapshotReceive(_ received: ReceivedRelayFrame,
                                              ticket: SettingsReceiveTicket) async throws -> SettingsSnapshotReceiveResult {
        let attempt = try beginSettingsSnapshotReceive(received, ticket: ticket)
        try await checkSettingsSnapshotReceiveStore(attempt)
        try checkSettingsSnapshotAttempt(attempt)
        let request: RemoteSettingsSnapshotReadRequest?
        if case .prepared(let pending, _) = attempt.original { request = pending.request }
        else { request = nil }
        let classified = try await identity.receiveClassified(received, host: ticket.scope.host, into: store,
                                                              matchingSnapshot: request)
        attempt.classified = classified
        try checkSettingsSnapshotAttempt(attempt)
        _ = try currentSettingsSnapshotOriginal(attempt)
        guard classified.matchesReceivedFrame(received) else { throw WireError.identity }
        try await checkSettingsSnapshotReceiveStore(attempt)
        try checkSettingsSnapshotAttempt(attempt)
        let verified: VerifiedReplicaSettingsReply
        switch classified.result {
        case .ordinary, .refused:
            return .durable(classified)
        case .settingsSnapshot(let proof), .settingsAccess(let proof), .settingsBind(let proof), .settingsInspection(let proof), .settingsUpdate(let proof):
            verified = proof
        }
        guard verified.matchesVerifiedPeer(ticket.scope.host, localID: ticket.scope.localID) else {
            throw WireError.identity
        }
        let committed = try await verified.commitMetadata(into: store)
        attempt.committed = committed
        try checkSettingsSnapshotAttempt(attempt)
        _ = try currentSettingsSnapshotOriginal(attempt)
        try await checkSettingsSnapshotReceiveStore(attempt)
        try checkSettingsSnapshotAttempt(attempt)
        guard committed.matchesVerifiedPeer(ticket.scope.host, localID: ticket.scope.localID) else {
            throw WireError.identity
        }
        guard case .settingsSnapshot = classified.result,
              try matchesSettingsSnapshotOriginal(attempt, metadata: verified.metadata),
              try matchesSettingsSnapshotOriginal(attempt, metadata: committed.metadata) else {
            return .refused(committed)
        }
        guard case .fresh = committed.disposition else { return .duplicate(committed) }
        guard case .unissued = attempt.qualification else { throw WireError.conflict }
        let qualification = ReplicaSettingsSnapshotQualification(attempt: attempt, committed: committed)
        attempt.qualification = .issued(ObjectIdentifier(qualification))
        return .qualified(qualification)
    }

    fileprivate func checkSettingsSnapshotQualification(_ qualification: ReplicaSettingsSnapshotQualification,
                                                     committed: CommittedReplicaSettingsReply) throws {
        let attempt = qualification.attempt
        try checkSettingsSnapshotAttempt(attempt)
        guard qualification.committed === committed, attempt.committed === committed,
              case .issued(let identifier) = attempt.qualification,
              identifier == ObjectIdentifier(qualification),
              case .fresh = committed.disposition,
              let classified = attempt.classified, classified.matchesReceivedFrame(attempt.received),
              case .settingsSnapshot(let verified) = classified.result,
              verified.matchesVerifiedPeer(attempt.ticket.scope.host, localID: attempt.ticket.scope.localID),
              committed.matchesVerifiedPeer(attempt.ticket.scope.host, localID: attempt.ticket.scope.localID),
              try matchesSettingsSnapshotOriginal(attempt, metadata: verified.metadata),
              try matchesSettingsSnapshotOriginal(attempt, metadata: committed.metadata) else {
            throw WireError.unavailable
        }
    }

    fileprivate func consumeSettingsSnapshotQualification(_ qualification: ReplicaSettingsSnapshotQualification,
                                                       committed: CommittedReplicaSettingsReply) throws {
        try checkSettingsSnapshotQualification(qualification, committed: committed)
        guard let pending = try currentSettingsSnapshotOriginal(qualification.attempt) else {
            throw WireError.unavailable
        }
        qualification.attempt.qualification = .consumed
        pending.consumed = true
        retirePendingSettingsSnapshot(pending)
    }

    private func recheckSettingsSnapshotQualification(_ qualification: ReplicaSettingsSnapshotQualification) async throws {
        try checkSettingsSnapshotQualification(qualification, committed: qualification.committed)
        try await checkSettingsSnapshotReceiveStore(qualification.attempt)
        try checkSettingsSnapshotQualification(qualification, committed: qualification.committed)
    }

    func checkSettingsSnapshotDisplayProjection(_ qualification: ReplicaSettingsSnapshotQualification,
                                             committed: CommittedReplicaSettingsReply) throws {
        try checkSettingsSnapshotQualification(qualification, committed: committed)
        guard let pending = try currentSettingsSnapshotOriginal(qualification.attempt) else {
            throw WireError.unavailable
        }
        let header = committed.metadata.header
        guard header.created >= pending.wallFloor, header.expires <= pending.input.time.expiresAt else {
            throw WireError.invalid
        }
    }

    private func adoptSettingsSnapshotDisplay(_ qualification: ReplicaSettingsSnapshotQualification) async throws {
        try await recheckSettingsSnapshotQualification(qualification)
        try checkSettingsSnapshotDisplayProjection(qualification, committed: qualification.committed)
        guard let pending = try currentSettingsSnapshotOriginal(qualification.attempt) else { throw WireError.unavailable }
        let scope = pending.scope
        let queryExpiry = pending.input.time.expiresAt
        let queryDeadline = pending.deadline
        let header = qualification.committed.metadata.header
        let projection = try qualification.committed.projectSnapshotDisplay(qualifiedBy: qualification, owner: self)
        let expiresAt = min(queryExpiry, header.expires)
        guard expiresAt > pending.wallFloor, expiresAt <= queryExpiry else { throw WireError.expired }
        let clip = queryExpiry - expiresAt
        guard clip <= 30_000 else { throw WireError.expired }
        let deadline = queryDeadline.advanced(by: .milliseconds(-Int64(clip)))
        let monotonic = ContinuousClock.now
        let wall = try WireID.now()
        guard isCurrentSettingsScope(scope), wall >= pending.wallFloor, wall < expiresAt,
              monotonic >= pending.monotonicFloor, monotonic < deadline else { throw WireError.expired }
        let observedAt: UInt64
        switch projection.display {
        case .values(let snapshot): observedAt = snapshot.observedAt
        case .denied:
            pending.association.retired = true
            observedAt = header.created
        }
        settingsSnapshotDisplayCache = SettingsSnapshotDisplayCache(
            scope: scope, projection: projection, requestHandle: pending.handle,
            bindingHandle: pending.association.handle, wallFloor: pending.wallFloor,
            monotonicFloor: pending.monotonicFloor, expiresAt: expiresAt, deadline: deadline, observedAt: observedAt
        )
    }

    private func isCurrentSettingsSnapshotDisplay(_ cache: SettingsSnapshotDisplayCache) throws -> Bool {
        guard let scope = cache.scope, isCurrentSettingsScope(scope),
              scope.host == cache.host, scope.localID == cache.localID,
              let association = settingsBindingAssociation, association.handle === cache.bindingHandle,
              try isCurrentSettingsParent(association, scope: scope) else { return false }
        guard case .values(let snapshot) = cache.projection.display, !snapshot.stale else { return false }
        let monotonic = ContinuousClock.now
        let wall = try WireID.now()
        return wall >= cache.wallFloor && wall < cache.expiresAt
            && monotonic >= cache.monotonicFloor && monotonic < cache.deadline
    }

    private func settingsSnapshotDisplayView(_ cache: SettingsSnapshotDisplayCache?, snapshot: ReplicaSnapshot,
                                          initiallyCurrent: Bool) async throws -> ReplicaSettingsSnapshotView? {
        guard settingsSnapshotDisplayCache === cache else { throw WireError.unavailable }
        guard let cache else { return nil }
        guard !snapshot.revoked, snapshot.host == cache.host,
              identity.publicKeys.deviceID == cache.localID else {
            settingsSnapshotDisplayCache = nil
            settingsInspectionDisplayCache = nil
            return nil
        }
        var current = try isCurrentSettingsSnapshotDisplay(cache) && initiallyCurrent
        do {
            try await store.checkActive(host: cache.host)
        } catch let error as WireError where error == .identity || error == .revoked {
            if settingsSnapshotDisplayCache === cache { settingsSnapshotDisplayCache = nil }
            throw error
        }
        guard settingsSnapshotDisplayCache === cache else { throw WireError.unavailable }
        let afterHostCheck = try isCurrentSettingsSnapshotDisplay(cache)
        current = current && afterHostCheck
        let localID = try await store.identityID()
        guard settingsSnapshotDisplayCache === cache else { throw WireError.unavailable }
        let afterIdentityCheck = try isCurrentSettingsSnapshotDisplay(cache)
        current = current && afterIdentityCheck
        guard localID == cache.localID else {
            settingsSnapshotDisplayCache = nil
            settingsInspectionDisplayCache = nil
            throw WireError.identity
        }
        return ReplicaSettingsSnapshotView(display: cache.projection.display, observedAt: cache.observedAt,
                                               expiresAt: cache.expiresAt,
                                               freshness: current ? .currentConnectionObservation : .stale,
                                               requestHandle: cache.requestHandle)
    }

    private func settingsSnapshotRequestStatus() -> ReplicaSettingsAccessRequestStatus? {
        if let sent = outbound.values.compactMap({ entry -> SettingsSnapshotSend? in
            if case .snapshot(let sent) = entry { return sent }
            return nil
        }).first {
            if !sent.sendStarted, !sent.original.retired { return .preparing }
            if !sent.original.retired { return .awaitingReply }
            return .settling
        }
        if let pending = pendingSettingsSnapshot {
            return pending.retired ? settingsSnapshotOutcome ?? .interrupted : .preparing
        }
        return settingsSnapshotOutcome
    }

    private func isCurrentSettingsInspectionOriginal(_ pending: PendingSettingsInspection) throws -> Bool {
        guard pendingSettingsInspection === pending, !pending.retired, !pending.consumed,
              pending.input.requestID == pending.capture.requestID,
              pending.input.operationID == pending.capture.operationID,
              pending.input.queryID == pending.capture.queryID,
              isCurrentSettingsScope(pending.scope),
              try isCurrentSettingsParent(pending.association, scope: pending.scope) else { return false }
        let monotonic = ContinuousClock.now
        let wall = try WireID.now()
        return wall >= pending.wallFloor && wall < pending.input.time.expiresAt
            && monotonic >= pending.monotonicFloor && monotonic < pending.deadline
    }

    private func checkSettingsInspectionStore(_ pending: PendingSettingsInspection) async throws -> Bool {
        guard try isCurrentSettingsInspectionOriginal(pending) else { return false }
        let scope = pending.scope
        try await store.checkActive(host: scope.host)
        guard try isCurrentSettingsInspectionOriginal(pending) else { return false }
        let localID = try await store.identityID()
        guard try isCurrentSettingsInspectionOriginal(pending) else { return false }
        guard localID == scope.localID else { throw WireError.identity }
        return true
    }

    private func prepareSettingsInspectionEnvelope(_ pending: PendingSettingsInspection) async throws -> EncryptedEnvelope? {
        guard try isCurrentSettingsInspectionOriginal(pending) else { return nil }
        guard try await checkSettingsInspectionStore(pending), try isCurrentSettingsInspectionOriginal(pending) else { return nil }
        guard let request = pending.request else { throw WireError.unavailable }
        let envelope = try await identity.sealSettings(request, recipient: pending.scope.host)
        guard try isCurrentSettingsInspectionOriginal(pending) else { return nil }
        guard try await checkSettingsInspectionStore(pending), try isCurrentSettingsInspectionOriginal(pending) else { return nil }
        guard envelope.header.sender == pending.scope.localID,
              envelope.header.recipient == pending.input.hostID,
              envelope.header.expires == pending.input.time.expiresAt,
              envelope.header.created >= pending.wallFloor,
              envelope.header.expires > envelope.header.created,
              envelope.header.expires - envelope.header.created <= 30_000 else { throw WireError.identity }
        return envelope
    }

    private func preparePendingSettingsInspection(socket: RelayConnection) async throws {
        guard let pending = pendingSettingsInspection, pending.scope.socket === socket else { return }
        guard try isCurrentSettingsInspectionOriginal(pending) else {
            if settingsInspectionOutcome == nil { settingsInspectionOutcome = .expired }
            retirePendingSettingsInspection(pending)
            return
        }
        guard case .waiting = pending.phase else { return }
        pending.phase = .preparing
        do {
            do {
                let envelope = try await prepareSettingsInspectionEnvelope(pending)
                if let envelope, try isCurrentSettingsInspectionOriginal(pending) {
                    guard case .inspection(let sent) = outbound[.inspection(pending.input.queryID)],
                          sent.original === pending, sent.envelope == nil, !sent.sendStarted,
                          envelope.header.messageID != pending.input.queryID,
                          !hasOutboundMessage(envelope.header.messageID),
                          !acknowledgements.contains(envelope.header.messageID) else { throw WireError.conflict }
                    sent.envelope = envelope
                    pending.phase = .prepared(envelope)
                }
            }
            if case .preparing = pending.phase {
                pending.phase = .waiting
                retirePendingSettingsInspection(pending)
            }
        } catch {
            pending.phase = .waiting
            settingsInspectionOutcome = .interrupted
            retirePendingSettingsInspection(pending)
            retireSettingsScope(socket: pending.scope.socket)
            throw error
        }
    }

    private func retirePendingSettingsInspection(_ pending: PendingSettingsInspection) {
        pending.retired = true
        switch pending.phase {
        case .preparing, .receiving: return
        case .waiting, .prepared: break
        }
        if case .inspection(let sent) = outbound[.inspection(pending.input.queryID)], sent.original === pending,
           !sent.sendStarted {
            outbound.removeValue(forKey: .inspection(pending.input.queryID))
        }
        if pendingSettingsInspection === pending { pendingSettingsRequest = nil }
    }

    private func completeSettingsInspectionSend(_ sent: SettingsInspectionSend) throws {
        let key = OutboundID.inspection(sent.original.input.queryID)
        guard case .inspection(let registered) = outbound[key], registered === sent,
              outboundSocket === sent.original.scope.socket else { throw WireError.conflict }
        guard !sent.failed, isCurrentSettingsScope(sent.original.scope),
              sent.sendReturned, sent.accepted, sent.replyReturned else { return }
        outbound.removeValue(forKey: key)
    }

    private func sendPendingSettingsInspection(socket: RelayConnection) async throws {
        guard let pending = pendingSettingsInspection, pending.scope.socket === socket,
              try isCurrentSettingsInspectionOriginal(pending), case .prepared(let envelope) = pending.phase,
              case .inspection(let sent) = outbound[.inspection(pending.input.queryID)], sent.original === pending,
              !sent.sendStarted else { return }
        guard try await checkSettingsInspectionStore(pending), try isCurrentSettingsInspectionOriginal(pending),
              case .prepared(let retained) = pending.phase, sameSettingsEnvelope(retained, envelope) else { return }
        guard outboundSocket === socket, case .inspection(let registered) = outbound[.inspection(pending.input.queryID)],
              registered === sent, let bound = sent.envelope, sameSettingsEnvelope(bound, envelope),
              !sent.sendStarted, outbound[.ordinary(envelope.header.messageID)] == nil else { throw WireError.conflict }
        sent.sendStarted = true
        do {
            try await socket.send(.object(["type": .string("send"), "schema_version": .unsigned(1), "envelope": envelope.wire]))
            sent.sendReturned = true
            try completeSettingsInspectionSend(sent)
        } catch {
            sent.sendReturned = true
            sent.failed = true
            retireSettingsScope(socket: socket)
            throw error
        }
    }

    private func beginSettingsInspectionReceive(_ received: ReceivedRelayFrame,
                                           ticket: SettingsReceiveTicket) throws -> SettingsInspectionReceiveAttempt {
        try Task.checkCancellation()
        guard let owner = settingsReceive, owner.ticket === ticket,
              isCurrentSettingsScope(ticket.scope) else { throw WireError.unavailable }
        guard owner.attempt == nil else { throw WireError.conflict }
        guard case .delivery = received.frame else { throw WireError.invalid }
        let original: SettingsInspectionOriginal
        if let pending = pendingSettingsInspection {
            guard pending.scope === ticket.scope else { throw WireError.conflict }
            let current = try isCurrentSettingsInspectionOriginal(pending)
            if case .prepared(let prepared) = pending.phase, current,
               case .inspection(let sent) = outbound[.inspection(pending.input.queryID)], sent.original === pending,
               sent.sendStarted, let bound = sent.envelope, sameSettingsEnvelope(bound, prepared) {
                pending.phase = .receiving(prepared)
                original = .prepared(pending, prepared)
            } else {
                if !current { retirePendingSettingsInspection(pending) }
                original = .absent
            }
        } else {
            original = .absent
        }
        let attempt = SettingsInspectionReceiveAttempt(issuer: ObjectIdentifier(self), ticket: ticket,
                                                   received: received, original: original)
        owner.attempt = .inspection(attempt)
        return attempt
    }

    private func checkSettingsInspectionAttempt(_ attempt: SettingsInspectionReceiveAttempt) throws {
        try Task.checkCancellation()
        guard attempt.issuer == ObjectIdentifier(self),
              let owner = settingsReceive, owner.ticket === attempt.ticket,
              case .inspection(let registered) = owner.attempt, registered === attempt,
              isCurrentSettingsScope(attempt.ticket.scope) else {
            throw WireError.unavailable
        }
    }

    private func currentSettingsInspectionOriginal(_ attempt: SettingsInspectionReceiveAttempt) throws -> PendingSettingsInspection? {
        guard case .prepared(let pending, let prepared) = attempt.original else { return nil }
        guard pendingSettingsInspection === pending, pending.scope === attempt.ticket.scope,
              case .receiving(let retained) = pending.phase,
              sameSettingsEnvelope(retained, prepared) else {
            retireSettingsScope(socket: attempt.ticket.scope.socket)
            throw WireError.conflict
        }
        guard try isCurrentSettingsInspectionOriginal(pending) else {
            retirePendingSettingsInspection(pending)
            return nil
        }
        return pending
    }

    private func checkSettingsInspectionReceiveStore(_ attempt: SettingsInspectionReceiveAttempt) async throws {
        try checkSettingsInspectionAttempt(attempt)
        _ = try currentSettingsInspectionOriginal(attempt)
        let scope = attempt.ticket.scope
        try await store.checkActive(host: scope.host)
        try checkSettingsInspectionAttempt(attempt)
        _ = try currentSettingsInspectionOriginal(attempt)
        let localID = try await store.identityID()
        try checkSettingsInspectionAttempt(attempt)
        _ = try currentSettingsInspectionOriginal(attempt)
        guard localID == scope.localID else { throw WireError.identity }
    }

    private func matchesSettingsInspectionOriginal(_ attempt: SettingsInspectionReceiveAttempt,
                                              metadata: ReplicaSettingsReplyMetadata) throws -> Bool {
        guard let pending = try currentSettingsInspectionOriginal(attempt),
              case .prepared(_, let prepared) = attempt.original,
              case .query(let queryID) = metadata.correlation, let request = pending.request,
              request.requestID == pending.input.requestID, request.operationID == pending.input.operationID,
              request.queryID == pending.input.queryID, request.time == pending.input.time else { return false }
        let scope = attempt.ticket.scope
        return metadata.inReplyTo == prepared.header.messageID && queryID == pending.input.queryID
            && metadata.header.messageID != queryID && metadata.header.messageID != prepared.header.messageID
            && (metadata.generation == nil || metadata.generation == request.parent.generation)
            && pending.input.hostID == scope.host.deviceID
            && metadata.header.sender == scope.host.deviceID && metadata.header.recipient == scope.localID
            && prepared.header.sender == scope.localID && prepared.header.recipient == scope.host.deviceID
            && prepared.header.expires == pending.input.time.expiresAt
            && prepared.header.created >= pending.wallFloor
            && prepared.header.expires > prepared.header.created
            && prepared.header.expires - prepared.header.created <= 30_000
    }

    private func classifySettingsInspectionReceive(_ received: ReceivedRelayFrame,
                                              ticket: SettingsReceiveTicket) async throws -> SettingsInspectionReceiveResult {
        let attempt = try beginSettingsInspectionReceive(received, ticket: ticket)
        try await checkSettingsInspectionReceiveStore(attempt)
        try checkSettingsInspectionAttempt(attempt)
        let request: RemoteSettingsOriginalInspectionRequest?
        if case .prepared(let pending, _) = attempt.original { request = pending.request }
        else { request = nil }
        let classified = try await identity.receiveClassified(received, host: ticket.scope.host, into: store,
                                                              matchingInspection: request)
        attempt.classified = classified
        try checkSettingsInspectionAttempt(attempt)
        _ = try currentSettingsInspectionOriginal(attempt)
        guard classified.matchesReceivedFrame(received) else { throw WireError.identity }
        try await checkSettingsInspectionReceiveStore(attempt)
        try checkSettingsInspectionAttempt(attempt)
        let verified: VerifiedReplicaSettingsReply
        switch classified.result {
        case .ordinary, .refused:
            return .durable(classified)
        case .settingsInspection(let proof), .settingsAccess(let proof), .settingsBind(let proof), .settingsSnapshot(let proof), .settingsUpdate(let proof):
            verified = proof
        }
        guard verified.matchesVerifiedPeer(ticket.scope.host, localID: ticket.scope.localID) else {
            throw WireError.identity
        }
        let committed = try await verified.commitMetadata(into: store)
        attempt.committed = committed
        try checkSettingsInspectionAttempt(attempt)
        _ = try currentSettingsInspectionOriginal(attempt)
        try await checkSettingsInspectionReceiveStore(attempt)
        try checkSettingsInspectionAttempt(attempt)
        guard committed.matchesVerifiedPeer(ticket.scope.host, localID: ticket.scope.localID) else {
            throw WireError.identity
        }
        guard case .settingsInspection = classified.result,
              try matchesSettingsInspectionOriginal(attempt, metadata: verified.metadata),
              try matchesSettingsInspectionOriginal(attempt, metadata: committed.metadata) else {
            return .refused(committed)
        }
        guard case .fresh = committed.disposition else { return .duplicate(committed) }
        guard case .unissued = attempt.qualification else { throw WireError.conflict }
        let qualification = ReplicaSettingsInspectionQualification(attempt: attempt, committed: committed)
        attempt.qualification = .issued(ObjectIdentifier(qualification))
        return .qualified(qualification)
    }

    fileprivate func checkSettingsInspectionQualification(_ qualification: ReplicaSettingsInspectionQualification,
                                                     committed: CommittedReplicaSettingsReply) throws {
        let attempt = qualification.attempt
        try checkSettingsInspectionAttempt(attempt)
        guard qualification.committed === committed, attempt.committed === committed,
              case .issued(let identifier) = attempt.qualification,
              identifier == ObjectIdentifier(qualification),
              case .fresh = committed.disposition,
              let classified = attempt.classified, classified.matchesReceivedFrame(attempt.received),
              case .settingsInspection(let verified) = classified.result,
              verified.matchesVerifiedPeer(attempt.ticket.scope.host, localID: attempt.ticket.scope.localID),
              committed.matchesVerifiedPeer(attempt.ticket.scope.host, localID: attempt.ticket.scope.localID),
              try matchesSettingsInspectionOriginal(attempt, metadata: verified.metadata),
              try matchesSettingsInspectionOriginal(attempt, metadata: committed.metadata) else {
            throw WireError.unavailable
        }
    }

    fileprivate func consumeSettingsInspectionQualification(_ qualification: ReplicaSettingsInspectionQualification,
                                                       committed: CommittedReplicaSettingsReply) throws {
        try checkSettingsInspectionQualification(qualification, committed: committed)
        guard let pending = try currentSettingsInspectionOriginal(qualification.attempt) else {
            throw WireError.unavailable
        }
        qualification.attempt.qualification = .consumed
        pending.consumed = true
        retirePendingSettingsInspection(pending)
    }

    private func recheckSettingsInspectionQualification(_ qualification: ReplicaSettingsInspectionQualification) async throws {
        try checkSettingsInspectionQualification(qualification, committed: qualification.committed)
        try await checkSettingsInspectionReceiveStore(qualification.attempt)
        try checkSettingsInspectionQualification(qualification, committed: qualification.committed)
    }

    func checkSettingsInspectionDisplayProjection(_ qualification: ReplicaSettingsInspectionQualification,
                                             committed: CommittedReplicaSettingsReply) throws {
        try checkSettingsInspectionQualification(qualification, committed: committed)
        guard let pending = try currentSettingsInspectionOriginal(qualification.attempt) else {
            throw WireError.unavailable
        }
        let header = committed.metadata.header
        guard header.created >= pending.wallFloor, header.expires <= pending.input.time.expiresAt else {
            throw WireError.invalid
        }
    }

    private func adoptSettingsInspectionDisplay(_ qualification: ReplicaSettingsInspectionQualification) async throws {
        try await recheckSettingsInspectionQualification(qualification)
        try checkSettingsInspectionDisplayProjection(qualification, committed: qualification.committed)
        guard let pending = try currentSettingsInspectionOriginal(qualification.attempt) else { throw WireError.unavailable }
        let scope = pending.scope
        let queryExpiry = pending.input.time.expiresAt
        let queryDeadline = pending.deadline
        let header = qualification.committed.metadata.header
        let projection = try qualification.committed.projectInspectionDisplay(qualifiedBy: qualification, owner: self)
        let expiresAt = min(queryExpiry, header.expires)
        guard expiresAt > pending.wallFloor, expiresAt <= queryExpiry else { throw WireError.expired }
        let clip = queryExpiry - expiresAt
        guard clip <= 30_000 else { throw WireError.expired }
        let deadline = queryDeadline.advanced(by: .milliseconds(-Int64(clip)))
        let monotonic = ContinuousClock.now
        let wall = try WireID.now()
        guard isCurrentSettingsScope(scope), wall >= pending.wallFloor, wall < expiresAt,
              monotonic >= pending.monotonicFloor, monotonic < deadline else { throw WireError.expired }
        let observedAt: UInt64
        switch projection.display {
        case .observation(let requestID, let operationID, let observed, _):
            guard requestID == pending.input.requestID, operationID == pending.input.operationID else { throw WireError.invalid }
            observedAt = observed
        case .denied:
            pending.association.retired = true
            observedAt = header.created
        }
        settingsInspectionDisplayCache = SettingsInspectionDisplayCache(
            scope: scope, projection: projection, requestHandle: pending.handle,
            bindingHandle: pending.association.handle, wallFloor: pending.wallFloor,
            monotonicFloor: pending.monotonicFloor, expiresAt: expiresAt, deadline: deadline, observedAt: observedAt
        )
    }

    private func isCurrentSettingsInspectionDisplay(_ cache: SettingsInspectionDisplayCache) throws -> Bool {
        guard let scope = cache.scope, isCurrentSettingsScope(scope),
              scope.host == cache.host, scope.localID == cache.localID,
              let association = settingsBindingAssociation, association.handle === cache.bindingHandle,
              try isCurrentSettingsParent(association, scope: scope) else { return false }
        guard case .observation = cache.projection.display else { return false }
        let monotonic = ContinuousClock.now
        let wall = try WireID.now()
        return wall >= cache.wallFloor && wall < cache.expiresAt
            && monotonic >= cache.monotonicFloor && monotonic < cache.deadline
    }

    private func settingsInspectionDisplayView(_ cache: SettingsInspectionDisplayCache?, inspection: ReplicaSnapshot,
                                          initiallyCurrent: Bool) async throws -> ReplicaSettingsInspectionView? {
        guard settingsInspectionDisplayCache === cache else { throw WireError.unavailable }
        guard let cache else { return nil }
        guard !inspection.revoked, inspection.host == cache.host,
              identity.publicKeys.deviceID == cache.localID else {
            settingsInspectionDisplayCache = nil
            return nil
        }
        var current = try isCurrentSettingsInspectionDisplay(cache) && initiallyCurrent
        do {
            try await store.checkActive(host: cache.host)
        } catch let error as WireError where error == .identity || error == .revoked {
            if settingsInspectionDisplayCache === cache { settingsInspectionDisplayCache = nil }
            throw error
        }
        guard settingsInspectionDisplayCache === cache else { throw WireError.unavailable }
        let afterHostCheck = try isCurrentSettingsInspectionDisplay(cache)
        current = current && afterHostCheck
        let localID = try await store.identityID()
        guard settingsInspectionDisplayCache === cache else { throw WireError.unavailable }
        let afterIdentityCheck = try isCurrentSettingsInspectionDisplay(cache)
        current = current && afterIdentityCheck
        guard localID == cache.localID else {
            settingsInspectionDisplayCache = nil
            throw WireError.identity
        }
        return ReplicaSettingsInspectionView(display: cache.projection.display, observedAt: cache.observedAt,
                                               expiresAt: cache.expiresAt,
                                               freshness: current ? .currentConnectionObservation : .stale,
                                               requestHandle: cache.requestHandle)
    }

    private func settingsInspectionRequestStatus() -> ReplicaSettingsAccessRequestStatus? {
        if let sent = outbound.values.compactMap({ entry -> SettingsInspectionSend? in
            if case .inspection(let sent) = entry { return sent }
            return nil
        }).first {
            if !sent.sendStarted, !sent.original.retired { return .preparing }
            if !sent.original.retired { return .awaitingReply }
            return .settling
        }
        if let pending = pendingSettingsInspection {
            return pending.retired ? settingsInspectionOutcome ?? .interrupted : .preparing
        }
        return settingsInspectionOutcome
    }

    private func beginSettingsBindReceive(_ received: ReceivedRelayFrame,
                                         ticket: SettingsReceiveTicket) throws -> SettingsBindReceiveAttempt {
        try Task.checkCancellation()
        guard let owner = settingsReceive, owner.ticket === ticket,
              isCurrentSettingsScope(ticket.scope) else { throw WireError.unavailable }
        guard owner.attempt == nil else { throw WireError.conflict }
        guard case .delivery = received.frame else { throw WireError.invalid }
        let original: SettingsBindOriginal
        if let pending = pendingSettingsBind {
            guard pending.scope === ticket.scope else { throw WireError.conflict }
            let current = try isCurrentSettingsBindOriginal(pending)
            if case .prepared(let prepared) = pending.phase, current,
               case .bind(let sent) = outbound[.bind(pending.association.input.bindingRequestID)],
               sent.original === pending, sent.sendStarted, let bound = sent.envelope,
               sameSettingsEnvelope(bound, prepared) {
                pending.phase = .receiving(prepared)
                original = .prepared(pending, prepared)
            } else {
                if !current { retirePendingSettingsBind(pending) }
                original = .absent
            }
        } else {
            original = .absent
        }
        let attempt = SettingsBindReceiveAttempt(issuer: ObjectIdentifier(self), ticket: ticket,
                                                 received: received, original: original)
        owner.attempt = .bind(attempt)
        return attempt
    }

    private func checkSettingsBindAttempt(_ attempt: SettingsBindReceiveAttempt) throws {
        try Task.checkCancellation()
        guard attempt.issuer == ObjectIdentifier(self),
              let owner = settingsReceive, owner.ticket === attempt.ticket,
              case .bind(let registered) = owner.attempt, registered === attempt,
              isCurrentSettingsScope(attempt.ticket.scope) else { throw WireError.unavailable }
    }

    private func currentSettingsBindOriginal(_ attempt: SettingsBindReceiveAttempt) throws -> PendingSettingsBind? {
        guard case .prepared(let pending, let prepared) = attempt.original else { return nil }
        guard pendingSettingsBind === pending, pending.scope === attempt.ticket.scope,
              case .receiving(let retained) = pending.phase,
              sameSettingsEnvelope(retained, prepared) else {
            retireSettingsScope(socket: attempt.ticket.scope.socket)
            throw WireError.conflict
        }
        guard try isCurrentSettingsBindOriginal(pending) else {
            retirePendingSettingsBind(pending)
            return nil
        }
        return pending
    }

    private func checkSettingsBindReceiveStore(_ attempt: SettingsBindReceiveAttempt) async throws {
        try checkSettingsBindAttempt(attempt)
        _ = try currentSettingsBindOriginal(attempt)
        let scope = attempt.ticket.scope
        try await store.checkActive(host: scope.host)
        try checkSettingsBindAttempt(attempt)
        _ = try currentSettingsBindOriginal(attempt)
        let localID = try await store.identityID()
        try checkSettingsBindAttempt(attempt)
        _ = try currentSettingsBindOriginal(attempt)
        guard localID == scope.localID else { throw WireError.identity }
    }

    private func matchesSettingsBindOriginal(_ attempt: SettingsBindReceiveAttempt,
                                            metadata: ReplicaSettingsReplyMetadata) throws -> Bool {
        guard let pending = try currentSettingsBindOriginal(attempt),
              case .prepared(_, let prepared) = attempt.original,
              let request = pending.association.request,
              case .bind(let bindingRequestID) = metadata.correlation else { return false }
        let scope = attempt.ticket.scope
        return metadata.inReplyTo == prepared.header.messageID && bindingRequestID == request.bindingRequestID
            && metadata.generation == request.generation && request.hostID == scope.host.deviceID
            && metadata.header.sender == scope.host.deviceID && metadata.header.recipient == scope.localID
            && prepared.header.sender == scope.localID && prepared.header.recipient == scope.host.deviceID
            && prepared.header.expires == request.time.expiresAt
            && prepared.header.created >= pending.association.wallFloor
            && prepared.header.expires > prepared.header.created
            && prepared.header.expires - prepared.header.created <= 30_000
    }

    private func classifySettingsBindReceive(_ received: ReceivedRelayFrame,
                                            ticket: SettingsReceiveTicket) async throws -> SettingsBindReceiveResult {
        let attempt = try beginSettingsBindReceive(received, ticket: ticket)
        try await checkSettingsBindReceiveStore(attempt)
        try checkSettingsBindAttempt(attempt)
        let classified = try await identity.receiveClassified(received, host: ticket.scope.host, into: store)
        attempt.classified = classified
        try checkSettingsBindAttempt(attempt)
        _ = try currentSettingsBindOriginal(attempt)
        guard classified.matchesReceivedFrame(received) else { throw WireError.identity }
        try await checkSettingsBindReceiveStore(attempt)
        try checkSettingsBindAttempt(attempt)
        let verified: VerifiedReplicaSettingsReply
        switch classified.result {
        case .ordinary, .refused:
            return .durable(classified)
        case .settingsAccess(let proof), .settingsBind(let proof), .settingsSnapshot(let proof), .settingsInspection(let proof), .settingsUpdate(let proof):
            verified = proof
        }
        guard verified.matchesVerifiedPeer(ticket.scope.host, localID: ticket.scope.localID) else {
            throw WireError.identity
        }
        let committed = try await verified.commitMetadata(into: store)
        attempt.committed = committed
        try checkSettingsBindAttempt(attempt)
        _ = try currentSettingsBindOriginal(attempt)
        try await checkSettingsBindReceiveStore(attempt)
        try checkSettingsBindAttempt(attempt)
        guard committed.matchesVerifiedPeer(ticket.scope.host, localID: ticket.scope.localID) else {
            throw WireError.identity
        }
        guard case .settingsBind = classified.result,
              try matchesSettingsBindOriginal(attempt, metadata: verified.metadata),
              try matchesSettingsBindOriginal(attempt, metadata: committed.metadata) else {
            return .refused(committed)
        }
        guard case .fresh = committed.disposition else { return .duplicate(committed) }
        guard case .unissued = attempt.qualification else { throw WireError.conflict }
        let qualification = ReplicaSettingsBindQualification(attempt: attempt, committed: committed)
        attempt.qualification = .issued(ObjectIdentifier(qualification))
        return .qualified(qualification)
    }

    fileprivate func checkSettingsBindQualification(_ qualification: ReplicaSettingsBindQualification,
                                                   committed: CommittedReplicaSettingsReply) throws {
        let attempt = qualification.attempt
        try checkSettingsBindAttempt(attempt)
        guard qualification.committed === committed, attempt.committed === committed,
              case .issued(let identifier) = attempt.qualification,
              identifier == ObjectIdentifier(qualification),
              case .fresh = committed.disposition,
              let classified = attempt.classified, classified.matchesReceivedFrame(attempt.received),
              case .settingsBind(let verified) = classified.result,
              verified.matchesVerifiedPeer(attempt.ticket.scope.host, localID: attempt.ticket.scope.localID),
              committed.matchesVerifiedPeer(attempt.ticket.scope.host, localID: attempt.ticket.scope.localID),
              try matchesSettingsBindOriginal(attempt, metadata: verified.metadata),
              try matchesSettingsBindOriginal(attempt, metadata: committed.metadata) else { throw WireError.unavailable }
    }

    fileprivate func consumeSettingsBindQualification(_ qualification: ReplicaSettingsBindQualification,
                                                     committed: CommittedReplicaSettingsReply) throws {
        try checkSettingsBindQualification(qualification, committed: committed)
        guard let pending = try currentSettingsBindOriginal(qualification.attempt) else { throw WireError.unavailable }
        qualification.attempt.qualification = .consumed
        pending.consumed = true
        retirePendingSettingsBind(pending)
    }

    private func recheckSettingsBindQualification(_ qualification: ReplicaSettingsBindQualification) async throws {
        try checkSettingsBindQualification(qualification, committed: qualification.committed)
        try await checkSettingsBindReceiveStore(qualification.attempt)
        try checkSettingsBindQualification(qualification, committed: qualification.committed)
    }

    func checkSettingsBindProjection(_ qualification: ReplicaSettingsBindQualification,
                                     committed: CommittedReplicaSettingsReply) throws -> ReplicaSettingsBindProjectionInput {
        try checkSettingsBindQualification(qualification, committed: committed)
        guard let pending = try currentSettingsBindOriginal(qualification.attempt),
              case .prepared(_, let prepared) = qualification.attempt.original,
              let request = pending.association.request else { throw WireError.unavailable }
        let header = committed.metadata.header
        guard header.created >= pending.association.wallFloor else { throw WireError.invalid }
        return ReplicaSettingsBindProjectionInput(request: request, requestMessageID: prepared.header.messageID,
                                                 wallFloor: pending.association.wallFloor,
                                                 grantExpiresAt: pending.association.input.grantExpiresAt)
    }

    private func adoptSettingsBoundParent(_ qualification: ReplicaSettingsBindQualification) async throws {
        try await recheckSettingsBindQualification(qualification)
        _ = try checkSettingsBindProjection(qualification, committed: qualification.committed)
        guard let pending = try currentSettingsBindOriginal(qualification.attempt) else { throw WireError.unavailable }
        let association = pending.association
        let scope = pending.scope
        guard settingsBindingAssociation === association, association.parent == nil,
              association.parentDeadline == nil else { throw WireError.conflict }
        let projection = try qualification.committed.projectBoundParent(qualifiedBy: qualification, owner: self)
        let requestedExpiry = association.input.requestedParentExpiresAt
        guard projection.expiresAt > association.wallFloor, projection.expiresAt <= requestedExpiry else { throw WireError.expired }
        let clip = requestedExpiry - projection.expiresAt
        guard clip <= 300_000 else { throw WireError.expired }
        let deadline = association.requestedParentDeadline.advanced(by: .milliseconds(-Int64(clip)))
        let monotonic = ContinuousClock.now
        let wall = try WireID.now()
        guard settingsBindingAssociation === association, !association.retired,
              association.scope === scope, isCurrentSettingsScope(scope),
              wall >= association.wallFloor, wall < projection.expiresAt,
              monotonic >= association.monotonicFloor, monotonic < deadline else { throw WireError.expired }
        association.parent = projection
        association.parentDeadline = deadline
        association.observedAt = qualification.committed.metadata.header.created
    }

    private func finishSettingsReceive(_ ticket: SettingsReceiveTicket) throws {
        guard let owner = settingsReceive, owner.ticket === ticket else {
            retireSettingsScope(socket: ticket.scope.socket)
            throw WireError.conflict
        }
        var accessCompletion: (PendingSettingsAccess, EncryptedEnvelope, Bool)?
        var bindCompletion: (PendingSettingsBind, EncryptedEnvelope, Bool)?
        var snapshotCompletion: (PendingSettingsSnapshot, EncryptedEnvelope, Bool)?
        var inspectionCompletion: (PendingSettingsInspection, EncryptedEnvelope, Bool)?
        var updateCompletion: (PendingSettingsUpdate, EncryptedEnvelope, Bool)?
        do {
            switch owner.attempt {
            case .access(let attempt):
                guard attempt.issuer == ObjectIdentifier(self), attempt.ticket === ticket else {
                    retireSettingsScope(socket: ticket.scope.socket)
                    throw WireError.conflict
                }
                if case .prepared(let pending, let prepared) = attempt.original {
                    guard pendingSettingsAccess === pending, pending.scope === ticket.scope,
                          case .receiving(let retained) = pending.phase,
                          sameSettingsEnvelope(retained, prepared) else {
                        retireSettingsScope(socket: ticket.scope.socket)
                        throw WireError.conflict
                    }
                    accessCompletion = (pending, prepared, try isCurrentSettingsOriginal(pending))
                }
                attempt.classified = nil
                attempt.committed = nil
            case .bind(let attempt):
                guard attempt.issuer == ObjectIdentifier(self), attempt.ticket === ticket else {
                    retireSettingsScope(socket: ticket.scope.socket)
                    throw WireError.conflict
                }
                if case .prepared(let pending, let prepared) = attempt.original {
                    guard pendingSettingsBind === pending, pending.scope === ticket.scope,
                          case .receiving(let retained) = pending.phase,
                          sameSettingsEnvelope(retained, prepared) else {
                        retireSettingsScope(socket: ticket.scope.socket)
                        throw WireError.conflict
                    }
                    bindCompletion = (pending, prepared, try isCurrentSettingsBindOriginal(pending))
                }
                attempt.classified = nil
                attempt.committed = nil
            case .snapshot(let attempt):
                guard attempt.issuer == ObjectIdentifier(self), attempt.ticket === ticket else {
                    retireSettingsScope(socket: ticket.scope.socket)
                    throw WireError.conflict
                }
                if case .prepared(let pending, let prepared) = attempt.original {
                    guard pendingSettingsSnapshot === pending, pending.scope === ticket.scope,
                          case .receiving(let retained) = pending.phase,
                          sameSettingsEnvelope(retained, prepared) else {
                        retireSettingsScope(socket: ticket.scope.socket)
                        throw WireError.conflict
                    }
                    snapshotCompletion = (pending, prepared, try isCurrentSettingsSnapshotOriginal(pending))
                }
                attempt.classified = nil
                attempt.committed = nil
            case .inspection(let attempt):
                guard attempt.issuer == ObjectIdentifier(self), attempt.ticket === ticket else {
                    retireSettingsScope(socket: ticket.scope.socket)
                    throw WireError.conflict
                }
                if case .prepared(let pending, let prepared) = attempt.original {
                    guard pendingSettingsInspection === pending, pending.scope === ticket.scope,
                          case .receiving(let retained) = pending.phase,
                          sameSettingsEnvelope(retained, prepared) else {
                        retireSettingsScope(socket: ticket.scope.socket)
                        throw WireError.conflict
                    }
                    inspectionCompletion = (pending, prepared, try isCurrentSettingsInspectionOriginal(pending))
                }
                attempt.classified = nil
                attempt.committed = nil
            case .update(let attempt):
                guard attempt.issuer == ObjectIdentifier(self), attempt.ticket === ticket else {
                    retireSettingsScope(socket: ticket.scope.socket)
                    throw WireError.conflict
                }
                if case .prepared(let pending, let prepared) = attempt.original {
                    guard pendingSettingsUpdate === pending, pending.scope === ticket.scope,
                          case .receiving(let retained) = pending.phase,
                          sameSettingsEnvelope(retained, prepared) else {
                        retireSettingsScope(socket: ticket.scope.socket)
                        throw WireError.conflict
                    }
                    updateCompletion = (pending, prepared, try isCurrentSettingsUpdateOriginal(pending))
                }
                attempt.classified = nil
                attempt.committed = nil
            case nil: break
            }
            owner.attempt = nil
        }
        if let (pending, prepared, current) = accessCompletion {
            if pending.consumed {
                settingsAccessOutcome = .received
                if case .access(let sent) = outbound[.access(pending.request.queryID)], sent.original === pending {
                    sent.replyReturned = true
                    try completeSettingsAccessSend(sent)
                }
            }
            if current {
                pending.phase = .prepared(prepared)
            } else {
                pending.retired = true
                pending.phase = .waiting
                pendingSettingsRequest = nil
            }
        }
        if let (pending, prepared, current) = bindCompletion {
            if pending.consumed,
               case .bind(let sent) = outbound[.bind(pending.association.input.bindingRequestID)],
               sent.original === pending {
                sent.replyReturned = true
                try completeSettingsBindSend(sent)
            }
            if current {
                pending.phase = .prepared(prepared)
            } else {
                pending.retired = true
                if !pending.consumed { pending.association.retired = true }
                pending.phase = .waiting
                pendingSettingsRequest = nil
            }
        }
        if let (pending, prepared, current) = snapshotCompletion {
            if pending.consumed {
                settingsSnapshotOutcome = .received
                if case .snapshot(let sent) = outbound[.snapshot(pending.input.queryID)], sent.original === pending {
                    sent.replyReturned = true
                    try completeSettingsSnapshotSend(sent)
                }
            }
            if current {
                pending.phase = .prepared(prepared)
            } else {
                pending.retired = true
                pending.phase = .waiting
                pendingSettingsRequest = nil
            }
        }
        if let (pending, prepared, current) = inspectionCompletion {
            if pending.consumed {
                settingsInspectionOutcome = .received
                if case .inspection(let sent) = outbound[.inspection(pending.input.queryID)], sent.original === pending {
                    sent.replyReturned = true
                    try completeSettingsInspectionSend(sent)
                }
            }
            if current {
                pending.phase = .prepared(prepared)
            } else {
                pending.retired = true
                pending.phase = .waiting
                pendingSettingsRequest = nil
            }
        }
        if let (pending, prepared, current) = updateCompletion {
            if pending.consumed,
               case .update(let sent) = outbound[.update(pending.input.requestID)], sent.original === pending {
                sent.replyReturned = true
                try completeSettingsUpdateSend(sent)
            }
            if current {
                pending.phase = .prepared(prepared)
            } else {
                pending.retired = true
                pending.phase = .waiting
                pendingSettingsRequest = nil
            }
        }
        settingsReceive = nil
        releaseSettingsAssociations()
    }

    func requestSettingsUpdate(_ capture: ReplicaSettingsUpdateInput) throws -> ReplicaSettingsRequestHandle {
        try Task.checkCancellation()
        guard capture.service === self, let scope = settingsScope, isCurrentSettingsScope(scope),
              scope.token === capture.scope, let association = settingsBindingAssociation,
              association.handle === capture.bindingHandle, association.scope === scope,
              let parent = association.parent, let parentDeadline = association.parentDeadline,
              let bindingRequest = association.request,
              try isCurrentSettingsParent(association, scope: scope) else { throw WireError.unavailable }
        if let pending = pendingSettingsUpdate, pending.capture === capture { return pending.handle }
        releaseSettingsAssociations()
        guard settingsUpdateOriginal == nil, pendingSettingsRequest == nil, !hasPrivateSettingsSend,
              outbound.count < 16, outboundSocket === scope.socket else { throw WireError.queueFull }
        guard capture.expectedSettingsRevision > 0, capture.expectedSettingsRevision <= UInt64(Int64.max),
              capture.enabled != nil || capture.dimensions != nil,
              capture.enabled == nil || association.writableKeys.contains(.embeddingsEnabled),
              capture.dimensions == nil || association.writableKeys.contains(.embeddingsDimensions) else { throw WireError.invalid }
        if case .some(.selected(let value)) = capture.dimensions {
            guard value > 0, value <= 65_536 else { throw WireError.invalid }
        }
        let monotonic = ContinuousClock.now
        let wall = try WireID.now()
        guard capture.wallFloor >= association.wallFloor, capture.monotonicFloor >= association.monotonicFloor,
              wall >= capture.wallFloor, wall < capture.expiresAt,
              monotonic >= capture.monotonicFloor, monotonic < capture.deadline else { throw WireError.expired }
        let expiresAt = min(capture.expiresAt, parent.expiresAt)
        guard expiresAt > capture.wallFloor else { throw WireError.expired }
        let time = try RemoteSettingsRequestTime(issuedAt: capture.wallFloor, expiresAt: expiresAt)
        let deadline = min(capture.monotonicFloor.advanced(by: .milliseconds(Int64(expiresAt - capture.wallFloor))), parentDeadline)
        guard monotonic < deadline else { throw WireError.expired }
        let key = OutboundID.update(capture.requestID)
        guard outbound[key] == nil else { throw WireError.conflict }
        try capture.consumeAdmission()
        let input = ReplicaSettingsUpdateRequestInput(hostID: scope.host.deviceID,
                            requestID: capture.requestID, expectedSettingsRevision: capture.expectedSettingsRevision,
                            enabled: capture.enabled, dimensions: capture.dimensions, time: time)
        let handle = ReplicaSettingsRequestHandle()
        let record = SettingsUpdateOriginalRecord(scope: scope, handle: handle, generation: bindingRequest.generation, input: input,
                            wallFloor: capture.wallFloor, monotonicFloor: capture.monotonicFloor, deadline: deadline)
        let pending = PendingSettingsUpdate(handle: handle, capture: capture, scope: scope,
                            association: association, input: input, record: record, deadline: deadline)
        settingsUpdateOriginal = record
        pendingSettingsRequest = .update(pending)
        outbound[key] = .update(SettingsUpdateSend(original: pending))
        lastSettingsInput = .update(capture)
        do {
            let request = try parent.prepareUpdateRequest(owner: self)
            guard pendingSettingsUpdate === pending, record.request == nil,
                  request.hostID == input.hostID, request.requestID == input.requestID,
                  request.parent.generation == record.generation, request.time == input.time,
                  request.expectedSettingsRevision == input.expectedSettingsRevision,
                  request.enabled == input.enabled, request.dimensions == input.dimensions else { throw WireError.conflict }
            record.request = request
            return handle
        } catch {
            record.status = .interrupted
            retirePendingSettingsUpdate(pending)
            throw error
        }
    }

    func captureSettingsUpdateRequestInput(_ projection: ReplicaSettingsBoundParentProjection) throws -> ReplicaSettingsUpdateRequestInput {
        try Task.checkCancellation()
        guard let pending = pendingSettingsUpdate, try isCurrentSettingsUpdateOriginal(pending),
              pending.association.parent === projection, !pending.requestCaptured,
              pending.request == nil, case .waiting = pending.phase,
              case .update(let sent) = outbound[.update(pending.input.requestID)],
              sent.original === pending, !sent.sendStarted else { throw WireError.unavailable }
        pending.requestCaptured = true
        return pending.input
    }

    private func isCurrentSettingsUpdateOriginal(_ pending: PendingSettingsUpdate) throws -> Bool {
        guard pendingSettingsUpdate === pending, settingsUpdateOriginal === pending.record,
              !pending.retired, !pending.consumed, pending.input.requestID == pending.capture.requestID,
              pending.input.expectedSettingsRevision == pending.capture.expectedSettingsRevision,
              pending.input.enabled == pending.capture.enabled, pending.input.dimensions == pending.capture.dimensions,
              pending.record.host == pending.scope.host, pending.record.localID == pending.scope.localID,
              pending.association.request?.generation == pending.record.generation,
              isCurrentSettingsScope(pending.scope),
              try isCurrentSettingsParent(pending.association, scope: pending.scope) else { return false }
        let monotonic = ContinuousClock.now
        let wall = try WireID.now()
        return wall >= pending.wallFloor && wall < pending.input.time.expiresAt
            && monotonic >= pending.monotonicFloor && monotonic < pending.deadline
    }

    private func checkSettingsUpdateStore(_ pending: PendingSettingsUpdate) async throws -> Bool {
        guard try isCurrentSettingsUpdateOriginal(pending) else { return false }
        let scope = pending.scope
        try await store.checkActive(host: scope.host)
        guard try isCurrentSettingsUpdateOriginal(pending) else { return false }
        let localID = try await store.identityID()
        guard try isCurrentSettingsUpdateOriginal(pending) else { return false }
        guard localID == scope.localID else { throw WireError.identity }
        return true
    }

    private func prepareSettingsUpdateEnvelope(_ pending: PendingSettingsUpdate, sent: SettingsUpdateSend) async throws {
        guard try isCurrentSettingsUpdateOriginal(pending) else { return }
        guard try await checkSettingsUpdateStore(pending), try isCurrentSettingsUpdateOriginal(pending) else { return }
        guard let request = pending.request, sent.original === pending, sent.envelope == nil,
              pending.record.encryptedRequestID == nil, !sent.sendStarted else { throw WireError.unavailable }
        let envelope = try await identity.sealSettings(request, recipient: pending.scope.host)
        sent.envelope = envelope
        pending.record.encryptedRequestID = envelope.header.messageID
        guard case .update(let registered) = outbound[.update(pending.input.requestID)], registered === sent,
              !hasConflictingSettingsUpdateMessage(envelope.header.messageID, excluding: sent),
              !acknowledgements.contains(envelope.header.messageID) else { throw WireError.conflict }
        guard try isCurrentSettingsUpdateOriginal(pending) else { return }
        guard try await checkSettingsUpdateStore(pending), try isCurrentSettingsUpdateOriginal(pending) else { return }
        guard envelope.header.sender == pending.scope.localID, envelope.header.recipient == pending.input.hostID,
              envelope.header.expires == pending.input.time.expiresAt, envelope.header.created >= pending.wallFloor,
              envelope.header.expires > envelope.header.created,
              envelope.header.expires - envelope.header.created <= 30_000 else { throw WireError.identity }
        _ = try envelope.wire.encoded(maximum: 49_152)
        pending.phase = .prepared(envelope)
    }

    private func hasConflictingSettingsUpdateMessage(_ id: UUID, excluding actual: SettingsUpdateSend) -> Bool {
        if outbound[.ordinary(id)] != nil { return true }
        return outbound.values.contains { entry in
            switch entry {
            case .ordinary: false
            case .access(let sent): sent.envelope?.header.messageID == id
            case .bind(let sent): sent.envelope?.header.messageID == id
            case .snapshot(let sent): sent.envelope?.header.messageID == id
            case .inspection(let sent): sent.envelope?.header.messageID == id
            case .update(let sent): sent !== actual && sent.envelope?.header.messageID == id
            }
        }
    }

    private func preparePendingSettingsUpdate(socket: RelayConnection) async throws {
        guard let pending = pendingSettingsUpdate, pending.scope.socket === socket else { return }
        guard try isCurrentSettingsUpdateOriginal(pending) else {
            if pending.record.status == nil { pending.record.status = .expired }
            retirePendingSettingsUpdate(pending)
            return
        }
        guard case .waiting = pending.phase else { return }
        guard case .update(let sent) = outbound[.update(pending.input.requestID)], sent.original === pending else {
            throw WireError.conflict
        }
        pending.phase = .preparing
        do {
            try await prepareSettingsUpdateEnvelope(pending, sent: sent)
            if case .preparing = pending.phase { pending.phase = .waiting }
            if try !isCurrentSettingsUpdateOriginal(pending) || sent.envelope == nil {
                retirePendingSettingsUpdate(pending)
            }
        } catch {
            pending.phase = .waiting
            pending.record.status = .interrupted
            retirePendingSettingsUpdate(pending)
            retireSettingsScope(socket: pending.scope.socket)
            throw error
        }
    }

    private func retirePendingSettingsUpdate(_ pending: PendingSettingsUpdate) {
        pending.retired = true
        switch pending.phase {
        case .preparing, .receiving: return
        case .waiting, .prepared: break
        }
        if case .update(let sent) = outbound[.update(pending.input.requestID)], sent.original === pending,
           !sent.sendStarted {
            outbound.removeValue(forKey: .update(pending.input.requestID))
        }
        if pendingSettingsUpdate === pending { pendingSettingsRequest = nil }
    }

    private func completeSettingsUpdateSend(_ sent: SettingsUpdateSend) throws {
        let key = OutboundID.update(sent.original.input.requestID)
        guard case .update(let registered) = outbound[key], registered === sent,
              outboundSocket === sent.original.scope.socket else { throw WireError.conflict }
        guard !sent.failed, isCurrentSettingsScope(sent.original.scope),
              sent.sendReturned, sent.accepted, sent.replyReturned else { return }
        outbound.removeValue(forKey: key)
    }

    private func sendPendingSettingsUpdate(socket: RelayConnection) async throws {
        guard let pending = pendingSettingsUpdate, pending.scope.socket === socket,
              try isCurrentSettingsUpdateOriginal(pending), case .prepared(let envelope) = pending.phase,
              case .update(let sent) = outbound[.update(pending.input.requestID)], sent.original === pending,
              !sent.sendStarted else { return }
        pending.phase = .preparing
        do {
            let current = try await checkSettingsUpdateStore(pending)
            pending.phase = .prepared(envelope)
            guard current, try isCurrentSettingsUpdateOriginal(pending),
                  case .prepared(let retained) = pending.phase, sameSettingsEnvelope(retained, envelope) else {
                if pending.record.status == nil { pending.record.status = .interrupted }
                retirePendingSettingsUpdate(pending)
                return
            }
            guard outboundSocket === socket, case .update(let registered) = outbound[.update(pending.input.requestID)],
                  registered === sent, let bound = sent.envelope, sameSettingsEnvelope(bound, envelope),
                  !sent.sendStarted, outbound[.ordinary(envelope.header.messageID)] == nil else { throw WireError.conflict }
        } catch {
            pending.phase = .prepared(envelope)
            if pending.record.status == nil { pending.record.status = .interrupted }
            retirePendingSettingsUpdate(pending)
            retireSettingsScope(socket: socket)
            throw error
        }
        sent.sendStarted = true
        do {
            try await socket.send(.object(["type": .string("send"), "schema_version": .unsigned(1), "envelope": envelope.wire]))
            sent.sendReturned = true
            try completeSettingsUpdateSend(sent)
        } catch {
            sent.sendReturned = true
            sent.failed = true
            retireSettingsScope(socket: socket)
            throw error
        }
    }

    private func beginSettingsUpdateReceive(_ received: ReceivedRelayFrame,
                                           ticket: SettingsReceiveTicket) throws -> SettingsUpdateReceiveAttempt {
        try Task.checkCancellation()
        guard let owner = settingsReceive, owner.ticket === ticket,
              isCurrentSettingsScope(ticket.scope) else { throw WireError.unavailable }
        guard owner.attempt == nil else { throw WireError.conflict }
        guard case .delivery = received.frame else { throw WireError.invalid }
        let original: SettingsUpdateOriginal
        if let pending = pendingSettingsUpdate {
            guard pending.scope === ticket.scope else { throw WireError.conflict }
            let current = try isCurrentSettingsUpdateOriginal(pending)
            if case .prepared(let prepared) = pending.phase, current,
               case .update(let sent) = outbound[.update(pending.input.requestID)], sent.original === pending,
               sent.sendStarted, let bound = sent.envelope, sameSettingsEnvelope(bound, prepared) {
                pending.phase = .receiving(prepared)
                original = .prepared(pending, prepared)
            } else {
                if !current { retirePendingSettingsUpdate(pending) }
                original = .absent
            }
        } else {
            original = .absent
        }
        let attempt = SettingsUpdateReceiveAttempt(issuer: ObjectIdentifier(self), ticket: ticket,
                                                   received: received, original: original)
        owner.attempt = .update(attempt)
        return attempt
    }

    private func checkSettingsUpdateAttempt(_ attempt: SettingsUpdateReceiveAttempt) throws {
        try Task.checkCancellation()
        guard attempt.issuer == ObjectIdentifier(self),
              let owner = settingsReceive, owner.ticket === attempt.ticket,
              case .update(let registered) = owner.attempt, registered === attempt,
              isCurrentSettingsScope(attempt.ticket.scope) else {
            throw WireError.unavailable
        }
    }

    private func currentSettingsUpdateOriginal(_ attempt: SettingsUpdateReceiveAttempt) throws -> PendingSettingsUpdate? {
        guard case .prepared(let pending, let prepared) = attempt.original else { return nil }
        guard pendingSettingsUpdate === pending, pending.scope === attempt.ticket.scope,
              case .receiving(let retained) = pending.phase,
              sameSettingsEnvelope(retained, prepared) else {
            retireSettingsScope(socket: attempt.ticket.scope.socket)
            throw WireError.conflict
        }
        guard try isCurrentSettingsUpdateOriginal(pending) else {
            retirePendingSettingsUpdate(pending)
            return nil
        }
        return pending
    }

    private func checkSettingsUpdateReceiveStore(_ attempt: SettingsUpdateReceiveAttempt) async throws {
        try checkSettingsUpdateAttempt(attempt)
        _ = try currentSettingsUpdateOriginal(attempt)
        let scope = attempt.ticket.scope
        try await store.checkActive(host: scope.host)
        try checkSettingsUpdateAttempt(attempt)
        _ = try currentSettingsUpdateOriginal(attempt)
        let localID = try await store.identityID()
        try checkSettingsUpdateAttempt(attempt)
        _ = try currentSettingsUpdateOriginal(attempt)
        guard localID == scope.localID else { throw WireError.identity }
    }

    private func matchesSettingsUpdateOriginal(_ attempt: SettingsUpdateReceiveAttempt,
                                              metadata: ReplicaSettingsReplyMetadata) throws -> Bool {
        guard let pending = try currentSettingsUpdateOriginal(attempt),
              case .prepared(_, let prepared) = attempt.original,
              case .command(let requestID) = metadata.correlation, let request = pending.request,
              request.requestID == pending.input.requestID, request.time == pending.input.time,
              request.expectedSettingsRevision == pending.input.expectedSettingsRevision,
              request.enabled == pending.input.enabled, request.dimensions == pending.input.dimensions,
              request.parent.generation == pending.record.generation,
              pending.record.encryptedRequestID == prepared.header.messageID else { return false }
        let scope = attempt.ticket.scope
        return metadata.inReplyTo == prepared.header.messageID && requestID == pending.input.requestID
            && metadata.generation == request.parent.generation
            && pending.input.hostID == scope.host.deviceID
            && metadata.header.sender == scope.host.deviceID && metadata.header.recipient == scope.localID
            && prepared.header.sender == scope.localID && prepared.header.recipient == scope.host.deviceID
            && prepared.header.expires == pending.input.time.expiresAt
            && prepared.header.created >= pending.wallFloor
            && prepared.header.expires > prepared.header.created
            && prepared.header.expires - prepared.header.created <= 30_000
    }

    private func classifySettingsUpdateReceive(_ received: ReceivedRelayFrame,
                                              ticket: SettingsReceiveTicket) async throws -> SettingsUpdateReceiveResult {
        let attempt = try beginSettingsUpdateReceive(received, ticket: ticket)
        try await checkSettingsUpdateReceiveStore(attempt)
        try checkSettingsUpdateAttempt(attempt)
        let original: ReplicaSettingsUpdateReplyExpectation?
        if case .prepared(let pending, let prepared) = attempt.original, let request = pending.request {
            original = ReplicaSettingsUpdateReplyExpectation(request: request, encryptedRequestID: prepared.header.messageID)
        } else { original = nil }
        let classified = try await identity.receiveClassified(received, host: ticket.scope.host, into: store,
                                                              matchingUpdate: original)
        attempt.classified = classified
        try checkSettingsUpdateAttempt(attempt)
        _ = try currentSettingsUpdateOriginal(attempt)
        guard classified.matchesReceivedFrame(received) else { throw WireError.identity }
        try await checkSettingsUpdateReceiveStore(attempt)
        try checkSettingsUpdateAttempt(attempt)
        let verified: VerifiedReplicaSettingsReply
        switch classified.result {
        case .ordinary, .refused:
            return .durable(classified)
        case .settingsUpdate(let proof), .settingsAccess(let proof), .settingsBind(let proof), .settingsSnapshot(let proof), .settingsInspection(let proof):
            verified = proof
        }
        guard verified.matchesVerifiedPeer(ticket.scope.host, localID: ticket.scope.localID) else {
            throw WireError.identity
        }
        let committed = try await verified.commitMetadata(into: store)
        attempt.committed = committed
        try checkSettingsUpdateAttempt(attempt)
        _ = try currentSettingsUpdateOriginal(attempt)
        try await checkSettingsUpdateReceiveStore(attempt)
        try checkSettingsUpdateAttempt(attempt)
        guard committed.matchesVerifiedPeer(ticket.scope.host, localID: ticket.scope.localID) else {
            throw WireError.identity
        }
        guard case .settingsUpdate = classified.result,
              try matchesSettingsUpdateOriginal(attempt, metadata: verified.metadata),
              try matchesSettingsUpdateOriginal(attempt, metadata: committed.metadata) else {
            return .refused(committed)
        }
        guard case .fresh = committed.disposition else { return .duplicate(committed) }
        guard case .unissued = attempt.qualification else { throw WireError.conflict }
        let qualification = ReplicaSettingsUpdateQualification(attempt: attempt, committed: committed)
        attempt.qualification = .issued(ObjectIdentifier(qualification))
        return .qualified(qualification)
    }

    fileprivate func checkSettingsUpdateQualification(_ qualification: ReplicaSettingsUpdateQualification,
                                                     committed: CommittedReplicaSettingsReply) throws {
        let attempt = qualification.attempt
        try checkSettingsUpdateAttempt(attempt)
        guard qualification.committed === committed, attempt.committed === committed,
              case .issued(let identifier) = attempt.qualification,
              identifier == ObjectIdentifier(qualification),
              case .fresh = committed.disposition,
              let classified = attempt.classified, classified.matchesReceivedFrame(attempt.received),
              case .settingsUpdate(let verified) = classified.result,
              verified.matchesVerifiedPeer(attempt.ticket.scope.host, localID: attempt.ticket.scope.localID),
              committed.matchesVerifiedPeer(attempt.ticket.scope.host, localID: attempt.ticket.scope.localID),
              try matchesSettingsUpdateOriginal(attempt, metadata: verified.metadata),
              try matchesSettingsUpdateOriginal(attempt, metadata: committed.metadata) else {
            throw WireError.unavailable
        }
    }

    fileprivate func consumeSettingsUpdateQualification(_ qualification: ReplicaSettingsUpdateQualification,
                                                       committed: CommittedReplicaSettingsReply) throws {
        try checkSettingsUpdateQualification(qualification, committed: committed)
        guard let pending = try currentSettingsUpdateOriginal(qualification.attempt) else {
            throw WireError.unavailable
        }
        qualification.attempt.qualification = .consumed
        pending.consumed = true
        retirePendingSettingsUpdate(pending)
    }

    private func recheckSettingsUpdateQualification(_ qualification: ReplicaSettingsUpdateQualification) async throws {
        try checkSettingsUpdateQualification(qualification, committed: qualification.committed)
        try await checkSettingsUpdateReceiveStore(qualification.attempt)
        try checkSettingsUpdateQualification(qualification, committed: qualification.committed)
    }

    func checkSettingsUpdateProjection(_ qualification: ReplicaSettingsUpdateQualification,
                                             committed: CommittedReplicaSettingsReply) throws {
        try checkSettingsUpdateQualification(qualification, committed: committed)
        guard let pending = try currentSettingsUpdateOriginal(qualification.attempt) else {
            throw WireError.unavailable
        }
        let header = committed.metadata.header
        guard header.created >= pending.wallFloor, header.expires <= pending.input.time.expiresAt else {
            throw WireError.invalid
        }
    }

    private func adoptSettingsUpdateOutcome(_ qualification: ReplicaSettingsUpdateQualification) async throws {
        try await recheckSettingsUpdateQualification(qualification)
        try checkSettingsUpdateProjection(qualification, committed: qualification.committed)
        guard let pending = try currentSettingsUpdateOriginal(qualification.attempt),
              settingsUpdateOriginal === pending.record, pending.record.outcome == nil else { throw WireError.unavailable }
        let outcome = try qualification.committed.projectUpdateOutcome(qualifiedBy: qualification, owner: self)
        pending.record.outcome = outcome
        pending.record.status = .received
        let header = qualification.committed.metadata.header
        let expiresAt = min(pending.input.time.expiresAt, header.expires)
        guard expiresAt > pending.wallFloor else { throw WireError.expired }
        let clip = pending.input.time.expiresAt - expiresAt
        guard clip <= 30_000 else { throw WireError.expired }
        let deadline = pending.deadline.advanced(by: .milliseconds(-Int64(clip)))
        let monotonic = ContinuousClock.now
        let wall = try WireID.now()
        guard isCurrentSettingsScope(pending.scope), wall >= pending.wallFloor, wall < expiresAt,
              monotonic >= pending.monotonicFloor, monotonic < deadline else { throw WireError.expired }
    }

}

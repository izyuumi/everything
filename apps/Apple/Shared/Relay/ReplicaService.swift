import Foundation

struct PairingSummary: Sendable {
    let deviceID: UUID
    let fingerprint: String
    let expires: UInt64
}

struct ReplicaViewState: Sendable {
    let snapshot: ReplicaSnapshot
    let identity: DevicePublicKeys
    let connection: String
}

final class ReplicaSettingsRequestHandle: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    fileprivate init() {}

    var description: String { "ReplicaSettingsRequestHandle([redacted])" }
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
    private var inflight: [UUID: ContinuousClock.Instant] = [:]
    private var acknowledgements = Set<UUID>()
    private var settingsScope: SettingsConnectionScope?
    private var pendingSettingsAccess: PendingSettingsAccess?
    private var settingsReceive: SettingsReceiveOwner?

    fileprivate final class SettingsConnectionScope: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
        let socket: RelayConnection
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

    fileprivate enum SettingsAccessPreparation: CustomStringConvertible, CustomDebugStringConvertible {
        case waiting
        case preparing
        case prepared(EncryptedEnvelope)
        case receiving(EncryptedEnvelope)

        var description: String { "SettingsAccessPreparation([redacted])" }
        var debugDescription: String { description }
    }

    fileprivate final class PendingSettingsAccess: CustomStringConvertible, CustomDebugStringConvertible {
        let handle: ReplicaSettingsRequestHandle
        let scope: SettingsConnectionScope
        let request: RemoteSettingsAccessRequest
        let wallFloor: UInt64
        let monotonicFloor: ContinuousClock.Instant
        let deadline: ContinuousClock.Instant
        var phase = SettingsAccessPreparation.waiting
        var retired = false
        var consumed = false

        init(handle: ReplicaSettingsRequestHandle, scope: SettingsConnectionScope,
             request: RemoteSettingsAccessRequest, wallFloor: UInt64,
             monotonicFloor: ContinuousClock.Instant, deadline: ContinuousClock.Instant) {
            self.handle = handle
            self.scope = scope
            self.request = request
            self.wallFloor = wallFloor
            self.monotonicFloor = monotonicFloor
            self.deadline = deadline
        }

        var description: String { "PendingSettingsAccess([redacted])" }
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
        var attempt: SettingsAccessReceiveAttempt?

        init(ticket: SettingsReceiveTicket) { self.ticket = ticket }

        var description: String { "SettingsReceiveOwner([redacted])" }
        var debugDescription: String { description }
    }

    fileprivate enum SettingsAccessOriginal: CustomStringConvertible, CustomDebugStringConvertible {
        case absent
        case prepared(PendingSettingsAccess, EncryptedEnvelope)

        var description: String { "SettingsAccessOriginal([redacted])" }
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

    private enum SettingsAccessReceiveResult: CustomStringConvertible, CustomDebugStringConvertible {
        case durable(ReplicaClassifiedDelivery)
        case qualified(ReplicaSettingsAccessQualification)
        case duplicate(CommittedReplicaSettingsReply)
        case refused(CommittedReplicaSettingsReply)

        var description: String { "SettingsAccessReceiveResult([redacted])" }
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
        ReplicaViewState(snapshot: try await store.snapshot(), identity: identity.publicKeys, connection: status)
    }

    func requestSettingsAccess() throws -> ReplicaSettingsRequestHandle {
        try Task.checkCancellation()
        guard let scope = settingsScope, isCurrentSettingsScope(scope) else { throw WireError.unavailable }
        guard pendingSettingsAccess == nil else { throw WireError.queueFull }
        let monotonicFloor = ContinuousClock.now
        let wallFloor = try WireID.now()
        let expires = wallFloor.addingReportingOverflow(30_000)
        guard !expires.overflow, expires.partialValue <= UInt64(Int64.max) else { throw WireError.expired }
        let time = try RemoteSettingsRequestTime(issuedAt: wallFloor, expiresAt: expires.partialValue)
        let request = try RemoteSettingsAccessRequest(hostID: scope.host.deviceID, queryID: UUID(), time: time)
        let handle = ReplicaSettingsRequestHandle()
        pendingSettingsAccess = PendingSettingsAccess(
            handle: handle, scope: scope, request: request, wallFloor: wallFloor,
            monotonicFloor: monotonicFloor, deadline: monotonicFloor.advanced(by: .milliseconds(30_000))
        )
        return handle
    }

    func cancelSettings(_ handle: ReplicaSettingsRequestHandle) {
        guard let pending = pendingSettingsAccess, pending.handle === handle else { return }
        retirePendingSettingsAccess(pending)
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

    func resolve(_ approval: ReplicaApproval, allow: Bool) async throws {
        try await submit(conversation: approval.conversationID, payload: .object(["kind": .string("resolve_permission"), "operation_id": .id(approval.id), "decision": .string(allow ? "allow" : "deny")]), now: WireID.now(), permission: approval)
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
        inflight.removeAll()
        acknowledgements.removeAll()
        reconnectForCapacity = false
        status = "Disconnected"
        self.stopping = nil
    }

    func disable() async throws {
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
                inflight.removeAll()
                acknowledgements.removeAll()
                reconnectForCapacity = false
                status = "Connected"
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

    private func pump(socket: RelayConnection, synchronize refresh: Bool) async throws {
        try Task.checkCancellation()
        try await preparePendingSettingsAccess(socket: socket)
        try await store.checkActive()
        let inboxDrained = try await drainInbox()
        if refresh, inboxDrained {
            do { try await synchronizeOne() }
            catch WireError.queueFull { status = "Local queue is full; preserving pending work" }
        }
        if inflight.values.contains(where: { $0.duration(to: .now) > .seconds(15) }) { throw WireError.timeout }
        let now = try WireID.now()
        let rows = try await store.outbound(now: now, excluding: Set(inflight.keys))
        for row in rows.prefix(max(0, 16 - inflight.count)) {
            try Task.checkCancellation()
            try await store.checkActive()
            try row.envelope.header.validate(now: WireID.now())
            try await store.attempted(row.id, now: now)
            inflight[row.id] = .now
            try await socket.send(.object(["type": .string("send"), "schema_version": .unsigned(1), "envelope": row.envelope.wire]))
        }
        if reconnectForCapacity && inflight.isEmpty && inboxDrained && rows.isEmpty { throw WireError.network }
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
            guard inflight[id] != nil else { throw WireError.conflict }
            try await store.relayAccepted(id)
            inflight.removeValue(forKey: id)
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
        guard settingsScope == nil, pendingSettingsAccess == nil, settingsReceive == nil else { throw WireError.queueFull }
        settingsScope = SettingsConnectionScope(socket: socket, host: host, localID: identity.publicKeys.deviceID, epoch: captured)
    }

    private func retirePendingSettingsAccess(_ pending: PendingSettingsAccess) {
        pending.retired = true
        switch pending.phase {
        case .preparing, .receiving: return
        case .waiting, .prepared: break
        }
        if pendingSettingsAccess === pending { pendingSettingsAccess = nil }
    }

    private func retireSettingsScope(socket: RelayConnection? = nil) {
        if let scope = settingsScope, socket == nil || scope.socket === socket {
            settingsScope = nil
        }
        if let pending = pendingSettingsAccess, socket == nil || pending.scope.socket === socket {
            retirePendingSettingsAccess(pending)
        }
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
            retirePendingSettingsAccess(pending)
            return
        }
        guard case .waiting = pending.phase else { return }
        pending.phase = .preparing
        do {
            do {
                let envelope = try await prepareSettingsAccessEnvelope(pending)
                if let envelope, try isCurrentSettingsOriginal(pending) {
                    pending.phase = .prepared(envelope)
                }
            }
            if case .preparing = pending.phase {
                pending.phase = .waiting
                retirePendingSettingsAccess(pending)
            }
        } catch {
            pending.phase = .waiting
            retirePendingSettingsAccess(pending)
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
        try await handle(received.frame, socket: socket)
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
            if case .prepared(let prepared) = pending.phase, current {
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
        owner.attempt = attempt
        return attempt
    }

    private func checkSettingsAccessAttempt(_ attempt: SettingsAccessReceiveAttempt) throws {
        try Task.checkCancellation()
        guard attempt.issuer == ObjectIdentifier(self),
              let owner = settingsReceive, owner.ticket === attempt.ticket,
              owner.attempt === attempt, isCurrentSettingsScope(attempt.ticket.scope) else {
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
        case .settingsAccess(let proof), .settingsBind(let proof):
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

    private func finishSettingsReceive(_ ticket: SettingsReceiveTicket) throws {
        guard let owner = settingsReceive, owner.ticket === ticket else {
            retireSettingsScope(socket: ticket.scope.socket)
            throw WireError.conflict
        }
        var completion: (PendingSettingsAccess, EncryptedEnvelope, Bool)?
        do {
            if let attempt = owner.attempt {
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
                    completion = (pending, prepared, try isCurrentSettingsOriginal(pending))
                }
                attempt.classified = nil
                attempt.committed = nil
                owner.attempt = nil
            }
        }
        if let (pending, prepared, current) = completion {
            if current {
                pending.phase = .prepared(prepared)
            } else {
                pending.retired = true
                pending.phase = .waiting
                pendingSettingsAccess = nil
            }
        }
        settingsReceive = nil
    }
}

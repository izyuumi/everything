import Foundation

struct RelayOrigin: Sendable, Equatable {
    let value: String
    let endpoint: URL

    init(_ value: String) throws {
        guard value.utf8.count <= 512, value.utf8.allSatisfy({ $0 >= 33 && $0 < 127 }),
              let parts = URLComponents(string: value), let scheme = parts.scheme, ["wss", "ws"].contains(scheme),
              parts.user == nil, parts.password == nil, parts.query == nil, parts.fragment == nil,
              parts.path.isEmpty, let host = parts.host, !host.isEmpty, host == host.lowercased(),
              host.allSatisfy({ $0.isASCII && ($0.isLetter || $0.isNumber || $0 == "." || $0 == "-") }),
              parts.port == nil || (1...65535).contains(parts.port ?? 0),
              !(parts.scheme == "wss" && parts.port == 443), !(parts.scheme == "ws" && parts.port == 80),
              parts.scheme != "ws" || host == "127.0.0.1",
              parts.string == value, let endpoint = URL(string: value + "/ws") else { throw WireError.invalid }
        self.value = value
        self.endpoint = endpoint
    }
}

enum RelayFrame: Sendable {
    case accepted(UUID)
    case delivery(EncryptedEnvelope)
    case acknowledged(UUID)
    case pong
}

final class RelaySessionDelegate: NSObject, URLSessionTaskDelegate, Sendable {
    func urlSession(_ session: URLSession, task: URLSessionTask, willPerformHTTPRedirection response: HTTPURLResponse, newRequest request: URLRequest, completionHandler: @escaping @Sendable (URLRequest?) -> Void) {
        completionHandler(nil)
    }
}

actor RelayConnection {
    private let socket: URLSessionWebSocketTask
    private let session: URLSession
    private let localID: UUID
    private var closed = false

    private init(origin: RelayOrigin, localID: UUID) {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.httpCookieStorage = nil
        configuration.urlCache = nil
        configuration.requestCachePolicy = .reloadIgnoringLocalCacheData
        configuration.timeoutIntervalForRequest = 10
        configuration.timeoutIntervalForResource = 90
        let createdSession = URLSession(configuration: configuration, delegate: RelaySessionDelegate(), delegateQueue: nil)
        session = createdSession
        socket = createdSession.webSocketTask(with: origin.endpoint)
        socket.maximumMessageSize = 2_097_152
        self.localID = localID
        socket.resume()
    }

    static func connect(origin: RelayOrigin, identity: DeviceIdentity) async throws -> RelayConnection {
        let own = identity.publicKeys
        let connection = RelayConnection(origin: origin, localID: own.deviceID)
        do {
            try await connection.authenticate(origin: origin, identity: identity)
            return connection
        } catch {
            await connection.close()
            throw error
        }
    }

    private func authenticate(origin: RelayOrigin, identity: DeviceIdentity) async throws {
        let deadline = ContinuousClock.now.advanced(by: .seconds(20))
        try await send(.object(["type": .string("begin_authentication"), "schema_version": .unsigned(1), "device_id": .id(localID)]))
        let raw = try await read(seconds: 20, maximum: 4_096)
        let fields = try raw.object(keys: ["type", "schema_version", "challenge"], required: ["type", "schema_version", "challenge"])
        guard try fields.required("type").text == "challenge", try fields.required("schema_version").integer == 1 else { throw WireError.invalid }
        let challenge = try fields.required("challenge").object(keys: ["schema_version", "session_id", "device_id", "origin", "issued_at_ms", "expires_at_ms", "nonce"], required: ["schema_version", "session_id", "device_id", "origin", "issued_at_ms", "expires_at_ms", "nonce"])
        let sessionID = try challenge.required("session_id").identifier
        let issued = try challenge.required("issued_at_ms").integer
        let expires = try challenge.required("expires_at_ms").integer
        let now = try WireID.now()
        guard try challenge.required("schema_version").integer == 1,
              try challenge.required("device_id").identifier == localID,
              try challenge.required("origin").text == origin.value,
              expires > now, expires > issued, expires - issued <= 20_000, issued <= now + 120_000 else { throw WireError.identity }
        var canonical = Data.domain("everything.relay.challenge.v1")
        canonical.appendBE(UInt16(1))
        canonical += WireID.bytes(sessionID) + WireID.bytes(localID)
        canonical.appendBE(UInt16(origin.value.utf8.count))
        canonical += Data(origin.value.utf8)
        canonical.appendBE(issued)
        canonical.appendBE(expires)
        canonical += try challenge.required("nonce").bytes(count: 32)
        let signature = try await identity.signChallenge(canonical)
        try await send(.object(["type": .string("authenticate"), "schema_version": .unsigned(1), "session_id": .id(sessionID), "signature": .bytes(signature)]))
        let remaining = ContinuousClock.now.duration(to: deadline)
        let current = try WireID.now()
        guard remaining > .zero, current < expires else { throw WireError.timeout }
        let components = remaining.components
        let seconds = Double(components.seconds) + Double(components.attoseconds) / 1_000_000_000_000_000_000
        let result = try await read(seconds: min(seconds, Double(expires - current) / 1_000), maximum: 4_096)
        let authenticated = try result.object(keys: ["type", "schema_version", "device_id"], required: ["type", "schema_version", "device_id"])
        guard try authenticated.required("type").text == "authenticated", try authenticated.required("schema_version").integer == 1,
              try authenticated.required("device_id").identifier == localID else { throw WireError.identity }
    }

    func send(_ frame: WireValue) async throws {
        guard !closed else { throw WireError.network }
        let data = try frame.encoded(maximum: 2_097_152)
        let socket = socket
        _ = try await Self.bounded(socket: socket, seconds: 5) {
            try await socket.send(.data(data))
            return true
        }
    }

    func receive() async throws -> RelayFrame {
        let raw = try await read(seconds: 90, maximum: 2_097_152)
        let type = try raw.field("type").text
        let keys: Set<String>
        switch type {
        case "accepted": keys = ["type", "schema_version", "message_id", "duplicate"]
        case "delivery": keys = ["type", "schema_version", "envelope"]
        case "acknowledged": keys = ["type", "schema_version", "message_id"]
        case "pong": keys = ["type", "schema_version"]
        case "error": keys = ["type", "schema_version", "code"]
        default: throw WireError.invalid
        }
        let fields = try raw.object(keys: keys, required: keys)
        guard try fields.required("schema_version").integer == 1 else { throw WireError.invalid }
        switch type {
        case "accepted":
            _ = try fields.required("duplicate").flag
            return .accepted(try fields.required("message_id").identifier)
        case "delivery":
            let envelope = try EncryptedEnvelope(fields.required("envelope"))
            guard envelope.header.recipient == localID else { throw WireError.identity }
            return .delivery(envelope)
        case "acknowledged": return .acknowledged(try fields.required("message_id").identifier)
        case "pong": return .pong
        case "error":
            let code = try fields.required("code").text
            if ["rate_limited", "queue_full", "session_already_active", "internal", "recipient_unavailable"].contains(code) { throw WireError.network }
            throw WireError.identity
        default: throw WireError.invalid
        }
    }

    private func read(seconds: Double, maximum: Int) async throws -> WireValue {
        guard !closed else { throw WireError.network }
        let socket = socket
        let data = try await Self.bounded(socket: socket, seconds: seconds) {
            switch try await socket.receive() {
            case .data(let data):
                guard data.count <= maximum else { throw WireError.size }
                return data
            case .string(let string):
                guard string.utf8.count <= maximum else { throw WireError.size }
                return Data(string.utf8)
            @unknown default: throw WireError.invalid
            }
        }
        return try WireValue.parse(data, maximum: maximum)
    }

    private static func bounded<T: Sendable>(socket: URLSessionWebSocketTask, seconds: Double, operation: @escaping @Sendable () async throws -> T) async throws -> T {
        try await withTaskCancellationHandler {
            try await withThrowingTaskGroup(of: T.self) { group in
                group.addTask(operation: operation)
                group.addTask {
                    try await Task.sleep(for: .milliseconds(Int64(max(1, seconds * 1_000))))
                    socket.cancel(with: .goingAway, reason: nil)
                    throw WireError.timeout
                }
                defer { group.cancelAll() }
                guard let value = try await group.next() else { throw WireError.network }
                return value
            }
        } onCancel: {
            socket.cancel(with: .goingAway, reason: nil)
        }
    }

    func close() {
        closed = true
        socket.cancel(with: .goingAway, reason: nil)
        session.invalidateAndCancel()
    }
}

import CryptoKit
import Foundation
import SQLite3

struct ReplicaConversation: Identifiable, Sendable {
    let id: UUID
    let title: String
    let cursor: UInt64
}

struct ReplicaTask: Identifiable, Sendable {
    let id: UUID
    let conversationID: UUID
    let instruction: String
    let status: String
}

struct ReplicaApproval: Identifiable, Sendable, Equatable {
    let id: UUID
    let conversationID: UUID
    let taskID: UUID
    let tool: String
    let callID: UUID
    let arguments: String?
    let inspection: WirePermissionInspection?

    func canAllow(at date: Date) -> Bool {
        let milliseconds = date.timeIntervalSince1970 * 1_000
        guard milliseconds.isFinite, milliseconds >= 0, milliseconds < Double(Int64.max), let inspection else { return false }
        let now = UInt64(milliseconds.rounded(.down))
        return id == callID && inspection.requestedAt <= now && now < inspection.expiresAt
    }
}

struct ReplicaPendingCommand: Identifiable, Sendable {
    let id: UUID
    let conversationID: UUID
    let original: WireValue
    let state: String
    let result: WireValue?
}

struct ReplicaSnapshot: Sendable {
    let host: DevicePublicKeys?
    let origin: String?
    let revoked: Bool
    let conversations: [ReplicaConversation]
    let tasks: [ReplicaTask]
    let approvals: [ReplicaApproval]
    let commands: [ReplicaPendingCommand]
}

enum ReplicaNotificationCategory: String, Sendable {
    case completion
    case attention
}

struct ReplicaNotificationNotice: Sendable {
    let category: ReplicaNotificationCategory
    let identifier: String
}

struct ReplicaNotificationPage: Sendable {
    let notices: [ReplicaNotificationNotice]
    let hasMore: Bool
}

struct StoredDelivery: Sendable {
    let id: UUID
    let payload: Data
}

struct StoredOutbound: Sendable {
    let id: UUID
    let envelope: EncryptedEnvelope
}

private enum SQLValue {
    case text(String), integer(Int64), bytes(Data), null
}

private final class ReplicaDatabase: @unchecked Sendable {
    let handle: OpaquePointer
    private let transient = unsafeBitCast(-1, to: sqlite3_destructor_type.self)

    init(url: URL) throws {
        var opened: OpaquePointer?
        guard sqlite3_open_v2(url.path, &opened, SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE | SQLITE_OPEN_FULLMUTEX, nil) == SQLITE_OK, let opened else {
            if let opened { sqlite3_close(opened) }
            throw WireError.storage
        }
        handle = opened
        sqlite3_busy_timeout(handle, 250)
    }

    deinit { sqlite3_close(handle) }

    func execute(_ sql: String, _ values: [SQLValue] = []) throws {
        try rows(sql, values) { _ in () }
    }

    @discardableResult
    func rows<T>(_ sql: String, _ values: [SQLValue] = [], map: (OpaquePointer) throws -> T) throws -> [T] {
        var statement: OpaquePointer?
        try check(sqlite3_prepare_v2(handle, sql, -1, &statement, nil))
        guard let statement else { throw WireError.storage }
        defer { sqlite3_finalize(statement) }
        for (index, value) in values.enumerated() {
            let offset = Int32(index + 1)
            let result: Int32
            switch value {
            case .text(let text): result = sqlite3_bind_text(statement, offset, text, -1, transient)
            case .integer(let integer): result = sqlite3_bind_int64(statement, offset, integer)
            case .bytes(let data):
                result = data.withUnsafeBytes { sqlite3_bind_blob(statement, offset, $0.baseAddress, Int32(data.count), transient) }
            case .null: result = sqlite3_bind_null(statement, offset)
            }
            try check(result)
        }
        var output: [T] = []
        while true {
            let result = sqlite3_step(statement)
            if result == SQLITE_DONE { return output }
            guard result == SQLITE_ROW else { try check(result); throw WireError.storage }
            output.append(try map(statement))
        }
    }

    func transaction<T>(_ operation: () throws -> T) throws -> T {
        try execute("BEGIN IMMEDIATE")
        do {
            let result = try operation()
            try execute("COMMIT")
            return result
        } catch {
            do { try execute("ROLLBACK") } catch { throw WireError.storage }
            throw error
        }
    }

    private func check(_ code: Int32) throws {
        if code == SQLITE_OK || code == SQLITE_DONE || code == SQLITE_ROW { return }
        if code == SQLITE_FULL { throw WireError.queueFull }
        if code == SQLITE_CONSTRAINT { throw WireError.conflict }
        throw WireError.storage
    }

    static func text(_ row: OpaquePointer, _ index: Int32) throws -> String {
        guard let bytes = sqlite3_column_text(row, index) else { throw WireError.storage }
        return String(cString: bytes)
    }

    static func data(_ row: OpaquePointer, _ index: Int32) throws -> Data {
        let count = Int(sqlite3_column_bytes(row, index))
        guard count >= 0, count <= 2_097_152 else { throw WireError.storage }
        if count == 0 { return Data() }
        guard let bytes = sqlite3_column_blob(row, index) else { throw WireError.storage }
        return Data(bytes: bytes, count: count)
    }

    func permissionDecision(operation: String, conversation: String) throws -> String? {
        let existing = try rows("SELECT conversation,decision FROM permission_decisions WHERE operation=?", [.text(operation)]) {
            (try Self.text($0, 0), try Self.text($0, 1))
        }.first
        guard let existing else { return nil }
        guard existing.0 == conversation, ["allow", "deny"].contains(existing.1) else { throw WireError.conflict }
        return existing.1
    }

    func retainPermissionDecision(_ event: WireEvent) throws {
        guard event.kind == "permission_decided" else { throw WireError.invalid }
        let operation = try event.payload.field("operation_id").identifier.uuidString.lowercased()
        let conversation = event.conversationID.uuidString.lowercased()
        let decision = try event.payload.field("decision").text
        guard ["allow", "deny"].contains(decision) else { throw WireError.invalid }
        let requestedConversation = try rows("SELECT conversation FROM approvals WHERE id=?", [.text(operation)]) { try Self.text($0, 0) }.first
        if let requestedConversation { guard requestedConversation == conversation else { throw WireError.conflict } }
        if let previous = try permissionDecision(operation: operation, conversation: conversation) {
            guard previous == decision else { throw WireError.conflict }
        } else {
            try execute("INSERT INTO permission_decisions(operation,conversation,decision) VALUES (?,?,?)", [.text(operation), .text(conversation), .text(decision)])
        }
        try execute("UPDATE approvals SET decided=1 WHERE id=? AND conversation=?", [.text(operation), .text(conversation)])
    }

    func restorePermissionDecisions() throws {
        var after: Int64 = 0
        var records = 0
        var bytes = 0
        while true {
            let page = try rows("SELECT rowid,conversation,sequence,event_id,envelope FROM events WHERE kind='permission_decided' AND rowid>? ORDER BY rowid LIMIT 8", [.integer(after)]) { row in
                let length = Int(sqlite3_column_bytes(row, 4))
                guard length > 0, length <= 262_144 else { throw WireError.storage }
                let data = try Self.data(row, 4)
                let event = try WireEvent(WireValue.parse(data, maximum: 262_144))
                guard event.kind == "permission_decided",
                      event.conversationID.uuidString.lowercased() == (try Self.text(row, 1)),
                      Int64(event.sequence) == sqlite3_column_int64(row, 2),
                      event.id.uuidString.lowercased() == (try Self.text(row, 3)) else { throw WireError.conflict }
                return (sqlite3_column_int64(row, 0), event, data.count)
            }
            if page.isEmpty { return }
            for (row, event, count) in page {
                guard records < 262_144, bytes <= 67_108_864 - count else { throw WireError.queueFull }
                try retainPermissionDecision(event)
                after = row
                records += 1
                bytes += count
            }
        }
    }
}

actor ReplicaStore: DurableEnvelopeInbox {
    private let database: ReplicaDatabase

    init(directory: URL) throws {
        let manager = FileManager.default
        try manager.createDirectory(at: directory, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700, .protectionKey: FileProtectionType.complete])
        var protectedDirectory = directory
        var resourceValues = URLResourceValues()
        resourceValues.isExcludedFromBackup = true
        try protectedDirectory.setResourceValues(resourceValues)
        database = try ReplicaDatabase(url: directory.appendingPathComponent("replica-v1.sqlite3"))
        let version = try database.rows("PRAGMA user_version") { sqlite3_column_int64($0, 0) }.first ?? 0
        guard (0...3).contains(version) else { throw WireError.storage }
        for sql in [
            "PRAGMA page_size=4096", "PRAGMA max_page_count=16384", "PRAGMA journal_mode=WAL", "PRAGMA synchronous=FULL", "PRAGMA foreign_keys=ON",
            "CREATE TABLE IF NOT EXISTS metadata(key TEXT PRIMARY KEY,value BLOB NOT NULL)",
            "CREATE TABLE IF NOT EXISTS inbox(id TEXT PRIMARY KEY,digest BLOB NOT NULL,sender TEXT NOT NULL,recipient TEXT NOT NULL,expires INTEGER NOT NULL,payload BLOB,done INTEGER NOT NULL DEFAULT 0)",
            "CREATE TABLE IF NOT EXISTS outbox(id TEXT PRIMARY KEY,envelope BLOB NOT NULL,expires INTEGER NOT NULL,created INTEGER NOT NULL,attempts INTEGER NOT NULL DEFAULT 0,not_before INTEGER NOT NULL DEFAULT 0)",
            "CREATE TABLE IF NOT EXISTS conversations(id TEXT PRIMARY KEY,title TEXT NOT NULL,cursor INTEGER NOT NULL DEFAULT 0)",
            "CREATE TABLE IF NOT EXISTS events(conversation TEXT NOT NULL,sequence INTEGER NOT NULL,event_id TEXT NOT NULL UNIQUE,kind TEXT NOT NULL,envelope BLOB NOT NULL,PRIMARY KEY(conversation,sequence))",
            "CREATE TABLE IF NOT EXISTS commands(id TEXT PRIMARY KEY,conversation TEXT NOT NULL,original BLOB NOT NULL,envelope BLOB NOT NULL,state TEXT NOT NULL,result BLOB)",
            "CREATE TABLE IF NOT EXISTS tasks(id TEXT PRIMARY KEY,conversation TEXT NOT NULL,instruction TEXT NOT NULL,status TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS calls(id TEXT PRIMARY KEY,arguments BLOB NOT NULL)",
            "CREATE TABLE IF NOT EXISTS approvals(id TEXT PRIMARY KEY,conversation TEXT NOT NULL,task TEXT NOT NULL,tool TEXT NOT NULL,call TEXT NOT NULL,decided INTEGER NOT NULL DEFAULT 0,inspection BLOB)",
            "CREATE TABLE IF NOT EXISTS permission_decisions(operation TEXT PRIMARY KEY,conversation TEXT NOT NULL,decision TEXT NOT NULL)"
        ] { try database.execute(sql) }
        try database.transaction {
            let columns = try database.rows("PRAGMA table_info(approvals)") { try ReplicaDatabase.text($0, 1) }
            if !columns.contains("inspection") {
                guard version < 2 else { throw WireError.storage }
                try database.execute("ALTER TABLE approvals ADD COLUMN inspection BLOB")
            }
            if version < 3 { try database.restorePermissionDecisions() }
            try database.execute("PRAGMA user_version=3")
        }
        for name in ["replica-v1.sqlite3", "replica-v1.sqlite3-wal", "replica-v1.sqlite3-shm"] {
            let path = directory.appendingPathComponent(name).path
            if manager.fileExists(atPath: path) { try manager.setAttributes([.protectionKey: FileProtectionType.complete, .posixPermissions: 0o600], ofItemAtPath: path) }
        }
    }

    private func metadata(_ key: String) throws -> Data? {
        try database.rows("SELECT value FROM metadata WHERE key=?", [.text(key)]) { try ReplicaDatabase.data($0, 0) }.first
    }

    private func setMetadata(_ key: String, _ data: Data) throws {
        try database.execute("INSERT INTO metadata(key,value) VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [.text(key), .bytes(data)])
    }

    func identityID() throws -> UUID? {
        guard let data = try metadata("local_id") else { return nil }
        guard let value = String(data: data, encoding: .utf8), let id = UUID(uuidString: value), id != WireID.zero else { throw WireError.identity }
        return id
    }

    func bindIdentity(_ id: UUID) throws {
        try database.transaction {
            if let current = try identityID(), current != id { throw WireError.identity }
            try setMetadata("local_id", Data(id.uuidString.lowercased().utf8))
        }
    }

    func host() throws -> DevicePublicKeys? {
        guard let data = try metadata("host") else { return nil }
        return try DevicePublicKeys(WireValue.parse(data, maximum: 4_096))
    }

    func checkActive(host expected: DevicePublicKeys? = nil) throws {
        guard try metadata("revoked") == nil else { throw WireError.revoked }
        if let expected, try host() != expected { throw WireError.identity }
    }

    func pin(_ keys: DevicePublicKeys, pairingID: UUID) throws {
        try database.transaction {
            try checkActive()
            if let existing = try host(), existing != keys { throw WireError.identity }
            guard try metadata("pairing_" + pairingID.uuidString) == nil else { throw WireError.pairing }
            try setMetadata("host", keys.wire.encoded(maximum: 4_096))
            try setMetadata("pairing_" + pairingID.uuidString, Data([1]))
        }
    }

    func configure(origin: RelayOrigin) throws {
        try checkActive()
        try setMetadata("origin", Data(origin.value.utf8))
    }

    func addConversation(_ id: UUID, title: String) throws {
        try checkActive()
        guard !title.contains("\0"), title.utf8.count <= 256 else { throw WireError.size }
        let count = try database.rows("SELECT count(*) FROM conversations") { sqlite3_column_int64($0, 0) }.first ?? 0
        guard try count < 64 || cursor(id) != nil else { throw WireError.queueFull }
        try database.execute("INSERT INTO conversations(id,title) VALUES (?,?) ON CONFLICT(id) DO UPDATE SET title=excluded.title", [.text(id.uuidString.lowercased()), .text(title)])
    }

    func cursor(_ id: UUID) throws -> UInt64? {
        try database.rows("SELECT cursor FROM conversations WHERE id=?", [.text(id.uuidString.lowercased())]) { row in
            let value = sqlite3_column_int64(row, 0)
            guard value >= 0 else { throw WireError.storage }
            return UInt64(value)
        }.first
    }

    func queue(_ envelopes: [EncryptedEnvelope], host: DevicePublicKeys) throws {
        try database.transaction {
            try checkActive(host: host)
            for envelope in envelopes { try insertOutbound(envelope, host: host) }
        }
    }

    private func insertOutbound(_ envelope: EncryptedEnvelope, host: DevicePublicKeys) throws {
        guard let localID = try identityID(), envelope.header.sender == localID,
              envelope.header.recipient == host.deviceID, envelope.header.expires <= UInt64(Int64.max) else { throw WireError.identity }
        let data = try envelope.wire.encoded(maximum: 2_097_152)
        let totals = try database.rows("SELECT count(*),coalesce(sum(length(envelope)),0) FROM outbox") { (sqlite3_column_int64($0, 0), sqlite3_column_int64($0, 1)) }.first ?? (0, 0)
        guard totals.0 < 128, totals.1 + Int64(data.count) <= 16_777_216 else { throw WireError.queueFull }
        try database.execute("INSERT INTO outbox(id,envelope,expires,created) VALUES (?,?,?,?)", [.text(envelope.header.messageID.uuidString.lowercased()), .bytes(data), .integer(Int64(envelope.header.expires)), .integer(Int64(envelope.header.created))])
    }

    func queueCommand(_ original: WireValue, envelope: EncryptedEnvelope, host: DevicePublicKeys, permission: ReplicaApproval? = nil) throws {
        let id = try original.field("command_id").identifier.uuidString.lowercased()
        let conversation = try original.field("conversation_id").identifier.uuidString.lowercased()
        let encoded = try original.encoded(maximum: 262_144)
        try database.transaction {
            try checkActive(host: host)
            let payload = try original.field("payload")
            if try payload.field("kind").text == "resolve_permission" {
                guard let permission, permission.conversationID.uuidString.lowercased() == conversation,
                      try payload.field("operation_id").identifier == permission.id else { throw WireError.conflict }
                let decision = try payload.field("decision").text
                guard ["allow", "deny"].contains(decision) else { throw WireError.invalid }
                try requireCurrent(permission, allow: decision == "allow", now: WireID.now())
            } else if permission != nil { throw WireError.invalid }
            let previous = try database.rows("SELECT original FROM commands WHERE id=?", [.text(id)]) { try ReplicaDatabase.data($0, 0) }.first
            if let previous {
                guard previous == encoded else { throw WireError.conflict }
                return
            }
            let count = try database.rows("SELECT count(*) FROM commands") { sqlite3_column_int64($0, 0) }.first ?? 0
            guard count < 5_000 else { throw WireError.queueFull }
            try insertOutbound(envelope, host: host)
            try database.execute("INSERT INTO commands(id,conversation,original,envelope,state) VALUES (?,?,?,?,'queued')", [.text(id), .text(conversation), .bytes(encoded), .bytes(envelope.wire.encoded(maximum: 2_097_152))])
        }
    }

    func outbound(now: UInt64, excluding: Set<UUID>) throws -> [StoredOutbound] {
        try checkActive()
        guard now <= UInt64(Int64.max), let host = try host(), let localID = try identityID() else { throw WireError.identity }
        try database.transaction {
            let expired = try database.rows("SELECT envelope FROM outbox WHERE expires<=?", [.integer(Int64(now))]) { try ReplicaDatabase.data($0, 0) }
            for envelope in expired {
                try database.execute("UPDATE commands SET state='delivery_expired_outcome_unknown' WHERE envelope=? AND state='queued'", [.bytes(envelope)])
            }
            try database.execute("DELETE FROM outbox WHERE expires<=?", [.integer(Int64(now))])
            try database.execute("DELETE FROM inbox WHERE done=1 AND expires<=?", [.integer(Int64(now))])
        }
        let identifiers = try database.rows("SELECT id FROM outbox WHERE not_before<=? ORDER BY created,id LIMIT 128", [.integer(Int64(now))]) { row in
            guard let id = UUID(uuidString: try ReplicaDatabase.text(row, 0)) else { throw WireError.storage }
            return id
        }
        return try identifiers.filter { !excluding.contains($0) }.prefix(16).map { id in
            guard let data = try database.rows("SELECT envelope FROM outbox WHERE id=?", [.text(id.uuidString.lowercased())], map: { try ReplicaDatabase.data($0, 0) }).first else { throw WireError.storage }
            let envelope = try EncryptedEnvelope(WireValue.parse(data, maximum: 2_097_152))
            guard envelope.header.recipient == host.deviceID, envelope.header.sender == localID else { throw WireError.identity }
            return StoredOutbound(id: id, envelope: envelope)
        }
    }

    func attempted(_ id: UUID, now: UInt64) throws {
        try checkActive()
        guard now <= UInt64(Int64.max) - 2_000 else { throw WireError.invalid }
        try database.execute("UPDATE outbox SET attempts=min(attempts+1,32),not_before=? WHERE id=?", [.integer(Int64(now + 2_000)), .text(id.uuidString.lowercased())])
    }

    func relayAccepted(_ id: UUID) throws {
        try database.transaction {
            try checkActive()
            let data = try database.rows("SELECT envelope FROM outbox WHERE id=?", [.text(id.uuidString.lowercased())]) { try ReplicaDatabase.data($0, 0) }.first
            guard let data else { throw WireError.conflict }
            try database.execute("UPDATE commands SET state='relayed' WHERE envelope=? AND state='queued'", [.bytes(data)])
            try database.execute("DELETE FROM outbox WHERE id=?", [.text(id.uuidString.lowercased())])
        }
    }

    func acceptAuthenticated(_ envelope: EncryptedEnvelope, plaintext: Data, host: DevicePublicKeys, localID: UUID) throws -> Bool {
        try recordAuthenticated(envelope, plaintext: plaintext, host: host, localID: localID)
    }

    func rejectAuthenticated(_ envelope: EncryptedEnvelope, host: DevicePublicKeys, localID: UUID) throws -> Bool {
        try recordAuthenticated(envelope, plaintext: nil, host: host, localID: localID)
    }

    private func recordAuthenticated(_ envelope: EncryptedEnvelope, plaintext: Data?, host: DevicePublicKeys, localID: UUID) throws -> Bool {
        try database.transaction {
            try checkActive(host: host)
            guard try identityID() == localID, envelope.header.recipient == localID,
                  envelope.header.sender == host.deviceID, envelope.header.expires <= UInt64(Int64.max) else { throw WireError.identity }
            let id = envelope.header.messageID.uuidString.lowercased()
            let digest = Data(SHA256.hash(data: try envelope.wire.encoded(maximum: 2_097_152)))
            if let previous = try database.rows("SELECT digest FROM inbox WHERE id=?", [.text(id)], map: { try ReplicaDatabase.data($0, 0) }).first {
                guard previous == digest else { throw WireError.conflict }
                return false
            }
            let totals = try database.rows("SELECT count(*),coalesce(sum(length(payload)),0),sum(CASE WHEN done=0 THEN 1 ELSE 0 END) FROM inbox") { (sqlite3_column_int64($0, 0), sqlite3_column_int64($0, 1), sqlite3_column_int64($0, 2)) }.first ?? (0, 0, 0)
            guard totals.0 < 150_000 else { throw WireError.queueFull }
            if let plaintext {
                guard totals.2 < 128, totals.1 + Int64(plaintext.count) <= 16_777_216 else { throw WireError.queueFull }
                try database.execute("INSERT INTO inbox(id,digest,sender,recipient,expires,payload) VALUES (?,?,?,?,?,?)", [.text(id), .bytes(digest), .text(host.deviceID.uuidString.lowercased()), .text(localID.uuidString.lowercased()), .integer(Int64(envelope.header.expires)), .bytes(plaintext)])
            } else {
                try database.execute("INSERT INTO inbox(id,digest,sender,recipient,expires,payload,done) VALUES (?,?,?,?,?,NULL,1)", [.text(id), .bytes(digest), .text(host.deviceID.uuidString.lowercased()), .text(localID.uuidString.lowercased()), .integer(Int64(envelope.header.expires))])
            }
            return true
        }
    }

    func pendingInbox() throws -> [StoredDelivery] {
        try checkActive()
        return try database.rows("SELECT id,payload FROM inbox WHERE done=0 ORDER BY rowid LIMIT 16") { row in
            guard let id = UUID(uuidString: try ReplicaDatabase.text(row, 0)) else { throw WireError.storage }
            return StoredDelivery(id: id, payload: try ReplicaDatabase.data(row, 1))
        }
    }

    private func requireInbox(_ id: UUID) throws {
        let done = try database.rows("SELECT done FROM inbox WHERE id=?", [.text(id.uuidString.lowercased())]) { sqlite3_column_int64($0, 0) }.first
        guard done == 0 else { throw WireError.conflict }
    }

    private func finishInbox(_ id: UUID) throws {
        try database.execute("UPDATE inbox SET done=1,payload=NULL WHERE id=?", [.text(id.uuidString.lowercased())])
    }

    func apply(_ batch: WireSyncBatch, messageID: UUID, replies: [EncryptedEnvelope], host: DevicePublicKeys) throws {
        try database.transaction {
            try checkActive(host: host)
            try requireInbox(messageID)
            guard let current = try cursor(batch.conversationID) else { throw WireError.identity }
            guard batch.after <= current else { throw WireError.gap }
            var through = current
            for event in batch.events {
                let encoded = try event.raw.encoded(maximum: 262_144)
                let previous = try database.rows("SELECT event_id,envelope FROM events WHERE conversation=? AND sequence=?", [.text(batch.conversationID.uuidString.lowercased()), .integer(Int64(event.sequence))]) { (try ReplicaDatabase.text($0, 0), try ReplicaDatabase.data($0, 1)) }.first
                if let previous {
                    guard previous.0 == event.id.uuidString.lowercased(), previous.1 == encoded else { throw WireError.conflict }
                } else {
                    guard through < UInt64(Int64.max), event.sequence == through + 1 else { throw WireError.gap }
                    try database.execute("INSERT INTO events(conversation,sequence,event_id,kind,envelope) VALUES (?,?,?,?,?)", [.text(batch.conversationID.uuidString.lowercased()), .integer(Int64(event.sequence)), .text(event.id.uuidString.lowercased()), .text(event.kind), .bytes(encoded)])
                    try project(event)
                }
                through = max(through, event.sequence)
            }
            try database.execute("UPDATE conversations SET cursor=? WHERE id=?", [.integer(Int64(through)), .text(batch.conversationID.uuidString.lowercased())])
            guard replies.count == (batch.hasMore ? 2 : 1) else { throw WireError.invalid }
            let key = "sync_" + batch.conversationID.uuidString.lowercased()
            try database.execute("DELETE FROM metadata WHERE key=?", [.text(key)])
            for reply in replies { try insertOutbound(reply, host: host) }
            if batch.hasMore, let request = replies.last {
                try setMetadata(key, WireValue.object(["id": .id(request.header.messageID), "expires": .unsigned(request.header.expires)]).encoded(maximum: 256))
            }
            if !batch.hasMore, try notificationFloor(batch.conversationID, cursor: through) == nil {
                try setMetadata(notificationFloorKey(batch.conversationID), Data(String(through).utf8))
            }
            try finishInbox(messageID)
        }
    }

    func claimNotifications(conversation: UUID, host: DevicePublicKeys, localID: UUID, now: UInt64) throws -> ReplicaNotificationPage {
        try database.transaction {
            try checkActive(host: host)
            guard conversation != WireID.zero, localID != WireID.zero, now <= UInt64(Int64.max),
                  try identityID() == localID, let cursor = try cursor(conversation) else { throw WireError.identity }
            guard let floor = try notificationFloor(conversation, cursor: cursor), floor < cursor else {
                return ReplicaNotificationPage(notices: [], hasMore: false)
            }
            let sql = """
                SELECT conversation,sequence,event_id,kind,envelope FROM (
                    SELECT conversation,sequence,event_id,kind,envelope,
                        sum(length(CAST(envelope AS BLOB))) OVER (ORDER BY sequence) AS bytes
                    FROM (
                        SELECT conversation,sequence,event_id,kind,envelope FROM events
                        WHERE conversation=? AND sequence>? AND sequence<=?
                        ORDER BY sequence LIMIT 256
                    )
                ) WHERE bytes<=2097152 ORDER BY sequence
                """
            let conversationID = conversation.uuidString.lowercased()
            var through = floor
            var bytes = 0
            var seen = Set<UUID>()
            var completion = false
            var attention = false
            let count = try database.rows(sql, [.text(conversationID), .integer(Int64(floor)), .integer(Int64(cursor))]) { row in
                guard sqlite3_column_type(row, 0) == SQLITE_TEXT, sqlite3_column_bytes(row, 0) == 36,
                      sqlite3_column_type(row, 1) == SQLITE_INTEGER,
                      sqlite3_column_type(row, 2) == SQLITE_TEXT, sqlite3_column_bytes(row, 2) == 36,
                      sqlite3_column_type(row, 3) == SQLITE_TEXT, sqlite3_column_bytes(row, 3) <= 64,
                      sqlite3_column_type(row, 4) == SQLITE_BLOB else { throw WireError.storage }
                let sequence = sqlite3_column_int64(row, 1)
                guard through < UInt64(Int64.max), sequence > 0, UInt64(sequence) == through + 1 else { throw WireError.gap }
                let length = Int(sqlite3_column_bytes(row, 4))
                guard length > 0, length <= 262_144, bytes <= 2_097_152 - length else { throw WireError.size }
                let encoded = try ReplicaDatabase.data(row, 4)
                let event = try WireEvent(WireValue.parse(encoded, maximum: 262_144))
                let kind = try ReplicaDatabase.text(row, 3)
                guard event.conversationID == conversation, event.sequence == UInt64(sequence),
                      try ReplicaDatabase.text(row, 0) == conversationID,
                      try ReplicaDatabase.text(row, 2) == event.id.uuidString.lowercased(),
                      kind == event.kind, kind.utf8.count == Int(sqlite3_column_bytes(row, 3)),
                      seen.insert(event.id).inserted else { throw WireError.conflict }
                if event.kind == "task_status_changed", try event.payload.field("status").text == "completed" { completion = true }
                if event.kind == "permission_requested", try notificationPermissionIsPending(event, now: now) { attention = true }
                through = event.sequence
                bytes += encoded.count
            }.count
            guard count > 0, count <= 256, through <= cursor else { throw WireError.gap }
            var notices: [ReplicaNotificationNotice] = []
            for category in [ReplicaNotificationCategory.completion, .attention] {
                guard category == .completion ? completion : attention else { continue }
                let identifier = notificationIdentifier(category, conversation: conversation, through: through, host: host, localID: localID)
                notices.append(ReplicaNotificationNotice(category: category, identifier: identifier))
            }
            try setMetadata(notificationFloorKey(conversation), Data(String(through).utf8))
            return ReplicaNotificationPage(notices: notices, hasMore: through < cursor)
        }
    }

    private func notificationFloorKey(_ conversation: UUID) -> String {
        "notifications_v1_" + conversation.uuidString.lowercased()
    }

    private func notificationFloor(_ conversation: UUID, cursor: UInt64) throws -> UInt64? {
        guard let conversationCount = try database.rows("SELECT count(*) FROM conversations", map: { sqlite3_column_int64($0, 0) }).first,
              conversationCount >= 0, conversationCount <= 64 else { throw WireError.storage }
        let sql = """
            SELECT m.key,m.value,c.id,c.cursor FROM metadata m
            LEFT JOIN conversations c ON m.key='notifications_v1_'||c.id
            WHERE m.key GLOB 'notifications_v1_*' ORDER BY m.key LIMIT 65
            """
        let floors = try database.rows(sql) { row in
            guard sqlite3_column_type(row, 0) == SQLITE_TEXT, sqlite3_column_bytes(row, 0) == 53,
                  sqlite3_column_type(row, 1) == SQLITE_BLOB, (1...19).contains(Int(sqlite3_column_bytes(row, 1))),
                  sqlite3_column_type(row, 2) == SQLITE_TEXT, sqlite3_column_bytes(row, 2) == 36,
                  sqlite3_column_type(row, 3) == SQLITE_INTEGER,
                  let id = UUID(uuidString: try ReplicaDatabase.text(row, 2)), id != WireID.zero,
                  try ReplicaDatabase.text(row, 2) == id.uuidString.lowercased(),
                  try ReplicaDatabase.text(row, 0) == notificationFloorKey(id),
                  let text = String(data: try ReplicaDatabase.data(row, 1), encoding: .utf8),
                  let value = UInt64(text), String(value) == text,
                  value <= UInt64(Int64.max) else { throw WireError.storage }
            let durableCursor = sqlite3_column_int64(row, 3)
            guard durableCursor >= 0, value <= UInt64(durableCursor) else { throw WireError.storage }
            if id == conversation, UInt64(durableCursor) != cursor { throw WireError.conflict }
            return (id, value)
        }
        guard floors.count <= 64, floors.count <= Int(conversationCount) else { throw WireError.storage }
        return floors.first { $0.0 == conversation }?.1
    }

    private func notificationPermissionIsPending(_ event: WireEvent, now: UInt64) throws -> Bool {
        let payload = event.payload
        let operation = try payload.field("operation_id").identifier.uuidString.lowercased()
        let conversation = event.conversationID.uuidString.lowercased()
        let task = try payload.field("task_id").identifier.uuidString.lowercased()
        let call = try payload.field("tool_call_id").identifier.uuidString.lowercased()
        let tool = try payload.field("tool_name").text
        let current = try database.rows("SELECT conversation,task,tool,call,decided,inspection FROM approvals WHERE id=?", [.text(operation)]) { row in
            for index: Int32 in [0, 1, 3] {
                guard sqlite3_column_type(row, index) == SQLITE_TEXT, sqlite3_column_bytes(row, index) == 36 else { throw WireError.storage }
            }
            guard sqlite3_column_type(row, 2) == SQLITE_TEXT, sqlite3_column_bytes(row, 2) <= 262_144,
                  sqlite3_column_type(row, 4) == SQLITE_INTEGER else { throw WireError.storage }
            let decided = sqlite3_column_int64(row, 4)
            guard decided == 0 || decided == 1 else { throw WireError.storage }
            let inspection: WirePermissionInspection?
            if sqlite3_column_type(row, 5) == SQLITE_NULL { inspection = nil }
            else {
                guard sqlite3_column_type(row, 5) == SQLITE_BLOB, sqlite3_column_bytes(row, 5) <= 32_768 else { throw WireError.storage }
                let parsed = try WirePermissionInspection(WireValue.parse(ReplicaDatabase.data(row, 5), maximum: 32_768))
                try parsed.verifyArguments()
                inspection = parsed
            }
            guard try ReplicaDatabase.text(row, 0) == conversation, try ReplicaDatabase.text(row, 1) == task,
                  try ReplicaDatabase.text(row, 2) == tool, tool.utf8.count == Int(sqlite3_column_bytes(row, 2)),
                  try ReplicaDatabase.text(row, 3) == call else { throw WireError.conflict }
            return (decided, inspection)
        }.first
        guard let current else { throw WireError.conflict }
        if let original = try payload.fields["inspection"], original != .null {
            guard current.1?.raw == original else { throw WireError.conflict }
        }
        guard current.0 == 0, try database.permissionDecision(operation: operation, conversation: conversation) == nil else { return false }
        if let inspection = current.1 {
            return inspection.requestedAt <= now && now < inspection.expiresAt
        }
        return true
    }

    private func notificationIdentifier(_ category: ReplicaNotificationCategory, conversation: UUID, through: UInt64,
                                        host: DevicePublicKeys, localID: UUID) -> String {
        var identity = Data("everything.replica.notification.v1\0".utf8)
        identity.append(WireID.bytes(localID))
        identity.append(host.canonical)
        identity.append(WireID.bytes(conversation))
        identity.append(Data(String(through).utf8))
        identity.append(0)
        identity.append(Data(category.rawValue.utf8))
        return SHA256.hash(data: identity).map { String(format: "%02x", $0) }.joined()
    }

    func rejectGap(_ messageID: UUID, conversation: UUID, request: EncryptedEnvelope, host: DevicePublicKeys) throws {
        try database.transaction {
            try checkActive(host: host)
            try requireInbox(messageID)
            try insertOutbound(request, host: host)
            try setMetadata("sync_" + conversation.uuidString.lowercased(), WireValue.object(["id": .id(request.header.messageID), "expires": .unsigned(request.header.expires)]).encoded(maximum: 256))
            try finishInbox(messageID)
        }
    }

    func completeResult(_ messageID: UUID, commandID: UUID, state: String, result: WireValue?, host: DevicePublicKeys) throws {
        guard ["completed", "rejected"].contains(state) else { throw WireError.invalid }
        try database.transaction {
            try checkActive(host: host)
            try requireInbox(messageID)
            let encoded = try (result ?? .null).encoded()
            let prior = try database.rows("SELECT state,result FROM commands WHERE id=?", [.text(commandID.uuidString.lowercased())]) { (try ReplicaDatabase.text($0, 0), sqlite3_column_type($0, 1) == SQLITE_NULL ? nil : try ReplicaDatabase.data($0, 1)) }.first
            guard let prior else { throw WireError.conflict }
            if ["completed", "rejected"].contains(prior.0) {
                guard prior.0 == state else { throw WireError.conflict }
                if let previous = prior.1, previous != Data("null".utf8), encoded != Data("null".utf8), previous != encoded { throw WireError.conflict }
                if prior.1 == Data("null".utf8), encoded != Data("null".utf8) {
                    try database.execute("UPDATE commands SET result=? WHERE id=?", [.bytes(encoded), .text(commandID.uuidString.lowercased())])
                }
            } else {
                try database.execute("UPDATE commands SET state=?,result=? WHERE id=?", [.text(state), .bytes(encoded), .text(commandID.uuidString.lowercased())])
            }
            try finishInbox(messageID)
        }
    }

    func disable(messageID: UUID? = nil, host: DevicePublicKeys? = nil) throws {
        try database.transaction {
            if let host { try checkActive(host: host) }
            if let messageID { try requireInbox(messageID) }
            try setMetadata("revoked", Data([1]))
            try database.execute("DELETE FROM outbox")
            if let messageID { try finishInbox(messageID) }
        }
    }

    private func project(_ event: WireEvent) throws {
        let payload = event.payload
        let conversation = event.conversationID.uuidString.lowercased()
        switch event.kind {
        case "conversation_created":
            if let title = try payload.fields["title"], title != .null {
                let text = try title.text
                try database.execute("UPDATE conversations SET title=? WHERE id=?", [.text(String(text.prefix(256))), .text(conversation)])
            }
        case "task_created":
            let task = try payload.field("task")
            try database.execute("INSERT INTO tasks(id,conversation,instruction,status) VALUES (?,?,?,?)", [.text(task.field("id").identifier.uuidString.lowercased()), .text(conversation), .text(task.field("instruction").text), .text(task.field("status").text)])
        case "task_status_changed":
            try database.execute("UPDATE tasks SET status=? WHERE id=? AND conversation=?", [.text(payload.field("status").text), .text(payload.field("task_id").identifier.uuidString.lowercased()), .text(conversation)])
        case "tool_requested":
            let call = try payload.field("call")
            try database.execute("INSERT INTO calls(id,arguments) VALUES (?,?)", [.text(call.field("id").identifier.uuidString.lowercased()), .bytes(call.field("arguments").encoded(maximum: 262_144))])
        case "permission_requested":
            let operation = try payload.field("operation_id").identifier.uuidString.lowercased()
            let task = try payload.field("task_id").identifier.uuidString.lowercased()
            let tool = try payload.field("tool_name").text
            let call = try payload.field("tool_call_id").identifier.uuidString.lowercased()
            let rawInspection = try payload.fields["inspection"] ?? .null
            let inspection: WirePermissionInspection?
            if rawInspection == .null { inspection = nil }
            else {
                let parsed = try WirePermissionInspection(rawInspection)
                try parsed.verifyArguments()
                inspection = parsed
            }
            let encoded = try inspection?.raw.encoded(maximum: 32_768)
            let decided = try database.permissionDecision(operation: operation, conversation: conversation) != nil
            let existing = try database.rows("SELECT conversation,task,tool,call,inspection FROM approvals WHERE id=?", [.text(operation)]) { row in
                (try ReplicaDatabase.text(row, 0), try ReplicaDatabase.text(row, 1), try ReplicaDatabase.text(row, 2), try ReplicaDatabase.text(row, 3), sqlite3_column_type(row, 4) == SQLITE_NULL ? nil : try ReplicaDatabase.data(row, 4))
            }.first
            if let existing {
                guard existing.0 == conversation, existing.1 == task, existing.2 == tool, existing.3 == call else { throw WireError.conflict }
                if let previous = existing.4, let encoded { guard previous == encoded else { throw WireError.conflict } }
                if existing.4 == nil, let encoded { try database.execute("UPDATE approvals SET inspection=? WHERE id=?", [.bytes(encoded), .text(operation)]) }
                if decided { try database.execute("UPDATE approvals SET decided=1 WHERE id=? AND conversation=?", [.text(operation), .text(conversation)]) }
            } else {
                try database.execute("INSERT INTO approvals(id,conversation,task,tool,call,decided,inspection) VALUES (?,?,?,?,?,?,?)", [.text(operation), .text(conversation), .text(task), .text(tool), .text(call), .integer(decided ? 1 : 0), encoded.map(SQLValue.bytes) ?? .null])
            }
        case "permission_decided":
            try database.retainPermissionDecision(event)
        default: break
        }
    }

    func synchronizationConversations() throws -> [UUID] {
        try checkActive()
        return try database.rows("SELECT id FROM conversations ORDER BY id LIMIT 64") { row in
            guard let id = UUID(uuidString: try ReplicaDatabase.text(row, 0)), id != WireID.zero else { throw WireError.storage }
            return id
        }
    }

    func events(conversation: UUID, before: UInt64? = nil) throws -> [WireEvent] {
        let bound = before ?? UInt64(Int64.max)
        let rows = try database.rows("SELECT envelope FROM (SELECT envelope,sequence,sum(length(envelope)) OVER (ORDER BY sequence DESC) AS bytes FROM events WHERE conversation=? AND sequence<?) WHERE bytes<=2097152 ORDER BY sequence DESC LIMIT 100", [.text(conversation.uuidString.lowercased()), .integer(Int64(bound))]) { try ReplicaDatabase.data($0, 0) }
        var bytes = 0
        var result: [WireEvent] = []
        for row in rows {
            guard bytes + row.count <= 2_097_152 else { break }
            result.append(try WireEvent(WireValue.parse(row, maximum: 262_144)))
            bytes += row.count
        }
        return result.reversed()
    }

    private func decodeApproval(_ row: OpaquePointer) throws -> ReplicaApproval {
        guard let id = UUID(uuidString: try ReplicaDatabase.text(row, 0)),
              let conversation = UUID(uuidString: try ReplicaDatabase.text(row, 1)),
              let task = UUID(uuidString: try ReplicaDatabase.text(row, 2)),
              let call = UUID(uuidString: try ReplicaDatabase.text(row, 4)) else { throw WireError.storage }
        let inspection: WirePermissionInspection?
        if sqlite3_column_type(row, 6) == SQLITE_NULL { inspection = nil }
        else {
            let parsed = try WirePermissionInspection(WireValue.parse(ReplicaDatabase.data(row, 6), maximum: 32_768))
            try parsed.verifyArguments()
            inspection = parsed
        }
        let arguments = sqlite3_column_type(row, 5) == SQLITE_NULL ? nil : String(data: try ReplicaDatabase.data(row, 5), encoding: .utf8)
        return ReplicaApproval(id: id, conversationID: conversation, taskID: task, tool: try ReplicaDatabase.text(row, 3), callID: call, arguments: arguments, inspection: inspection)
    }

    private func requireCurrent(_ selection: ReplicaApproval, allow: Bool, now: UInt64) throws {
        let current = try database.rows("SELECT a.id,a.conversation,a.task,a.tool,a.call,c.arguments,a.inspection FROM approvals a LEFT JOIN calls c ON c.id=a.call WHERE a.id=? AND a.decided=0", [.text(selection.id.uuidString.lowercased())], map: decodeApproval).first
        guard current == selection else { throw WireError.conflict }
        if allow {
            guard selection.id == selection.callID, let inspection = selection.inspection,
                  inspection.requestedAt <= now, now < inspection.expiresAt else { throw WireError.expired }
            try inspection.verifyArguments()
        }
    }

    func snapshot() throws -> ReplicaSnapshot {
        let conversations = try database.rows("SELECT id,title,cursor FROM conversations ORDER BY title,id LIMIT 64") { row in
            guard let id = UUID(uuidString: try ReplicaDatabase.text(row, 0)) else { throw WireError.storage }
            return ReplicaConversation(id: id, title: try ReplicaDatabase.text(row, 1), cursor: UInt64(sqlite3_column_int64(row, 2)))
        }
        let tasks = try database.rows("SELECT id,conversation,instruction,status FROM (SELECT id,conversation,instruction,status,rowid,sum(length(instruction)+length(status)+108) OVER (ORDER BY rowid DESC) AS bytes FROM tasks WHERE status NOT IN ('completed','failed','cancelled')) WHERE bytes<=2097152 ORDER BY rowid DESC LIMIT 100") { row in
            guard let id = UUID(uuidString: try ReplicaDatabase.text(row, 0)), let conversation = UUID(uuidString: try ReplicaDatabase.text(row, 1)) else { throw WireError.storage }
            return ReplicaTask(id: id, conversationID: conversation, instruction: try ReplicaDatabase.text(row, 2), status: try ReplicaDatabase.text(row, 3))
        }
        let approvals = try database.rows("SELECT id,conversation,task,tool,call,arguments,inspection FROM (SELECT a.id,a.conversation,a.task,a.tool,a.call,c.arguments,a.inspection,a.rowid,sum(coalesce(length(c.arguments),0)+coalesce(length(a.inspection),0)+length(a.tool)+144) OVER (ORDER BY a.rowid DESC) AS bytes FROM approvals a LEFT JOIN calls c ON c.id=a.call WHERE a.decided=0) WHERE bytes<=2097152 ORDER BY rowid DESC LIMIT 100", map: decodeApproval)
        let commands = try database.rows("SELECT id,conversation,original,state,result FROM (SELECT id,conversation,original,state,result,rowid,sum(length(original)+coalesce(length(result),0)+length(state)+72) OVER (ORDER BY rowid DESC) AS bytes FROM commands) WHERE bytes<=2097152 ORDER BY rowid DESC LIMIT 100") { row in
            guard let id = UUID(uuidString: try ReplicaDatabase.text(row, 0)), let conversation = UUID(uuidString: try ReplicaDatabase.text(row, 1)) else { throw WireError.storage }
            return ReplicaPendingCommand(id: id, conversationID: conversation, original: try WireValue.parse(ReplicaDatabase.data(row, 2), maximum: 262_144), state: try ReplicaDatabase.text(row, 3), result: sqlite3_column_type(row, 4) == SQLITE_NULL ? nil : try WireValue.parse(ReplicaDatabase.data(row, 4)))
        }
        return ReplicaSnapshot(host: try host(), origin: try metadata("origin").flatMap { String(data: $0, encoding: .utf8) }, revoked: try metadata("revoked") != nil, conversations: conversations, tasks: tasks, approvals: approvals, commands: commands)
    }
}

extension ReplicaStore {
    func queueSync(_ envelope: EncryptedEnvelope, conversation: UUID, host: DevicePublicKeys, now: UInt64) throws {
        try database.transaction {
            try checkActive(host: host)
            let key = "sync_" + conversation.uuidString.lowercased()
            if let saved = try metadata(key), try WireValue.parse(saved, maximum: 256).field("expires").integer > now { return }
            try insertOutbound(envelope, host: host)
            try setMetadata(key, WireValue.object(["id": .id(envelope.header.messageID), "expires": .unsigned(envelope.header.expires)]).encoded(maximum: 256))
        }
    }
}

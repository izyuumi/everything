import Foundation
import SQLite3

private enum PreferenceSQLValue {
    case integer(Int64)
    case text(String)
    case bytes(Data)
}

private final class PreferenceDatabase: @unchecked Sendable {
    let handle: OpaquePointer
    private let transient = unsafeBitCast(-1, to: sqlite3_destructor_type.self)
    private var usable = true

    init(url: URL) throws {
        var opened: OpaquePointer?
        let result = sqlite3_open_v2(url.path, &opened, SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE | SQLITE_OPEN_FULLMUTEX, nil)
        guard result == SQLITE_OK, let opened else {
            if let opened { sqlite3_close(opened) }
            throw DevicePreferenceError.storage
        }
        handle = opened
        guard sqlite3_busy_timeout(handle, 250) == SQLITE_OK else { throw DevicePreferenceError.storage }
    }

    deinit { sqlite3_close(handle) }

    @discardableResult
    func rows<T>(_ sql: String, _ values: [PreferenceSQLValue] = [], map: (OpaquePointer) throws -> T) throws -> [T] {
        guard usable else { throw DevicePreferenceError.uncertain }
        var prepared: OpaquePointer?
        try check(sqlite3_prepare_v2(handle, sql, -1, &prepared, nil))
        guard let prepared else { throw DevicePreferenceError.storage }
        defer { sqlite3_finalize(prepared) }
        for (index, value) in values.enumerated() {
            let offset = Int32(index + 1)
            switch value {
            case .integer(let value): try check(sqlite3_bind_int64(prepared, offset, value))
            case .text(let value): try check(sqlite3_bind_text(prepared, offset, value, -1, transient))
            case .bytes(let value):
                try check(value.withUnsafeBytes { sqlite3_bind_blob(prepared, offset, $0.baseAddress, Int32(value.count), transient) })
            }
        }
        var rows: [T] = []
        while true {
            switch sqlite3_step(prepared) {
            case SQLITE_ROW: rows.append(try map(prepared))
            case SQLITE_DONE: return rows
            case let code: try check(code); throw DevicePreferenceError.storage
            }
        }
    }

    func execute(_ sql: String, _ values: [PreferenceSQLValue] = []) throws {
        try rows(sql, values) { _ in () }
    }

    func transaction<T>(_ operation: () throws -> T) throws -> T {
        try execute("BEGIN IMMEDIATE")
        do {
            let value = try operation()
            try execute("COMMIT")
            return value
        } catch {
            let original = error
            do { try execute("ROLLBACK") }
            catch {
                usable = false
                throw DevicePreferenceError.uncertain
            }
            throw original
        }
    }

    static func text(_ row: OpaquePointer, _ column: Int32, maximumBytes: Int) throws -> String {
        guard sqlite3_column_type(row, column) == SQLITE_TEXT,
              let value = sqlite3_column_text(row, column) else { throw DevicePreferenceError.storage }
        let count = Int(sqlite3_column_bytes(row, column))
        guard count > 0, count <= maximumBytes else { throw DevicePreferenceError.storage }
        let bytes = UnsafeBufferPointer(start: value, count: count)
        guard !bytes.contains(0), let decoded = String(bytes: bytes, encoding: .utf8) else {
            throw DevicePreferenceError.storage
        }
        return decoded
    }

    static func data(_ row: OpaquePointer, _ column: Int32) throws -> Data {
        let count = Int(sqlite3_column_bytes(row, column))
        guard sqlite3_column_type(row, column) == SQLITE_BLOB, count > 0, count <= 16 * 1024,
              let bytes = sqlite3_column_blob(row, column) else { throw DevicePreferenceError.storage }
        return Data(bytes: bytes, count: count)
    }

    private func check(_ code: Int32) throws {
        guard code != SQLITE_OK, code != SQLITE_ROW, code != SQLITE_DONE else { return }
        if code == SQLITE_FULL { throw DevicePreferenceError.capacity }
        throw DevicePreferenceError.storage
    }
}

actor DevicePreferenceStore {
    private let database: PreferenceDatabase
    private let deviceID: UUID
    private static let maximumRequests = 8_192

    init(directory: URL, deviceID: UUID) throws {
        let zero = UUID(uuid: (0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0))
        guard directory.isFileURL, deviceID != zero else { throw DevicePreferenceError.invalidRequest }
        self.deviceID = deviceID
        let manager = FileManager.default
        var attributes: [FileAttributeKey: Any] = [.posixPermissions: 0o700]
        #if os(iOS)
        attributes[.protectionKey] = FileProtectionType.complete
        #endif
        try manager.createDirectory(at: directory, withIntermediateDirectories: true, attributes: attributes)
        try manager.setAttributes(attributes, ofItemAtPath: directory.path)
        var values = URLResourceValues()
        values.isExcludedFromBackup = true
        var excluded = directory
        try excluded.setResourceValues(values)
        let url = directory.appendingPathComponent("device-preferences-v1.sqlite3")
        database = try PreferenceDatabase(url: url)
        let version = try database.rows("PRAGMA user_version") { sqlite3_column_int64($0, 0) }.first ?? 0
        guard version == 0 || version == 1 else { throw DevicePreferenceError.unsupportedVersion }
        for sql in [
            "PRAGMA page_size=4096",
            "PRAGMA max_page_count=2048",
            "PRAGMA journal_mode=WAL",
            "PRAGMA wal_autocheckpoint=128",
            "PRAGMA synchronous=FULL",
            "CREATE TABLE IF NOT EXISTS preferences(id INTEGER PRIMARY KEY CHECK(id=1),device TEXT NOT NULL,revision INTEGER NOT NULL CHECK(revision>0),completion INTEGER NOT NULL CHECK(completion IN (0,1)),attention INTEGER NOT NULL CHECK(attention IN (0,1)))",
            "CREATE TABLE IF NOT EXISTS requests(id TEXT PRIMARY KEY,original BLOB NOT NULL,outcome TEXT NOT NULL CHECK(outcome IN ('applied','conflict')),revision INTEGER NOT NULL CHECK(revision>0))"
        ] { try database.execute(sql) }
        try database.transaction {
            let identities = try database.rows("SELECT device FROM preferences WHERE id=1") { try PreferenceDatabase.text($0, 0, maximumBytes: 36) }
            if let identity = identities.first {
                guard identity == deviceID.uuidString.lowercased() else { throw DevicePreferenceError.wrongDevice }
            } else {
                try database.execute("INSERT INTO preferences(id,device,revision,completion,attention) VALUES(1,?,1,0,0)", [.text(deviceID.uuidString.lowercased())])
            }
            try database.execute("PRAGMA user_version=1")
        }
        for name in [url.lastPathComponent, url.lastPathComponent + "-wal", url.lastPathComponent + "-shm"] {
            let path = directory.appendingPathComponent(name).path
            if manager.fileExists(atPath: path) {
                var attributes: [FileAttributeKey: Any] = [.posixPermissions: 0o600]
                #if os(iOS)
                attributes[.protectionKey] = FileProtectionType.complete
                #endif
                try manager.setAttributes(attributes, ofItemAtPath: path)
            }
        }
    }

    func snapshot() throws -> DevicePreferenceSnapshot {
        try readSnapshot()
    }

    func result(for requestID: UUID) throws -> DevicePreferenceResult? {
        try storedRequest(requestID)?.result
    }

    func apply(_ patch: DevicePreferencePatch) throws -> DevicePreferenceResult {
        let original = try patch.encodedIdentity()
        guard patch.deviceID == deviceID else { throw DevicePreferenceError.wrongDevice }
        return try database.transaction {
            if let previous = try storedRequest(patch.requestID) {
                guard previous.original == original else { throw DevicePreferenceError.requestReused }
                return previous.result
            }
            let count = try database.rows("SELECT COUNT(*) FROM requests") { sqlite3_column_int64($0, 0) }.first ?? 0
            guard count < Self.maximumRequests else { throw DevicePreferenceError.capacity }
            let current = try readSnapshot()
            let outcome: DevicePreferenceResult.Outcome
            let revision: UInt64
            let label: String
            if current.revision != patch.expectedRevision {
                revision = current.revision
                label = "conflict"
                outcome = .conflict(observedRevision: revision)
            } else {
                guard current.revision < UInt64(Int64.max) else { throw DevicePreferenceError.capacity }
                var completion = current.completion
                var attention = current.attention
                for change in patch.changes {
                    switch change.key {
                    case .completion: completion = change.value
                    case .attention: attention = change.value
                    }
                }
                revision = current.revision + 1
                label = "applied"
                outcome = .applied(revision: revision)
                try database.execute("UPDATE preferences SET revision=?,completion=?,attention=? WHERE id=1 AND revision=?", [.integer(Int64(revision)), .integer(completion ? 1 : 0), .integer(attention ? 1 : 0), .integer(Int64(current.revision))])
                guard sqlite3_changes(database.handle) == 1 else { throw DevicePreferenceError.storage }
            }
            try database.execute("INSERT INTO requests(id,original,outcome,revision) VALUES(?,?,?,?)", [.text(patch.requestID.uuidString.lowercased()), .bytes(original), .text(label), .integer(Int64(revision))])
            return DevicePreferenceResult(requestID: patch.requestID, deviceID: deviceID, outcome: outcome)
        }
    }

    private func readSnapshot() throws -> DevicePreferenceSnapshot {
        let snapshots = try database.rows("SELECT device,revision,completion,attention FROM preferences WHERE id=1") { row in
            let revision = sqlite3_column_int64(row, 1)
            let completion = sqlite3_column_int64(row, 2)
            let attention = sqlite3_column_int64(row, 3)
            guard try PreferenceDatabase.text(row, 0, maximumBytes: 36) == deviceID.uuidString.lowercased(), revision > 0,
                  sqlite3_column_type(row, 1) == SQLITE_INTEGER,
                  sqlite3_column_type(row, 2) == SQLITE_INTEGER,
                  sqlite3_column_type(row, 3) == SQLITE_INTEGER,
                  [0, 1].contains(completion), [0, 1].contains(attention) else { throw DevicePreferenceError.storage }
            return DevicePreferenceSnapshot(deviceID: deviceID, revision: UInt64(revision), completion: completion == 1, attention: attention == 1)
        }
        guard let snapshot = snapshots.first else { throw DevicePreferenceError.storage }
        return snapshot
    }

    private func storedRequest(_ requestID: UUID) throws -> (original: Data, result: DevicePreferenceResult)? {
        try database.rows("SELECT original,outcome,revision FROM requests WHERE id=?", [.text(requestID.uuidString.lowercased())]) { row in
            let revision = sqlite3_column_int64(row, 2)
            guard sqlite3_column_type(row, 2) == SQLITE_INTEGER, revision > 0 else { throw DevicePreferenceError.storage }
            let outcome: DevicePreferenceResult.Outcome
            switch try PreferenceDatabase.text(row, 1, maximumBytes: 8) {
            case "applied": outcome = .applied(revision: UInt64(revision))
            case "conflict": outcome = .conflict(observedRevision: UInt64(revision))
            default: throw DevicePreferenceError.storage
            }
            return (original: try PreferenceDatabase.data(row, 0), result: DevicePreferenceResult(requestID: requestID, deviceID: deviceID, outcome: outcome))
        }.first
    }
}

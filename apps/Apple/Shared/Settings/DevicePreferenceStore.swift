import Foundation
import SQLite3

private enum PreferenceSQLValue {
    case integer(Int64)
    case text(String)
    case bytes(Data)
}

private enum PreferenceIdentity {
    case bound(UUID)
    case local
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

private enum PreferenceRequestKind: String {
    case notifications
    case appearance
}

private enum PreferenceMutation {
    case notifications(DevicePreferencePatch)
    case appearance(DeviceAppearancePatch)

    var kind: PreferenceRequestKind {
        switch self {
        case .notifications: .notifications
        case .appearance: .appearance
        }
    }

    var requestID: UUID {
        switch self {
        case .notifications(let patch): patch.requestID
        case .appearance(let patch): patch.requestID
        }
    }

    var deviceID: UUID {
        switch self {
        case .notifications(let patch): patch.deviceID
        case .appearance(let patch): patch.deviceID
        }
    }

    var expectedRevision: UInt64 {
        switch self {
        case .notifications(let patch): patch.expectedRevision
        case .appearance(let patch): patch.expectedRevision
        }
    }

    func encodedIdentity() throws -> Data {
        switch self {
        case .notifications(let patch): try patch.encodedIdentity()
        case .appearance(let patch): try patch.encodedIdentity()
        }
    }
}

private struct PreferenceState {
    let deviceID: UUID
    let revision: UInt64
    let completion: Bool
    let attention: Bool
    let privacyCover: Bool
    let appearance: DeviceAppearance

    var notifications: DevicePreferenceSnapshot {
        DevicePreferenceSnapshot(deviceID: deviceID, revision: revision, completion: completion, attention: attention, privacyCover: privacyCover)
    }

    var appearanceSnapshot: DeviceAppearanceSnapshot {
        DeviceAppearanceSnapshot(deviceID: deviceID, revision: revision, appearance: appearance)
    }
}

private struct PreferenceColumn: Equatable {
    let name: String
    let type: String
    let required: Bool
    let defaultValue: String?
    let primaryKey: Bool
}

private struct StoredPreferenceRequest {
    let kind: PreferenceRequestKind
    let original: Data
    let result: DevicePreferenceResult
}

private struct PreferenceRequestIdentity: Decodable {
    struct Scope: Decodable {
        let kind: String
        let deviceID: UUID

        enum CodingKeys: String, CodingKey {
            case kind
            case deviceID = "device_id"
        }
    }

    let kind: String?
    let contractVersion: UInt16
    let requestID: UUID
    let scope: Scope
    let expectedRevision: UInt64
    let changes: [DevicePreferenceChange]?
    let appearance: DeviceAppearance?

    enum CodingKeys: String, CodingKey {
        case kind
        case contractVersion = "contract_version"
        case requestID = "request_id"
        case scope
        case expectedRevision = "expected_revision"
        case changes
        case appearance
    }

    func mutation(for kind: PreferenceRequestKind) throws -> PreferenceMutation {
        guard contractVersion == 1, scope.kind == "device_preferences" else { throw DevicePreferenceError.storage }
        switch kind {
        case .notifications:
            guard self.kind == nil, appearance == nil, let changes else { throw DevicePreferenceError.storage }
            return .notifications(DevicePreferencePatch(requestID: requestID, deviceID: scope.deviceID,
                expectedRevision: expectedRevision, changes: changes))
        case .appearance:
            guard self.kind == "appearance", changes == nil, let appearance else { throw DevicePreferenceError.storage }
            return .appearance(DeviceAppearancePatch(requestID: requestID, deviceID: scope.deviceID,
                expectedRevision: expectedRevision, appearance: appearance))
        }
    }
}

actor DevicePreferenceStore {
    private let database: PreferenceDatabase
    private let deviceID: UUID
    private static let maximumRequests = 8_192
    private static let zeroID = UUID(uuid: (0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0))

    init(directory: URL, deviceID: UUID) throws {
        let opened = try Self.open(directory: directory, identity: .bound(deviceID))
        database = opened.database
        self.deviceID = opened.deviceID
    }

    static func local(directory: URL) throws -> DevicePreferenceStore {
        try DevicePreferenceStore(localDirectory: directory)
    }

    private init(localDirectory directory: URL) throws {
        let opened = try Self.open(directory: directory, identity: .local)
        database = opened.database
        deviceID = opened.deviceID
    }

    private static func open(directory: URL, identity: PreferenceIdentity) throws -> (database: PreferenceDatabase, deviceID: UUID) {
        guard directory.isFileURL else { throw DevicePreferenceError.invalidRequest }
        if case .bound(let deviceID) = identity, deviceID == zeroID { throw DevicePreferenceError.invalidRequest }
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
        let database = try PreferenceDatabase(url: url)
        try validateVersion(database)
        for sql in [
            "PRAGMA page_size=4096",
            "PRAGMA max_page_count=2048",
            "PRAGMA journal_mode=WAL",
            "PRAGMA wal_autocheckpoint=128",
            "PRAGMA synchronous=FULL"
        ] { try database.execute(sql) }
        guard try integer(database, "PRAGMA page_size") == 4096,
              try integer(database, "PRAGMA max_page_count") <= 2048 else { throw DevicePreferenceError.capacity }
        let deviceID = try database.transaction {
            try validateVersion(database)
            let version = try integer(database, "PRAGMA user_version")
            if version == 0 {
                let count = try integer(database, "SELECT COUNT(*) FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*'")
                guard count == 0 else { throw DevicePreferenceError.storage }
                try createSchema(database)
                let deviceID: UUID
                switch identity {
                case .bound(let expected): deviceID = expected
                case .local: deviceID = UUID()
                }
                guard deviceID != zeroID else { throw DevicePreferenceError.storage }
                try database.execute("INSERT INTO preferences(id,device,revision,completion,attention,appearance,privacy) VALUES(1,?,1,0,0,'system',0)", [.text(deviceID.uuidString.lowercased())])
                try database.execute("PRAGMA user_version=3")
                try validateSchema(database, version: 3)
                _ = try readState(database: database, deviceID: deviceID)
                return deviceID
            }
            try validateSchema(database, version: version)
            let deviceID = try existingIdentity(database, expected: identity)
            let currentRevision: UInt64
            if version < 3 {
                currentRevision = try readLegacySnapshot(database: database, deviceID: deviceID).revision
            } else {
                currentRevision = try readState(database: database, deviceID: deviceID).revision
            }
            try validateRequests(database, deviceID: deviceID, revision: currentRevision, version: version)
            if version == 1 {
                try database.execute("ALTER TABLE preferences ADD COLUMN appearance TEXT NOT NULL DEFAULT 'system' CHECK(appearance IN ('system','light','dark'))")
                try database.execute("ALTER TABLE requests ADD COLUMN kind TEXT NOT NULL DEFAULT 'notifications' CHECK(kind IN ('notifications','appearance'))")
                try database.execute("PRAGMA user_version=2")
                try validateSchema(database, version: 2)
            }
            if version < 3 {
                try database.execute("ALTER TABLE preferences ADD COLUMN privacy INTEGER NOT NULL DEFAULT 0 CHECK(privacy IN (0,1))")
                try database.execute("PRAGMA user_version=3")
                try validateSchema(database, version: 3)
                _ = try readState(database: database, deviceID: deviceID)
            }
            return deviceID
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
        return (database: database, deviceID: deviceID)
    }

    func snapshot() throws -> DevicePreferenceSnapshot {
        try Self.readState(database: database, deviceID: deviceID).notifications
    }

    func appearanceSnapshot() throws -> DeviceAppearanceSnapshot {
        try Self.readState(database: database, deviceID: deviceID).appearanceSnapshot
    }

    func result(for requestID: UUID) throws -> DevicePreferenceResult? {
        try result(for: requestID, kind: .notifications)
    }

    func appearanceResult(for requestID: UUID) throws -> DevicePreferenceResult? {
        try result(for: requestID, kind: .appearance)
    }

    func apply(_ patch: DevicePreferencePatch) throws -> DevicePreferenceResult {
        try apply(.notifications(patch))
    }

    func applyAppearance(_ patch: DeviceAppearancePatch) throws -> DevicePreferenceResult {
        try apply(.appearance(patch))
    }

    private func result(for requestID: UUID, kind: PreferenceRequestKind) throws -> DevicePreferenceResult? {
        guard let previous = try storedRequest(requestID) else { return nil }
        guard previous.kind == kind else { throw DevicePreferenceError.requestReused }
        return previous.result
    }

    private func apply(_ mutation: PreferenceMutation) throws -> DevicePreferenceResult {
        let original = try mutation.encodedIdentity()
        guard mutation.deviceID == deviceID else { throw DevicePreferenceError.wrongDevice }
        return try database.transaction {
            if let previous = try storedRequest(mutation.requestID) {
                guard previous.kind == mutation.kind, previous.original == original else { throw DevicePreferenceError.requestReused }
                return previous.result
            }
            let count = try Self.integer(database, "SELECT COUNT(*) FROM requests")
            guard count < Self.maximumRequests else { throw DevicePreferenceError.capacity }
            let current = try Self.readState(database: database, deviceID: deviceID)
            let outcome: DevicePreferenceResult.Outcome
            let revision: UInt64
            let label: String
            if current.revision != mutation.expectedRevision {
                revision = current.revision
                label = "conflict"
                outcome = .conflict(observedRevision: revision)
            } else {
                guard current.revision < UInt64(Int64.max) else { throw DevicePreferenceError.capacity }
                revision = current.revision + 1
                label = "applied"
                outcome = .applied(revision: revision)
                switch mutation {
                case .notifications(let patch):
                    var completion = current.completion
                    var attention = current.attention
                    var privacyCover = current.privacyCover
                    for change in patch.changes {
                        switch change.key {
                        case .completion: completion = change.value
                        case .attention: attention = change.value
                        case .privacyCover: privacyCover = change.value
                        }
                    }
                    try database.execute("UPDATE preferences SET revision=?,completion=?,attention=?,privacy=? WHERE id=1 AND revision=?", [.integer(Int64(revision)), .integer(completion ? 1 : 0), .integer(attention ? 1 : 0), .integer(privacyCover ? 1 : 0), .integer(Int64(current.revision))])
                case .appearance(let patch):
                    try database.execute("UPDATE preferences SET revision=?,appearance=? WHERE id=1 AND revision=?", [.integer(Int64(revision)), .text(patch.appearance.rawValue), .integer(Int64(current.revision))])
                }
                guard sqlite3_changes(database.handle) == 1 else { throw DevicePreferenceError.storage }
            }
            try database.execute("INSERT INTO requests(id,original,outcome,revision,kind) VALUES(?,?,?,?,?)", [.text(mutation.requestID.uuidString.lowercased()), .bytes(original), .text(label), .integer(Int64(revision)), .text(mutation.kind.rawValue)])
            return DevicePreferenceResult(requestID: mutation.requestID, deviceID: deviceID, outcome: outcome)
        }
    }

    private func storedRequest(_ requestID: UUID) throws -> StoredPreferenceRequest? {
        try database.rows("SELECT id,original,outcome,revision,kind FROM requests WHERE id=?", [.text(requestID.uuidString.lowercased())]) { row in
            let stored = try Self.readRequest(row, deviceID: deviceID, version: 2)
            guard stored.result.requestID == requestID else { throw DevicePreferenceError.storage }
            return stored
        }.first
    }

    private static func integer(_ database: PreferenceDatabase, _ sql: String) throws -> Int64 {
        let values = try database.rows(sql) { row in
            guard sqlite3_column_type(row, 0) == SQLITE_INTEGER else { throw DevicePreferenceError.storage }
            return sqlite3_column_int64(row, 0)
        }
        guard values.count == 1, let value = values.first else { throw DevicePreferenceError.storage }
        return value
    }

    private static func validateVersion(_ database: PreferenceDatabase) throws {
        let version = try integer(database, "PRAGMA user_version")
        guard (0...3).contains(version) else { throw DevicePreferenceError.unsupportedVersion }
    }

    private static func createSchema(_ database: PreferenceDatabase) throws {
        try database.execute("CREATE TABLE preferences(id INTEGER PRIMARY KEY CHECK(id=1),device TEXT NOT NULL,revision INTEGER NOT NULL CHECK(revision>0),completion INTEGER NOT NULL CHECK(completion IN (0,1)),attention INTEGER NOT NULL CHECK(attention IN (0,1)),appearance TEXT NOT NULL DEFAULT 'system' CHECK(appearance IN ('system','light','dark')),privacy INTEGER NOT NULL DEFAULT 0 CHECK(privacy IN (0,1)))")
        try database.execute("CREATE TABLE requests(id TEXT PRIMARY KEY,original BLOB NOT NULL,outcome TEXT NOT NULL CHECK(outcome IN ('applied','conflict')),revision INTEGER NOT NULL CHECK(revision>0),kind TEXT NOT NULL DEFAULT 'notifications' CHECK(kind IN ('notifications','appearance')))")
    }

    private static func validateSchema(_ database: PreferenceDatabase, version: Int64) throws {
        let tables = try database.rows("SELECT name,type FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*' ORDER BY name LIMIT 3") { row in
            guard try PreferenceDatabase.text(row, 1, maximumBytes: 5) == "table" else { throw DevicePreferenceError.storage }
            return try PreferenceDatabase.text(row, 0, maximumBytes: 11)
        }
        guard tables == ["preferences", "requests"] else { throw DevicePreferenceError.storage }
        var preferences = [
            PreferenceColumn(name: "id", type: "INTEGER", required: false, defaultValue: nil, primaryKey: true),
            PreferenceColumn(name: "device", type: "TEXT", required: true, defaultValue: nil, primaryKey: false),
            PreferenceColumn(name: "revision", type: "INTEGER", required: true, defaultValue: nil, primaryKey: false),
            PreferenceColumn(name: "completion", type: "INTEGER", required: true, defaultValue: nil, primaryKey: false),
            PreferenceColumn(name: "attention", type: "INTEGER", required: true, defaultValue: nil, primaryKey: false)
        ]
        var requests = [
            PreferenceColumn(name: "id", type: "TEXT", required: false, defaultValue: nil, primaryKey: true),
            PreferenceColumn(name: "original", type: "BLOB", required: true, defaultValue: nil, primaryKey: false),
            PreferenceColumn(name: "outcome", type: "TEXT", required: true, defaultValue: nil, primaryKey: false),
            PreferenceColumn(name: "revision", type: "INTEGER", required: true, defaultValue: nil, primaryKey: false)
        ]
        if version >= 2 {
            preferences.append(PreferenceColumn(name: "appearance", type: "TEXT", required: true, defaultValue: "'system'", primaryKey: false))
            requests.append(PreferenceColumn(name: "kind", type: "TEXT", required: true, defaultValue: "'notifications'", primaryKey: false))
        }
        if version >= 3 {
            preferences.append(PreferenceColumn(name: "privacy", type: "INTEGER", required: true, defaultValue: "0", primaryKey: false))
        }
        for (table, expected) in [("preferences", preferences), ("requests", requests)] {
            let columns = try database.rows("PRAGMA table_xinfo(\(table))") { row in
                guard sqlite3_column_type(row, 0) == SQLITE_INTEGER, sqlite3_column_type(row, 3) == SQLITE_INTEGER,
                      sqlite3_column_type(row, 5) == SQLITE_INTEGER, sqlite3_column_type(row, 6) == SQLITE_INTEGER,
                      sqlite3_column_int64(row, 6) == 0 else { throw DevicePreferenceError.storage }
                let required = sqlite3_column_int64(row, 3)
                let primary = sqlite3_column_int64(row, 5)
                guard [0, 1].contains(required), [0, 1].contains(primary) else { throw DevicePreferenceError.storage }
                let defaultValue = sqlite3_column_type(row, 4) == SQLITE_NULL ? nil : try PreferenceDatabase.text(row, 4, maximumBytes: 15)
                return PreferenceColumn(name: try PreferenceDatabase.text(row, 1, maximumBytes: 10),
                    type: try PreferenceDatabase.text(row, 2, maximumBytes: 7), required: required == 1,
                    defaultValue: defaultValue, primaryKey: primary == 1)
            }
            guard columns == expected else { throw DevicePreferenceError.storage }
        }
    }

    private static func existingIdentity(_ database: PreferenceDatabase, expected: PreferenceIdentity) throws -> UUID {
        let values = try database.rows("SELECT id,device FROM preferences LIMIT 2") { row in
            guard sqlite3_column_type(row, 0) == SQLITE_INTEGER, sqlite3_column_int64(row, 0) == 1 else {
                throw DevicePreferenceError.storage
            }
            return try canonicalID(row, 1)
        }
        guard values.count == 1, let deviceID = values.first else { throw DevicePreferenceError.storage }
        if case .bound(let bound) = expected, bound != deviceID { throw DevicePreferenceError.wrongDevice }
        return deviceID
    }

    private static func readState(database: PreferenceDatabase, deviceID: UUID) throws -> PreferenceState {
        let snapshots = try database.rows("SELECT id,device,revision,completion,attention,appearance,privacy FROM preferences LIMIT 2") { row in
            let snapshot = try readNotificationColumns(row, deviceID: deviceID)
            let privacy = sqlite3_column_int64(row, 6)
            guard let appearance = DeviceAppearance(rawValue: try PreferenceDatabase.text(row, 5, maximumBytes: 6)),
                  sqlite3_column_type(row, 6) == SQLITE_INTEGER, [0, 1].contains(privacy) else {
                throw DevicePreferenceError.storage
            }
            return PreferenceState(deviceID: deviceID, revision: snapshot.revision, completion: snapshot.completion,
                                   attention: snapshot.attention, privacyCover: privacy == 1, appearance: appearance)
        }
        guard snapshots.count == 1, let snapshot = snapshots.first else { throw DevicePreferenceError.storage }
        return snapshot
    }

    private static func readLegacySnapshot(database: PreferenceDatabase, deviceID: UUID) throws -> DevicePreferenceSnapshot {
        let snapshots = try database.rows("SELECT id,device,revision,completion,attention FROM preferences LIMIT 2") { row in
            try readNotificationColumns(row, deviceID: deviceID)
        }
        guard snapshots.count == 1, let snapshot = snapshots.first else { throw DevicePreferenceError.storage }
        return snapshot
    }

    private static func readNotificationColumns(_ row: OpaquePointer, deviceID: UUID) throws -> DevicePreferenceSnapshot {
        let revision = sqlite3_column_int64(row, 2)
        let completion = sqlite3_column_int64(row, 3)
        let attention = sqlite3_column_int64(row, 4)
        guard sqlite3_column_type(row, 0) == SQLITE_INTEGER, sqlite3_column_int64(row, 0) == 1,
              try canonicalID(row, 1) == deviceID, revision > 0,
              sqlite3_column_type(row, 2) == SQLITE_INTEGER,
              sqlite3_column_type(row, 3) == SQLITE_INTEGER,
              sqlite3_column_type(row, 4) == SQLITE_INTEGER,
              [0, 1].contains(completion), [0, 1].contains(attention) else { throw DevicePreferenceError.storage }
        return DevicePreferenceSnapshot(deviceID: deviceID, revision: UInt64(revision), completion: completion == 1, attention: attention == 1, privacyCover: false)
    }

    private static func canonicalID(_ row: OpaquePointer, _ column: Int32) throws -> UUID {
        let text = try PreferenceDatabase.text(row, column, maximumBytes: 36)
        guard let id = UUID(uuidString: text), id != zeroID, id.uuidString.lowercased() == text else {
            throw DevicePreferenceError.storage
        }
        return id
    }

    private static func validateRequests(_ database: PreferenceDatabase, deviceID: UUID, revision: UInt64, version: Int64) throws {
        let count = try integer(database, "SELECT COUNT(*) FROM requests")
        guard (0...Int64(maximumRequests)).contains(count) else { throw DevicePreferenceError.capacity }
        let query = version == 1 ? "SELECT id,original,outcome,revision FROM requests" : "SELECT id,original,outcome,revision,kind FROM requests"
        try database.rows(query) { row in
            let request = try readRequest(row, deviceID: deviceID, version: version)
            let observed: UInt64
            switch request.result.outcome {
            case .applied(let value), .conflict(let value): observed = value
            }
            guard observed <= revision else { throw DevicePreferenceError.storage }
        }
    }

    private static func readRequest(_ row: OpaquePointer, deviceID: UUID, version: Int64) throws -> StoredPreferenceRequest {
        let requestID = try canonicalID(row, 0)
        let original = try PreferenceDatabase.data(row, 1)
        let revision = sqlite3_column_int64(row, 3)
        guard sqlite3_column_type(row, 3) == SQLITE_INTEGER, revision > 0 else { throw DevicePreferenceError.storage }
        let kind: PreferenceRequestKind
        if version == 1 { kind = .notifications }
        else {
            guard let value = PreferenceRequestKind(rawValue: try PreferenceDatabase.text(row, 4, maximumBytes: 13)) else {
                throw DevicePreferenceError.storage
            }
            kind = value
        }
        let mutation: PreferenceMutation
        do {
            mutation = try JSONDecoder().decode(PreferenceRequestIdentity.self, from: original).mutation(for: kind)
            guard mutation.requestID == requestID, mutation.deviceID == deviceID,
                  try mutation.encodedIdentity() == original else { throw DevicePreferenceError.storage }
        } catch { throw DevicePreferenceError.storage }
        let outcome: DevicePreferenceResult.Outcome
        switch try PreferenceDatabase.text(row, 2, maximumBytes: 8) {
        case "applied":
            guard mutation.expectedRevision < UInt64(Int64.max), mutation.expectedRevision + 1 == UInt64(revision) else {
                throw DevicePreferenceError.storage
            }
            outcome = .applied(revision: UInt64(revision))
        case "conflict":
            guard mutation.expectedRevision != UInt64(revision) else { throw DevicePreferenceError.storage }
            outcome = .conflict(observedRevision: UInt64(revision))
        default: throw DevicePreferenceError.storage
        }
        return StoredPreferenceRequest(kind: kind, original: original,
            result: DevicePreferenceResult(requestID: requestID, deviceID: deviceID, outcome: outcome))
    }
}

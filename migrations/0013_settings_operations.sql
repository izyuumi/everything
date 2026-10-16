CREATE TABLE settings_operations (
    request_id TEXT PRIMARY KEY NOT NULL CHECK(length(request_id) = 36 AND lower(request_id) = request_id AND request_id NOT GLOB '*[^0-9a-f-]*' AND length(replace(request_id, '-', '')) = 32 AND substr(request_id, 9, 1) = '-' AND substr(request_id, 14, 1) = '-' AND substr(request_id, 19, 1) = '-' AND substr(request_id, 24, 1) = '-'),
    identity TEXT NOT NULL CHECK(json_valid(identity) AND json_type(identity) = 'object' AND length(CAST(identity AS BLOB)) <= 18432 AND coalesce(json_extract(identity, '$.version') = 1 AND json_extract(identity, '$.binding.request_id') = request_id, 0)),
    created_at INTEGER NOT NULL CHECK(created_at >= 0 AND coalesce(json_extract(identity, '$.binding.issued_at') = created_at, 0)),
    expires_at INTEGER NOT NULL CHECK(expires_at > created_at AND expires_at - created_at <= 300000 AND coalesce(json_extract(identity, '$.binding.expires_at') = expires_at, 0))
) STRICT;
CREATE TABLE settings_operation_updates (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    request_id TEXT NOT NULL REFERENCES settings_operations(request_id),
    record TEXT NOT NULL CHECK(json_valid(record) AND json_type(record) = 'object' AND length(CAST(record AS BLOB)) <= 2048 AND coalesce(json_extract(record, '$.phase.phase') IN ('awaiting_confirmation', 'awaiting_action', 'in_flight', 'applying', 'finished'), 0)),
    occurred_at INTEGER NOT NULL CHECK(occurred_at >= 0 AND coalesce(json_extract(record, '$.updated_at') = occurred_at, 0)),
    CHECK(coalesce(json_extract(record, '$.phase.phase') != 'applying' OR (json_type(record, '$.write') = 'object' AND json_type(record, '$.write.resolution') = 'null' AND json_extract(record, '$.phase.expected_revision') = json_extract(record, '$.write.intent.before.revision') AND json_extract(record, '$.phase.revision') = json_extract(record, '$.write.intent.after.revision')), 0)),
    CHECK(json_type(record, '$.write') IS NULL OR coalesce(json_type(record, '$.write') = 'object' AND json_type(record, '$.write.intent') = 'object' AND json_extract(record, '$.write.intent.version') = 1 AND json_extract(record, '$.phase.phase') IN ('applying', 'finished') AND json_extract(record, '$.write.intent.before.revision') > 0 AND json_extract(record, '$.write.intent.before.revision') < 9223372036854775807 AND json_extract(record, '$.write.intent.after.revision') = json_extract(record, '$.write.intent.before.revision') + 1 AND length(json_extract(record, '$.write.intent.before.sha256')) = 64 AND json_extract(record, '$.write.intent.before.sha256') NOT GLOB '*[^0-9a-f]*' AND length(json_extract(record, '$.write.intent.after.sha256')) = 64 AND json_extract(record, '$.write.intent.after.sha256') NOT GLOB '*[^0-9a-f]*' AND (json_type(record, '$.write.resolution') = 'null' OR json_extract(record, '$.write.resolution') IN ('applied', 'not_applied')), 0))
) STRICT;
CREATE INDEX settings_operation_history ON settings_operation_updates(request_id, sequence);
CREATE TABLE settings_native_requests (
    request_id TEXT PRIMARY KEY NOT NULL CHECK(length(request_id) = 36 AND lower(request_id) = request_id AND request_id NOT GLOB '*[^0-9a-f-]*' AND length(replace(request_id, '-', '')) = 32 AND substr(request_id, 9, 1) = '-' AND substr(request_id, 14, 1) = '-' AND substr(request_id, 19, 1) = '-' AND substr(request_id, 24, 1) = '-'),
    operation_id TEXT UNIQUE NOT NULL REFERENCES settings_operations(request_id),
    exact_request TEXT NOT NULL CHECK(json_valid(exact_request) AND json_type(exact_request) = 'object' AND length(CAST(exact_request AS BLOB)) <= 1024 AND coalesce(json_extract(exact_request, '$.native_request_id') = request_id AND json_extract(exact_request, '$.operation_id') = operation_id, 0))
) STRICT;
CREATE TRIGGER settings_operations_no_update BEFORE UPDATE ON settings_operations BEGIN SELECT RAISE(ABORT, 'settings operation identity is immutable'); END;
CREATE TRIGGER settings_operations_no_delete BEFORE DELETE ON settings_operations BEGIN SELECT RAISE(ABORT, 'settings operation identity is immutable'); END;
CREATE TRIGGER settings_updates_no_update BEFORE UPDATE ON settings_operation_updates BEGIN SELECT RAISE(ABORT, 'settings operation history is immutable'); END;
CREATE TRIGGER settings_updates_no_delete BEFORE DELETE ON settings_operation_updates BEGIN SELECT RAISE(ABORT, 'settings operation history is immutable'); END;
CREATE TRIGGER settings_native_requests_no_update BEFORE UPDATE ON settings_native_requests BEGIN SELECT RAISE(ABORT, 'native request identity is immutable'); END;
CREATE TRIGGER settings_native_requests_no_delete BEFORE DELETE ON settings_native_requests BEGIN SELECT RAISE(ABORT, 'native request identity is immutable'); END;

CREATE TABLE worker_endpoint (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    identity_json TEXT NOT NULL,
    role TEXT NOT NULL CHECK(role IN ('coordinator', 'executor')),
    origin TEXT NOT NULL
) STRICT;

CREATE TABLE worker_peers (
    device_id TEXT PRIMARY KEY,
    keys_json TEXT NOT NULL,
    pinned_at INTEGER NOT NULL CHECK(pinned_at >= 0),
    revoked_at INTEGER CHECK(revoked_at >= 0)
) STRICT;

CREATE TABLE worker_profile (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    profile_json TEXT NOT NULL CHECK(length(CAST(profile_json AS BLOB)) <= 32768)
) STRICT;

CREATE TRIGGER worker_profile_immutable BEFORE UPDATE ON worker_profile
BEGIN
    SELECT RAISE(ABORT, 'worker model profile is immutable');
END;

CREATE TABLE worker_delegations (
    delegation_id TEXT PRIMARY KEY,
    attempt_id TEXT NOT NULL UNIQUE,
    peer_id TEXT NOT NULL REFERENCES worker_peers(device_id),
    manifest_digest TEXT NOT NULL CHECK(length(manifest_digest) = 64),
    manifest_json TEXT CHECK(length(CAST(manifest_json AS BLOB)) <= 491520),
    state TEXT NOT NULL CHECK(state IN ('offered', 'accepted', 'running', 'cancel_requested', 'result_ready', 'acknowledged', 'rejected', 'revoked')),
    cancel_command TEXT,
    cancellation_acknowledged INTEGER NOT NULL DEFAULT 0 CHECK(cancellation_acknowledged IN (0, 1)),
    rejection_json TEXT,
    result_json TEXT CHECK(length(CAST(result_json AS BLOB)) <= 491520),
    result_digest TEXT,
    quarantine_digest TEXT,
    receipt_id TEXT,
    updated_at INTEGER NOT NULL CHECK(updated_at >= 0),
    CHECK((result_json IS NULL) = (result_digest IS NULL)),
    CHECK(quarantine_digest IS NULL OR quarantine_digest = result_digest)
) STRICT;

CREATE TABLE worker_messages (
    message_id TEXT PRIMARY KEY,
    peer_id TEXT NOT NULL REFERENCES worker_peers(device_id),
    direction TEXT NOT NULL CHECK(direction IN ('inbound', 'outbound')),
    packet_json BLOB NOT NULL CHECK(length(packet_json) BETWEEN 1 AND 524288),
    envelope_json TEXT NOT NULL CHECK(length(envelope_json) BETWEEN 1 AND 786432),
    expires_at INTEGER NOT NULL CHECK(expires_at >= 0),
    recorded_at INTEGER NOT NULL CHECK(recorded_at >= 0),
    relay_accepted INTEGER NOT NULL DEFAULT 0 CHECK(relay_accepted IN (0, 1)),
    suppressed INTEGER NOT NULL DEFAULT 0 CHECK(suppressed IN (0, 1))
) STRICT;

CREATE TABLE worker_native_admissions (
    task_id TEXT PRIMARY KEY,
    delegation_id TEXT NOT NULL UNIQUE REFERENCES worker_delegations(delegation_id),
    manifest_digest TEXT NOT NULL CHECK(length(manifest_digest) = 64),
    submission_json TEXT NOT NULL CHECK(length(CAST(submission_json AS BLOB)) <= 32768)
) STRICT;

CREATE TRIGGER worker_native_admission_immutable BEFORE UPDATE ON worker_native_admissions
BEGIN
    SELECT RAISE(ABORT, 'worker native admission is immutable');
END;

CREATE INDEX worker_pending_outbox ON worker_messages(direction, relay_accepted, recorded_at);
CREATE INDEX worker_ready_jobs ON worker_delegations(state, updated_at);

CREATE TRIGGER worker_endpoint_immutable BEFORE UPDATE ON worker_endpoint
BEGIN
    SELECT RAISE(ABORT, 'worker endpoint is immutable');
END;

CREATE TRIGGER worker_peer_keys_immutable BEFORE UPDATE OF device_id, keys_json, pinned_at ON worker_peers
BEGIN
    SELECT RAISE(ABORT, 'worker peer pin is immutable');
END;

CREATE TRIGGER worker_peer_revocation_sticky BEFORE UPDATE OF revoked_at ON worker_peers
WHEN OLD.revoked_at IS NOT NULL AND NEW.revoked_at IS NOT OLD.revoked_at
BEGIN
    SELECT RAISE(ABORT, 'worker peer revocation is permanent');
END;

CREATE TRIGGER worker_binding_immutable BEFORE UPDATE OF delegation_id, attempt_id, peer_id, manifest_digest ON worker_delegations
BEGIN
    SELECT RAISE(ABORT, 'worker binding is immutable');
END;

CREATE TRIGGER worker_manifest_immutable BEFORE UPDATE OF manifest_json ON worker_delegations
WHEN OLD.manifest_json IS NOT NULL AND NEW.manifest_json IS NOT OLD.manifest_json
BEGIN
    SELECT RAISE(ABORT, 'worker manifest is immutable');
END;

CREATE TRIGGER worker_result_immutable BEFORE UPDATE OF result_json, result_digest ON worker_delegations
WHEN OLD.result_json IS NOT NULL AND (NEW.result_json IS NOT OLD.result_json OR NEW.result_digest IS NOT OLD.result_digest)
BEGIN
    SELECT RAISE(ABORT, 'worker result is immutable');
END;

CREATE TRIGGER worker_receipt_immutable BEFORE UPDATE OF receipt_id ON worker_delegations
WHEN OLD.receipt_id IS NOT NULL AND NEW.receipt_id IS NOT OLD.receipt_id
BEGIN
    SELECT RAISE(ABORT, 'worker receipt is immutable');
END;

CREATE TRIGGER worker_message_immutable BEFORE UPDATE OF message_id, peer_id, direction, packet_json, envelope_json, expires_at, recorded_at ON worker_messages
BEGIN
    SELECT RAISE(ABORT, 'worker message is immutable');
END;

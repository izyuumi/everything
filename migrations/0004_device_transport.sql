CREATE TABLE transport_endpoint (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    device_id TEXT NOT NULL,
    configuration TEXT NOT NULL CHECK (json_valid(configuration)),
    revoked_at INTEGER CHECK (revoked_at IS NULL OR revoked_at >= 0)
) STRICT;
CREATE TABLE pairing_uses (
    pairing_id TEXT PRIMARY KEY NOT NULL,
    device_id TEXT NOT NULL REFERENCES devices(id),
    completed_at INTEGER NOT NULL CHECK (completed_at >= 0)
) STRICT;
CREATE TABLE transport_outbox (
    message_id TEXT PRIMARY KEY NOT NULL REFERENCES outbox(id) ON DELETE CASCADE,
    expires_at INTEGER NOT NULL CHECK (expires_at >= 0)
) STRICT;
CREATE TABLE transport_inbox (
    message_id TEXT PRIMARY KEY NOT NULL,
    sender TEXT NOT NULL REFERENCES devices(id),
    recipient TEXT NOT NULL,
    envelope_digest TEXT NOT NULL CHECK (length(envelope_digest) = 64),
    payload BLOB,
    received_at INTEGER NOT NULL CHECK (received_at >= 0),
    expires_at INTEGER NOT NULL CHECK (expires_at > received_at),
    state TEXT NOT NULL CHECK (state IN ('pending', 'processed', 'rejected')),
    detail TEXT,
    command_id TEXT,
    CHECK (state != 'pending' OR (payload IS NOT NULL AND length(payload) BETWEEN 1 AND 1048576))
) STRICT;
CREATE INDEX transport_inbox_pending ON transport_inbox(state, received_at, message_id);
CREATE INDEX transport_inbox_sender ON transport_inbox(sender, state);
CREATE INDEX transport_inbox_command ON transport_inbox(command_id);
CREATE TABLE remote_commands (
    command_id TEXT PRIMARY KEY NOT NULL,
    sender TEXT NOT NULL REFERENCES devices(id),
    conversation_id TEXT NOT NULL,
    request TEXT CHECK (request IS NULL OR json_valid(request)),
    request_digest TEXT NOT NULL CHECK (length(request_digest) = 64),
    state TEXT NOT NULL CHECK (state IN ('pending', 'completed', 'rejected')),
    result TEXT CHECK (result IS NULL OR json_valid(result)),
    result_digest TEXT CHECK (result_digest IS NULL OR length(result_digest) = 64),
    received_at INTEGER NOT NULL CHECK (received_at >= 0),
    updated_at INTEGER NOT NULL CHECK (updated_at >= received_at),
    CHECK (state != 'pending' OR request IS NOT NULL)
) STRICT;
CREATE INDEX remote_commands_pending ON remote_commands(state, received_at, command_id);
CREATE TABLE replica_events (
    host_id TEXT NOT NULL REFERENCES devices(id),
    conversation_id TEXT NOT NULL,
    sequence INTEGER NOT NULL CHECK (sequence > 0),
    event_id TEXT NOT NULL,
    envelope TEXT NOT NULL CHECK (json_valid(envelope)),
    PRIMARY KEY (host_id, conversation_id, sequence),
    UNIQUE (host_id, event_id)
) STRICT;
CREATE TABLE replica_cursors (
    host_id TEXT NOT NULL REFERENCES devices(id),
    conversation_id TEXT NOT NULL,
    after_sequence INTEGER NOT NULL CHECK (after_sequence >= 0),
    PRIMARY KEY (host_id, conversation_id)
) STRICT;
CREATE TABLE sync_offers (
    message_id TEXT PRIMARY KEY NOT NULL,
    device_id TEXT NOT NULL REFERENCES devices(id),
    conversation_id TEXT NOT NULL,
    after_sequence INTEGER NOT NULL CHECK (after_sequence >= 0),
    through_sequence INTEGER NOT NULL CHECK (through_sequence >= after_sequence),
    last_event_id TEXT,
    expires_at INTEGER NOT NULL CHECK (expires_at >= 0)
) STRICT;
CREATE INDEX sync_offers_device ON sync_offers(device_id);
CREATE TABLE device_sync_cursors (
    device_id TEXT NOT NULL REFERENCES devices(id),
    conversation_id TEXT NOT NULL,
    after_sequence INTEGER NOT NULL CHECK (after_sequence >= 0),
    PRIMARY KEY (device_id, conversation_id)
) STRICT;

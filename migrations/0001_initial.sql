CREATE TABLE conversations (
    id TEXT PRIMARY KEY NOT NULL,
    title TEXT NOT NULL,
    created_at INTEGER NOT NULL CHECK (created_at >= 0),
    updated_at INTEGER NOT NULL CHECK (updated_at >= created_at),
    last_sequence INTEGER NOT NULL DEFAULT 0 CHECK (last_sequence >= 0)
) STRICT;
CREATE TABLE events (
    id TEXT PRIMARY KEY NOT NULL,
    conversation_id TEXT NOT NULL REFERENCES conversations(id),
    sequence INTEGER NOT NULL CHECK (sequence > 0),
    kind TEXT NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    occurred_at INTEGER NOT NULL CHECK (occurred_at >= 0),
    UNIQUE (conversation_id, sequence)
) STRICT;
CREATE TABLE commands (
    id TEXT PRIMARY KEY NOT NULL,
    conversation_id TEXT NOT NULL REFERENCES conversations(id),
    request TEXT NOT NULL CHECK (json_valid(request)),
    first_sequence INTEGER NOT NULL CHECK (first_sequence > 0),
    last_sequence INTEGER NOT NULL CHECK (last_sequence >= first_sequence)
) STRICT;
CREATE TABLE tasks (
    id TEXT PRIMARY KEY NOT NULL,
    conversation_id TEXT NOT NULL REFERENCES conversations(id),
    parent_task_id TEXT REFERENCES tasks(id),
    state TEXT NOT NULL CHECK (state IN ('queued', 'running', 'waiting', 'completed', 'failed', 'cancelled')),
    input TEXT NOT NULL CHECK (json_valid(input)),
    checkpoint TEXT NOT NULL CHECK (json_valid(checkpoint)),
    created_at INTEGER NOT NULL CHECK (created_at >= 0),
    updated_at INTEGER NOT NULL CHECK (updated_at >= created_at),
    CHECK (parent_task_id IS NULL OR parent_task_id != id)
) STRICT;
CREATE INDEX tasks_state_order ON tasks(state, created_at, id);
CREATE INDEX tasks_conversation ON tasks(conversation_id, created_at, id);
CREATE TABLE tool_calls (
    id TEXT PRIMARY KEY NOT NULL,
    conversation_id TEXT NOT NULL REFERENCES conversations(id),
    task_id TEXT REFERENCES tasks(id),
    tool_name TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending', 'running', 'succeeded', 'failed', 'cancelled', 'denied', 'unknown')),
    request TEXT NOT NULL CHECK (json_valid(request)),
    result TEXT CHECK (result IS NULL OR json_valid(result)),
    created_at INTEGER NOT NULL CHECK (created_at >= 0),
    updated_at INTEGER NOT NULL CHECK (updated_at >= created_at)
) STRICT;
CREATE INDEX tool_calls_state_order ON tool_calls(state, created_at, id);
CREATE INDEX tool_calls_conversation ON tool_calls(conversation_id, created_at, id);
CREATE TABLE permissions (
    id TEXT PRIMARY KEY NOT NULL,
    conversation_id TEXT REFERENCES conversations(id),
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    created_at INTEGER NOT NULL CHECK (created_at >= 0),
    expires_at INTEGER CHECK (expires_at IS NULL OR expires_at >= created_at),
    revoked_at INTEGER CHECK (revoked_at IS NULL OR revoked_at >= created_at)
) STRICT;
CREATE INDEX permissions_conversation ON permissions(conversation_id);
CREATE TABLE devices (
    id TEXT PRIMARY KEY NOT NULL,
    label TEXT NOT NULL,
    public_identity TEXT NOT NULL CHECK (json_valid(public_identity)),
    created_at INTEGER NOT NULL CHECK (created_at >= 0),
    revoked_at INTEGER CHECK (revoked_at IS NULL OR revoked_at >= created_at)
) STRICT;
CREATE TABLE outbox (
    id TEXT PRIMARY KEY NOT NULL,
    device_id TEXT NOT NULL REFERENCES devices(id),
    envelope BLOB NOT NULL CHECK (length(envelope) > 0),
    created_at INTEGER NOT NULL CHECK (created_at >= 0),
    not_before INTEGER NOT NULL CHECK (not_before >= created_at),
    attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    delivered_at INTEGER CHECK (delivered_at IS NULL OR delivered_at >= created_at)
) STRICT;
CREATE INDEX outbox_delivery ON outbox(delivered_at, not_before, created_at, id);

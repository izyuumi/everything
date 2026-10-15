CREATE TABLE conversation_states (
    conversation_id TEXT PRIMARY KEY NOT NULL REFERENCES conversations(id),
    revision INTEGER NOT NULL CHECK (revision > 0),
    payload TEXT NOT NULL CHECK (json_valid(payload) AND length(CAST(payload AS BLOB)) <= 8388608),
    updated_at INTEGER NOT NULL CHECK (updated_at >= 0)
) STRICT;
ALTER TABLE commands ADD COLUMN state_digest TEXT CHECK (state_digest IS NULL OR length(state_digest) = 64);

CREATE TABLE conversation_checkpoint_commands (
    command_id TEXT PRIMARY KEY NOT NULL CHECK (length(CAST(command_id AS BLOB)) BETWEEN 1 AND 256),
    conversation_id TEXT NOT NULL REFERENCES conversations(id),
    expected_sequence INTEGER NOT NULL CHECK (expected_sequence >= 0),
    expected_revision INTEGER NOT NULL CHECK (expected_revision >= 0),
    request_digest TEXT NOT NULL CHECK (length(request_digest) = 64),
    revision INTEGER NOT NULL CHECK (revision > 0 AND expected_revision = revision - 1),
    updated_at INTEGER NOT NULL CHECK (updated_at >= 0)
) STRICT;
CREATE INDEX conversation_checkpoint_commands_conversation ON conversation_checkpoint_commands(conversation_id);

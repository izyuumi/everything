CREATE TABLE host_commands (
    command_id TEXT PRIMARY KEY NOT NULL CHECK (length(command_id) = 36),
    conversation_id TEXT NOT NULL CHECK (length(conversation_id) = 36),
    input_kind TEXT NOT NULL CHECK (input_kind IN ('domain', 'local_text', 'local_initialize')),
    origin TEXT NOT NULL CHECK (json_valid(origin) AND length(CAST(origin AS BLOB)) <= 16384),
    request_digest TEXT NOT NULL CHECK (length(request_digest) = 64),
    original TEXT CHECK (original IS NULL OR (json_valid(original) AND length(CAST(original AS BLOB)) <= 262144)),
    effective_options TEXT CHECK (effective_options IS NULL OR (json_valid(effective_options) AND length(CAST(effective_options AS BLOB)) <= 65536)),
    phase TEXT NOT NULL CHECK (phase IN ('accepted', 'applied', 'completed', 'rejected')),
    applied_receipt TEXT CHECK (applied_receipt IS NULL OR (json_valid(applied_receipt) AND length(CAST(applied_receipt AS BLOB)) <= 4096)),
    terminal_result TEXT CHECK (terminal_result IS NULL OR (json_valid(terminal_result) AND length(CAST(terminal_result AS BLOB)) <= 65536)),
    result_digest TEXT CHECK (result_digest IS NULL OR length(result_digest) = 64),
    accepted_at INTEGER NOT NULL CHECK (accepted_at >= 0),
    updated_at INTEGER NOT NULL CHECK (updated_at >= accepted_at),
    CHECK (phase IN ('completed', 'rejected') OR (original IS NOT NULL AND effective_options IS NOT NULL)),
    CHECK (phase != 'applied' OR applied_receipt IS NOT NULL),
    CHECK ((phase IN ('completed', 'rejected')) = (result_digest IS NOT NULL))
) STRICT;
CREATE INDEX host_commands_pending ON host_commands(phase, accepted_at, command_id);
CREATE INDEX host_commands_conversation ON host_commands(conversation_id, accepted_at, command_id);

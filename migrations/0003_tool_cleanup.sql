CREATE TABLE tool_cleanup_attestations (
    operation_id TEXT PRIMARY KEY NOT NULL REFERENCES tool_calls(id),
    original_result_digest TEXT NOT NULL CHECK (length(original_result_digest) = 64 AND original_result_digest NOT GLOB '*[^0-9a-f]*'),
    operator TEXT NOT NULL,
    note TEXT NOT NULL,
    attested_at INTEGER NOT NULL CHECK (attested_at >= 0)
) STRICT;

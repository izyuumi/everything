CREATE TABLE worker_terminal_sources (
    delegation_id TEXT PRIMARY KEY REFERENCES worker_delegations(delegation_id),
    message_id TEXT NOT NULL UNIQUE REFERENCES worker_messages(message_id)
) STRICT;

CREATE TRIGGER worker_terminal_source_immutable BEFORE UPDATE ON worker_terminal_sources
BEGIN
    SELECT RAISE(ABORT, 'worker terminal source is immutable');
END;

CREATE TRIGGER worker_terminal_source_retained BEFORE DELETE ON worker_terminal_sources
BEGIN
    SELECT RAISE(ABORT, 'worker terminal source must remain retained');
END;

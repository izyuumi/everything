CREATE TABLE scheduled_attempts (
    occurrence_id TEXT PRIMARY KEY NOT NULL REFERENCES scheduled_runs(occurrence_id),
    attempt_number INTEGER NOT NULL CHECK (attempt_number = 1),
    task_id TEXT NOT NULL UNIQUE,
    conversation_id TEXT NOT NULL UNIQUE,
    create_command_id TEXT NOT NULL UNIQUE,
    input_command_id TEXT NOT NULL UNIQUE,
    accepted_at INTEGER NOT NULL CHECK (accepted_at >= 0),
    deadline INTEGER NOT NULL CHECK (deadline >= accepted_at),
    execution TEXT NOT NULL CHECK (json_valid(execution) AND length(CAST(execution AS BLOB)) <= 32768)
) STRICT;
ALTER TABLE scheduling_usage ADD COLUMN attempt_bytes INTEGER NOT NULL DEFAULT 0 CHECK (attempt_bytes BETWEEN 0 AND 67108864);
CREATE TRIGGER scheduling_attempt_identity BEFORE UPDATE ON scheduled_attempts BEGIN
    SELECT RAISE(ABORT, 'scheduled attempt identities and policy are immutable');
END;
CREATE TRIGGER scheduling_attempt_insert AFTER INSERT ON scheduled_attempts BEGIN
    UPDATE scheduling_usage SET attempt_bytes=attempt_bytes+length(CAST(NEW.execution AS BLOB)) WHERE singleton=1;
END;
CREATE TRIGGER scheduling_attempt_delete AFTER DELETE ON scheduled_attempts BEGIN
    UPDATE scheduling_usage SET attempt_bytes=attempt_bytes-length(CAST(OLD.execution AS BLOB)) WHERE singleton=1;
END;

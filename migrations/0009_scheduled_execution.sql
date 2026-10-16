DROP TRIGGER scheduling_attempt_identity;
DROP TRIGGER scheduling_attempt_insert;
DROP TRIGGER scheduling_attempt_delete;
ALTER TABLE scheduled_attempts RENAME TO scheduled_attempts_v8;
CREATE TABLE scheduled_attempts (
    occurrence_id TEXT NOT NULL REFERENCES scheduled_runs(occurrence_id),
    attempt_number INTEGER NOT NULL CHECK (attempt_number BETWEEN 1 AND 8),
    task_id TEXT NOT NULL UNIQUE,
    conversation_id TEXT NOT NULL UNIQUE,
    create_command_id TEXT NOT NULL UNIQUE,
    input_command_id TEXT NOT NULL UNIQUE,
    accepted_at INTEGER NOT NULL CHECK (accepted_at >= 0),
    deadline INTEGER NOT NULL CHECK (deadline >= accepted_at),
    execution TEXT NOT NULL CHECK (json_valid(execution) AND length(CAST(execution AS BLOB)) <= 32768),
    PRIMARY KEY(occurrence_id,attempt_number)
) STRICT;
INSERT INTO scheduled_attempts SELECT * FROM scheduled_attempts_v8;
DROP TABLE scheduled_attempts_v8;
CREATE TABLE scheduling_execution_usage (
    singleton INTEGER PRIMARY KEY CHECK (singleton=1),
    attempts INTEGER NOT NULL CHECK (attempts BETWEEN 0 AND 100000),
    open_attempts INTEGER NOT NULL CHECK (open_attempts BETWEEN 0 AND 100000),
    result_bytes INTEGER NOT NULL CHECK (result_bytes BETWEEN 0 AND 204800000),
    delivery_slots INTEGER NOT NULL CHECK (delivery_slots BETWEEN 0 AND 2048),
    acknowledgement_bytes INTEGER NOT NULL CHECK (acknowledgement_bytes BETWEEN 0 AND 16777216)
) STRICT;
INSERT INTO scheduling_execution_usage SELECT 1,count(*),count(*),0,0,0 FROM scheduled_attempts;
CREATE TABLE scheduled_attempt_results (
    occurrence_id TEXT NOT NULL,
    attempt_number INTEGER NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('factory_failure','terminal')),
    settled_at INTEGER NOT NULL CHECK(settled_at>=0),
    model_requests INTEGER NOT NULL CHECK(model_requests BETWEEN 0 AND 128),
    tool_calls INTEGER NOT NULL CHECK(tool_calls BETWEEN 0 AND 1024),
    reserved_output_tokens INTEGER NOT NULL CHECK(reserved_output_tokens BETWEEN 0 AND 262144),
    usage_known INTEGER NOT NULL CHECK(usage_known IN (0,1)),
    diagnostic TEXT NOT NULL CHECK(length(CAST(diagnostic AS BLOB))<=1024),
    outcome_digest TEXT NOT NULL CHECK(length(outcome_digest)=64),
    next_retry_at INTEGER CHECK(next_retry_at>=settled_at),
    PRIMARY KEY(occurrence_id,attempt_number),
    FOREIGN KEY(occurrence_id,attempt_number) REFERENCES scheduled_attempts(occurrence_id,attempt_number),
    CHECK(kind!='factory_failure' OR (model_requests=0 AND tool_calls=0 AND reserved_output_tokens=0 AND usage_known=1))
) STRICT;
CREATE TABLE scheduled_attempt_admissions (
    occurrence_id TEXT NOT NULL,
    attempt_number INTEGER NOT NULL,
    admitted_at INTEGER NOT NULL CHECK(admitted_at>=0),
    PRIMARY KEY(occurrence_id,attempt_number),
    FOREIGN KEY(occurrence_id,attempt_number) REFERENCES scheduled_attempts(occurrence_id,attempt_number)
) STRICT;
INSERT INTO scheduled_attempt_admissions SELECT a.occurrence_id,a.attempt_number,COALESCE(r.started_at,r.claimed_at) FROM scheduled_attempts a JOIN scheduled_runs r ON r.occurrence_id=a.occurrence_id WHERE r.state='running';
CREATE TRIGGER scheduling_admission_immutable BEFORE UPDATE ON scheduled_attempt_admissions BEGIN
    SELECT RAISE(ABORT,'scheduled admission proofs are immutable');
END;
CREATE TABLE scheduled_factory_retirements (
    occurrence_id TEXT NOT NULL,
    attempt_number INTEGER NOT NULL,
    command_id TEXT NOT NULL UNIQUE,
    accepted_at INTEGER NOT NULL CHECK(accepted_at>=0),
    PRIMARY KEY(occurrence_id,attempt_number),
    FOREIGN KEY(occurrence_id,attempt_number) REFERENCES scheduled_attempts(occurrence_id,attempt_number)
) STRICT;
CREATE TABLE scheduled_unknown_acknowledgements (
    occurrence_id TEXT PRIMARY KEY REFERENCES scheduled_runs(occurrence_id),
    command_id TEXT NOT NULL UNIQUE,
    outcome_digest TEXT NOT NULL CHECK(length(outcome_digest)=64),
    operator_uid INTEGER NOT NULL CHECK(operator_uid BETWEEN 0 AND 4294967295),
    note TEXT NOT NULL CHECK(length(CAST(note AS BLOB)) BETWEEN 1 AND 1024),
    acknowledged_at INTEGER NOT NULL CHECK(acknowledged_at>=0)
) STRICT;
CREATE TABLE scheduled_deliveries (
    occurrence_id TEXT PRIMARY KEY REFERENCES scheduled_runs(occurrence_id),
    conversation_id TEXT NOT NULL,
    command_id TEXT NOT NULL UNIQUE,
    event_id TEXT NOT NULL UNIQUE,
    accepted_at INTEGER CHECK(accepted_at>=0),
    outcome_digest TEXT CHECK(length(outcome_digest)=64),
    notification TEXT CHECK(notification IS NULL OR (json_valid(notification) AND length(CAST(notification AS BLOB))<=32768)),
    acknowledged_sequence INTEGER CHECK(acknowledged_sequence>0),
    CHECK((accepted_at IS NULL AND outcome_digest IS NULL AND notification IS NULL AND acknowledged_sequence IS NULL) OR (accepted_at IS NOT NULL AND outcome_digest IS NOT NULL AND (notification IS NOT NULL OR acknowledged_sequence IS NOT NULL)))
) STRICT;
CREATE TRIGGER scheduling_attempt_identity BEFORE UPDATE ON scheduled_attempts BEGIN
    SELECT RAISE(ABORT,'scheduled attempt identities and policy are immutable');
END;
CREATE TRIGGER scheduling_attempt_insert AFTER INSERT ON scheduled_attempts BEGIN
    UPDATE scheduling_usage SET attempt_bytes=attempt_bytes+length(CAST(NEW.execution AS BLOB)) WHERE singleton=1;
    UPDATE scheduling_execution_usage SET attempts=attempts+1,open_attempts=open_attempts+1 WHERE singleton=1;
END;
CREATE TRIGGER scheduling_attempt_delete AFTER DELETE ON scheduled_attempts BEGIN
    UPDATE scheduling_usage SET attempt_bytes=attempt_bytes-length(CAST(OLD.execution AS BLOB)) WHERE singleton=1;
    UPDATE scheduling_execution_usage SET attempts=attempts-1,open_attempts=open_attempts-1 WHERE singleton=1;
END;
CREATE TRIGGER scheduling_result_immutable BEFORE UPDATE ON scheduled_attempt_results BEGIN
    SELECT RAISE(ABORT,'scheduled attempt results are immutable');
END;
CREATE TRIGGER scheduling_result_retained BEFORE DELETE ON scheduled_attempt_results BEGIN
    SELECT RAISE(ABORT,'scheduled attempt results are retained');
END;
CREATE TRIGGER scheduling_result_insert AFTER INSERT ON scheduled_attempt_results BEGIN
    UPDATE scheduling_execution_usage SET open_attempts=open_attempts-1,result_bytes=result_bytes+length(CAST(NEW.diagnostic AS BLOB))+192 WHERE singleton=1;
END;
CREATE TRIGGER scheduling_retirement_immutable BEFORE UPDATE ON scheduled_factory_retirements BEGIN
    SELECT RAISE(ABORT,'scheduled factory retirement identities are immutable');
END;
CREATE TRIGGER scheduling_ack_immutable BEFORE UPDATE ON scheduled_unknown_acknowledgements BEGIN
    SELECT RAISE(ABORT,'scheduled uncertainty acknowledgements are immutable');
END;
CREATE TRIGGER scheduling_ack_retained BEFORE DELETE ON scheduled_unknown_acknowledgements BEGIN
    SELECT RAISE(ABORT,'scheduled uncertainty acknowledgements are retained');
END;
CREATE TRIGGER scheduling_ack_insert AFTER INSERT ON scheduled_unknown_acknowledgements BEGIN
    UPDATE scheduling_execution_usage SET acknowledgement_bytes=acknowledgement_bytes+length(CAST(NEW.note AS BLOB))+192 WHERE singleton=1;
END;
CREATE TRIGGER scheduling_delivery_identity BEFORE UPDATE ON scheduled_deliveries WHEN
    NEW.occurrence_id!=OLD.occurrence_id OR NEW.conversation_id!=OLD.conversation_id OR NEW.command_id!=OLD.command_id OR NEW.event_id!=OLD.event_id OR OLD.acknowledged_sequence IS NOT NULL OR
    (OLD.accepted_at IS NOT NULL AND (NEW.accepted_at IS NOT OLD.accepted_at OR NEW.outcome_digest IS NOT OLD.outcome_digest OR (NEW.notification IS NOT OLD.notification AND NEW.acknowledged_sequence IS NULL))) BEGIN
    SELECT RAISE(ABORT,'scheduled delivery identity and payload are immutable');
END;
CREATE TRIGGER scheduling_delivery_insert AFTER INSERT ON scheduled_deliveries BEGIN
    UPDATE scheduling_execution_usage SET delivery_slots=delivery_slots+1 WHERE singleton=1;
END;
CREATE TRIGGER scheduling_delivery_ack AFTER UPDATE OF acknowledged_sequence ON scheduled_deliveries WHEN OLD.acknowledged_sequence IS NULL AND NEW.acknowledged_sequence IS NOT NULL BEGIN
    UPDATE scheduling_execution_usage SET delivery_slots=delivery_slots-1 WHERE singleton=1;
END;
CREATE TRIGGER scheduling_delivery_retained BEFORE DELETE ON scheduled_deliveries BEGIN
    SELECT RAISE(ABORT,'scheduled delivery receipts are retained');
END;

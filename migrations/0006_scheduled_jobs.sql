CREATE TABLE scheduled_jobs (
    id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL CHECK (length(CAST(name AS BLOB)) BETWEEN 1 AND 256),
    definition TEXT NOT NULL CHECK (json_valid(definition) AND length(CAST(definition AS BLOB)) <= 131072),
    generation INTEGER NOT NULL CHECK (generation > 0),
    revision INTEGER NOT NULL CHECK (revision > 0),
    enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    next_run_at INTEGER CHECK (next_run_at IS NULL OR next_run_at >= 0),
    last_run_at INTEGER CHECK (last_run_at IS NULL OR last_run_at >= 0),
    created_at INTEGER NOT NULL CHECK (created_at >= 0),
    updated_at INTEGER NOT NULL CHECK (updated_at >= created_at),
    deleted_at INTEGER CHECK (deleted_at IS NULL OR deleted_at >= created_at),
    CHECK (deleted_at IS NULL OR (enabled = 0 AND next_run_at IS NULL)),
    CHECK (last_run_at IS NULL OR last_run_at <= updated_at)
) STRICT;
CREATE INDEX scheduled_jobs_due ON scheduled_jobs(next_run_at, id) WHERE enabled = 1 AND deleted_at IS NULL AND next_run_at IS NOT NULL;
CREATE TABLE scheduled_runs (
    occurrence_id TEXT PRIMARY KEY NOT NULL,
    job_id TEXT NOT NULL REFERENCES scheduled_jobs(id),
    generation INTEGER NOT NULL CHECK (generation > 0),
    definition TEXT NOT NULL CHECK (json_valid(definition) AND length(CAST(definition AS BLOB)) <= 131072),
    scheduled_for INTEGER NOT NULL CHECK (scheduled_for >= 0),
    trigger TEXT NOT NULL CHECK (trigger IN ('scheduled', 'manual')),
    manual_command_id TEXT UNIQUE,
    initial_task_id TEXT NOT NULL UNIQUE,
    claim_digest TEXT NOT NULL CHECK (length(claim_digest) = 64),
    state TEXT NOT NULL CHECK (state IN ('claimed', 'running', 'succeeded', 'failed', 'cancelled', 'skipped', 'unknown')),
    revision INTEGER NOT NULL CHECK (revision > 0),
    claimed_at INTEGER NOT NULL CHECK (claimed_at >= scheduled_for),
    started_at INTEGER CHECK (started_at IS NULL OR started_at >= claimed_at),
    finished_at INTEGER CHECK (finished_at IS NULL OR finished_at >= claimed_at),
    updated_at INTEGER NOT NULL CHECK (updated_at >= claimed_at),
    outcome TEXT CHECK (outcome IS NULL OR (json_valid(outcome) AND length(CAST(outcome AS BLOB)) <= 32768)),
    CHECK ((trigger = 'manual') = (manual_command_id IS NOT NULL)),
    CHECK ((state IN ('claimed', 'running') AND finished_at IS NULL AND outcome IS NULL) OR (state NOT IN ('claimed', 'running') AND finished_at IS NOT NULL AND outcome IS NOT NULL)),
    CHECK (state != 'claimed' OR started_at IS NULL),
    CHECK (state != 'running' OR started_at IS NOT NULL),
    CHECK (started_at IS NULL OR started_at <= updated_at),
    CHECK (finished_at IS NULL OR finished_at <= updated_at),
    CHECK (started_at IS NULL OR finished_at IS NULL OR finished_at >= started_at)
) STRICT;
CREATE UNIQUE INDEX scheduled_runs_occurrence ON scheduled_runs(job_id, generation, scheduled_for) WHERE trigger = 'scheduled';
CREATE INDEX scheduled_runs_history ON scheduled_runs(job_id, claimed_at DESC, occurrence_id DESC);
CREATE INDEX scheduled_runs_overlap ON scheduled_runs(job_id, state) WHERE state IN ('claimed', 'running', 'unknown');
CREATE TRIGGER scheduling_run_identity BEFORE UPDATE OF occurrence_id, job_id, generation, definition, scheduled_for, trigger, manual_command_id, initial_task_id, claim_digest, claimed_at ON scheduled_runs BEGIN
    SELECT RAISE(ABORT, 'scheduled occurrence identity and definition are immutable');
END;
CREATE TABLE scheduling_usage (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    jobs INTEGER NOT NULL CHECK (jobs BETWEEN 0 AND 4096),
    job_bytes INTEGER NOT NULL CHECK (job_bytes BETWEEN 0 AND 67108864),
    runs INTEGER NOT NULL CHECK (runs BETWEEN 0 AND 100000),
    history_bytes INTEGER NOT NULL CHECK (history_bytes BETWEEN 0 AND 268435456),
    active_runs INTEGER NOT NULL CHECK (active_runs >= 0 AND active_runs <= runs)
) STRICT;
INSERT INTO scheduling_usage VALUES (1, 0, 0, 0, 0, 0);
CREATE TRIGGER scheduling_job_insert AFTER INSERT ON scheduled_jobs BEGIN
    UPDATE scheduling_usage SET jobs = jobs + 1, job_bytes = job_bytes + length(CAST(NEW.definition AS BLOB)) WHERE singleton = 1;
END;
CREATE TRIGGER scheduling_job_update AFTER UPDATE OF definition ON scheduled_jobs BEGIN
    UPDATE scheduling_usage SET job_bytes = job_bytes - length(CAST(OLD.definition AS BLOB)) + length(CAST(NEW.definition AS BLOB)) WHERE singleton = 1;
END;
CREATE TRIGGER scheduling_job_delete AFTER DELETE ON scheduled_jobs BEGIN
    UPDATE scheduling_usage SET jobs = jobs - 1, job_bytes = job_bytes - length(CAST(OLD.definition AS BLOB)) WHERE singleton = 1;
END;
CREATE TRIGGER scheduling_run_insert AFTER INSERT ON scheduled_runs BEGIN
    UPDATE scheduling_usage SET runs = runs + 1, history_bytes = history_bytes + length(CAST(NEW.definition AS BLOB)) + coalesce(length(CAST(NEW.outcome AS BLOB)), 0), active_runs = active_runs + (NEW.state IN ('claimed', 'running')) WHERE singleton = 1;
END;
CREATE TRIGGER scheduling_run_update AFTER UPDATE OF state, outcome ON scheduled_runs BEGIN
    UPDATE scheduling_usage SET history_bytes = history_bytes - coalesce(length(CAST(OLD.outcome AS BLOB)), 0) + coalesce(length(CAST(NEW.outcome AS BLOB)), 0), active_runs = active_runs - (OLD.state IN ('claimed', 'running')) + (NEW.state IN ('claimed', 'running')) WHERE singleton = 1;
END;
CREATE TRIGGER scheduling_run_delete AFTER DELETE ON scheduled_runs BEGIN
    UPDATE scheduling_usage SET runs = runs - 1, history_bytes = history_bytes - length(CAST(OLD.definition AS BLOB)) - coalesce(length(CAST(OLD.outcome AS BLOB)), 0), active_runs = active_runs - (OLD.state IN ('claimed', 'running')) WHERE singleton = 1;
END;

-- Durable Team Work Kernel tables.
CREATE TABLE team_work_tasks (
    id TEXT PRIMARY KEY NOT NULL,
    team_id TEXT NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    status TEXT NOT NULL,
    version INTEGER NOT NULL,
    blocked_by TEXT NOT NULL DEFAULT '[]',
    payload TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE INDEX idx_team_work_tasks_team_status
    ON team_work_tasks(team_id, status, updated_at);

CREATE TABLE team_work_runs (
    id TEXT PRIMARY KEY NOT NULL,
    team_id TEXT NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    task_id TEXT NOT NULL REFERENCES team_work_tasks(id) ON DELETE CASCADE,
    attempt INTEGER NOT NULL,
    status TEXT NOT NULL,
    payload TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE(task_id, attempt)
);

CREATE INDEX idx_team_work_runs_team_task
    ON team_work_runs(team_id, task_id, attempt);

CREATE TABLE team_work_events (
    sequence INTEGER NOT NULL,
    event_id TEXT NOT NULL UNIQUE,
    team_id TEXT NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    task_id TEXT NOT NULL REFERENCES team_work_tasks(id) ON DELETE CASCADE,
    run_id TEXT,
    name TEXT NOT NULL,
    task_version INTEGER NOT NULL,
    payload TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY(team_id, sequence)
);

CREATE INDEX idx_team_work_events_team_sequence
    ON team_work_events(team_id, sequence);

CREATE TABLE team_work_commands (
    team_id TEXT NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    task_id TEXT NOT NULL REFERENCES team_work_tasks(id) ON DELETE CASCADE,
    idempotency_key TEXT NOT NULL,
    envelope TEXT NOT NULL,
    receipt TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY(team_id, task_id, idempotency_key)
);

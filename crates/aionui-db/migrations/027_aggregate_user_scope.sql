-- Enforce aggregate parent chains for new writes without rewriting existing tables.

CREATE TRIGGER IF NOT EXISTS trg_mailbox_team_parent_insert
BEFORE INSERT ON mailbox
FOR EACH ROW
WHEN NOT EXISTS (SELECT 1 FROM teams WHERE id = NEW.team_id)
BEGIN
    SELECT RAISE(ABORT, 'mailbox.team_id must reference teams.id');
END;

CREATE TRIGGER IF NOT EXISTS trg_mailbox_team_parent_update
BEFORE UPDATE OF team_id ON mailbox
FOR EACH ROW
WHEN NOT EXISTS (SELECT 1 FROM teams WHERE id = NEW.team_id)
BEGIN
    SELECT RAISE(ABORT, 'mailbox.team_id must reference teams.id');
END;

CREATE TRIGGER IF NOT EXISTS trg_team_tasks_team_parent_insert
BEFORE INSERT ON team_tasks
FOR EACH ROW
WHEN NOT EXISTS (SELECT 1 FROM teams WHERE id = NEW.team_id)
BEGIN
    SELECT RAISE(ABORT, 'team_tasks.team_id must reference teams.id');
END;

CREATE TRIGGER IF NOT EXISTS trg_team_tasks_team_parent_update
BEFORE UPDATE OF team_id ON team_tasks
FOR EACH ROW
WHEN NOT EXISTS (SELECT 1 FROM teams WHERE id = NEW.team_id)
BEGIN
    SELECT RAISE(ABORT, 'team_tasks.team_id must reference teams.id');
END;

CREATE TRIGGER IF NOT EXISTS trg_cron_jobs_conversation_parent_insert
BEFORE INSERT ON cron_jobs
FOR EACH ROW
WHEN NEW.conversation_id IS NULL
  OR NEW.conversation_id = ''
  OR NOT EXISTS (SELECT 1 FROM conversations WHERE id = NEW.conversation_id)
BEGIN
    SELECT RAISE(ABORT, 'cron_jobs.conversation_id must reference conversations.id');
END;

CREATE TRIGGER IF NOT EXISTS trg_cron_jobs_conversation_parent_update
BEFORE UPDATE OF conversation_id ON cron_jobs
FOR EACH ROW
WHEN NEW.conversation_id IS NULL
  OR NEW.conversation_id = ''
  OR NOT EXISTS (SELECT 1 FROM conversations WHERE id = NEW.conversation_id)
BEGIN
    SELECT RAISE(ABORT, 'cron_jobs.conversation_id must reference conversations.id');
END;

CREATE TRIGGER IF NOT EXISTS trg_cron_job_runs_job_parent_insert
BEFORE INSERT ON cron_job_runs
FOR EACH ROW
WHEN NOT EXISTS (
    SELECT 1
    FROM cron_jobs j
    JOIN conversations c ON c.id = j.conversation_id
    WHERE j.id = NEW.job_id
)
BEGIN
    SELECT RAISE(ABORT, 'cron_job_runs.job_id must reference cron_jobs.id with conversation parent');
END;

CREATE TRIGGER IF NOT EXISTS trg_cron_job_runs_job_parent_update
BEFORE UPDATE OF job_id ON cron_job_runs
FOR EACH ROW
WHEN NOT EXISTS (
    SELECT 1
    FROM cron_jobs j
    JOIN conversations c ON c.id = j.conversation_id
    WHERE j.id = NEW.job_id
)
BEGIN
    SELECT RAISE(ABORT, 'cron_job_runs.job_id must reference cron_jobs.id with conversation parent');
END;

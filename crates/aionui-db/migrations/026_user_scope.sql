-- Migration 026: add user scope for local, aggregate, and configuration data.

PRAGMA foreign_keys = OFF;

CREATE TABLE users_new (
    id                 TEXT PRIMARY KEY NOT NULL,
    user_type          TEXT NOT NULL DEFAULT 'local'
                           CHECK(user_type IN ('local', 'aionpro')),
    external_user_id   TEXT,
    username           TEXT,
    email              TEXT,
    password_hash      TEXT,
    avatar_path        TEXT,
    jwt_secret         TEXT,
    status             TEXT NOT NULL DEFAULT 'active'
                           CHECK(status IN ('active', 'disabled')),
    session_generation INTEGER NOT NULL DEFAULT 0,
    created_at         INTEGER NOT NULL,
    updated_at         INTEGER NOT NULL,
    last_login         INTEGER,
    CHECK (
        (user_type = 'local' AND password_hash IS NOT NULL)
        OR
        (user_type = 'aionpro')
    ),
    CHECK (
        (external_user_id IS NULL)
        OR
        (length(external_user_id) > 0)
    )
);

INSERT INTO users_new (
    id,
    user_type,
    external_user_id,
    username,
    email,
    password_hash,
    avatar_path,
    jwt_secret,
    status,
    session_generation,
    created_at,
    updated_at,
    last_login
)
SELECT
    id,
    'local',
    NULL,
    username,
    email,
    password_hash,
    avatar_path,
    jwt_secret,
    'active',
    0,
    created_at,
    updated_at,
    last_login
FROM users;

CREATE TEMP TABLE user_scope_migration_checks (
    ok INTEGER NOT NULL CHECK (ok = 1)
);

INSERT INTO user_scope_migration_checks (ok)
SELECT CASE
    WHEN (SELECT COUNT(*) FROM users_new) = (SELECT COUNT(*) FROM users)
    THEN 1
    ELSE 0
END;

DROP TABLE user_scope_migration_checks;

DROP TABLE users;
ALTER TABLE users_new RENAME TO users;

CREATE UNIQUE INDEX idx_users_local_username
    ON users(username)
    WHERE user_type = 'local' AND username IS NOT NULL;
CREATE UNIQUE INDEX idx_users_email
    ON users(email)
    WHERE email IS NOT NULL;
CREATE UNIQUE INDEX idx_users_external_user
    ON users(user_type, external_user_id)
    WHERE external_user_id IS NOT NULL;
CREATE INDEX idx_users_username ON users(username);
CREATE INDEX idx_users_status ON users(status);

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

ALTER TABLE providers
    ADD COLUMN user_id TEXT NOT NULL DEFAULT 'system_default_user';
CREATE INDEX IF NOT EXISTS idx_providers_user_platform ON providers(user_id, platform);

ALTER TABLE remote_agents
    ADD COLUMN user_id TEXT NOT NULL DEFAULT 'system_default_user';
CREATE INDEX IF NOT EXISTS idx_remote_agents_user_status ON remote_agents(user_id, status);

CREATE TABLE mcp_servers_new (
    id               TEXT PRIMARY KEY NOT NULL,
    user_id          TEXT    NOT NULL DEFAULT 'system_default_user' REFERENCES users(id),
    name             TEXT    NOT NULL,
    description      TEXT,
    enabled          INTEGER NOT NULL DEFAULT 0,
    transport_type   TEXT    NOT NULL,
    transport_config TEXT    NOT NULL,
    tools            TEXT,
    last_test_status TEXT    NOT NULL DEFAULT 'disconnected',
    last_connected   INTEGER,
    original_json    TEXT,
    builtin          INTEGER NOT NULL DEFAULT 0,
    deleted_at       INTEGER,
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL,
    UNIQUE (user_id, name)
);

INSERT INTO mcp_servers_new (
    id, user_id, name, description, enabled, transport_type, transport_config,
    tools, last_test_status, last_connected, original_json, builtin, deleted_at,
    created_at, updated_at
)
SELECT
    id, 'system_default_user', name, description, enabled, transport_type,
    transport_config, tools, last_test_status, last_connected, original_json,
    builtin, deleted_at, created_at, updated_at
FROM mcp_servers;

DROP TABLE mcp_servers;
ALTER TABLE mcp_servers_new RENAME TO mcp_servers;
CREATE INDEX IF NOT EXISTS idx_mcp_servers_user_name ON mcp_servers(user_id, name);
CREATE INDEX IF NOT EXISTS idx_mcp_servers_user_enabled ON mcp_servers(user_id, enabled);
CREATE INDEX IF NOT EXISTS idx_mcp_servers_deleted_at ON mcp_servers(deleted_at);

CREATE TABLE oauth_tokens_new (
    user_id       TEXT NOT NULL DEFAULT 'system_default_user' REFERENCES users(id),
    server_url    TEXT NOT NULL,
    access_token  TEXT NOT NULL,
    refresh_token TEXT,
    token_type    TEXT NOT NULL DEFAULT 'bearer',
    expires_at    INTEGER,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    PRIMARY KEY (user_id, server_url)
);

INSERT INTO oauth_tokens_new (
    user_id, server_url, access_token, refresh_token, token_type, expires_at,
    created_at, updated_at
)
SELECT
    'system_default_user', server_url, access_token, refresh_token, token_type,
    expires_at, created_at, updated_at
FROM oauth_tokens;

DROP TABLE oauth_tokens;
ALTER TABLE oauth_tokens_new RENAME TO oauth_tokens;

CREATE TABLE system_settings_new (
    user_id                   TEXT PRIMARY KEY NOT NULL REFERENCES users(id),
    language                  TEXT    NOT NULL DEFAULT 'en-US',
    notification_enabled      INTEGER NOT NULL DEFAULT 1,
    cron_notification_enabled INTEGER NOT NULL DEFAULT 0,
    command_queue_enabled     INTEGER NOT NULL DEFAULT 0,
    save_upload_to_workspace  INTEGER NOT NULL DEFAULT 0,
    updated_at                INTEGER NOT NULL
);

INSERT INTO system_settings_new (
    user_id, language, notification_enabled, cron_notification_enabled,
    command_queue_enabled, save_upload_to_workspace, updated_at
)
SELECT
    'system_default_user', language, notification_enabled,
    cron_notification_enabled, command_queue_enabled, save_upload_to_workspace,
    updated_at
FROM system_settings
WHERE id = 1;

DROP TABLE system_settings;
ALTER TABLE system_settings_new RENAME TO system_settings;

CREATE TABLE client_preferences_new (
    user_id    TEXT NOT NULL DEFAULT 'system_default_user' REFERENCES users(id),
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, key)
);

INSERT INTO client_preferences_new (user_id, key, value, updated_at)
SELECT 'system_default_user', key, value, updated_at
FROM client_preferences;

DROP TABLE client_preferences;
ALTER TABLE client_preferences_new RENAME TO client_preferences;

ALTER TABLE agent_metadata
    ADD COLUMN user_id TEXT REFERENCES users(id);
CREATE INDEX IF NOT EXISTS idx_agent_metadata_user_sort
    ON agent_metadata(user_id, sort_order, name);

ALTER TABLE assistants
    ADD COLUMN user_id TEXT REFERENCES users(id);
UPDATE assistants
SET user_id = 'system_default_user'
WHERE user_id IS NULL;
CREATE INDEX IF NOT EXISTS idx_assistants_user_updated_at
    ON assistants(user_id, updated_at DESC);

CREATE TABLE assistant_overrides_new (
    user_id           TEXT NOT NULL DEFAULT 'system_default_user' REFERENCES users(id),
    assistant_id      TEXT NOT NULL,
    enabled           INTEGER NOT NULL DEFAULT 1,
    sort_order        INTEGER NOT NULL DEFAULT 0,
    last_used_at      INTEGER,
    updated_at        INTEGER NOT NULL,
    PRIMARY KEY (user_id, assistant_id)
);

INSERT INTO assistant_overrides_new (
    user_id, assistant_id, enabled, sort_order, last_used_at, updated_at
)
SELECT
    'system_default_user', assistant_id, enabled, sort_order, last_used_at, updated_at
FROM assistant_overrides;

DROP TABLE assistant_overrides;
ALTER TABLE assistant_overrides_new RENAME TO assistant_overrides;
CREATE INDEX IF NOT EXISTS idx_assistant_overrides_user_sort
    ON assistant_overrides(user_id, sort_order);

ALTER TABLE assistant_definitions
    ADD COLUMN user_id TEXT REFERENCES users(id);
UPDATE assistant_definitions
SET user_id = 'system_default_user'
WHERE user_id IS NULL
  AND NOT (source = 'builtin' AND owner_type = 'system');
DROP INDEX IF EXISTS idx_assistant_definitions_source_ref;
DROP INDEX IF EXISTS idx_assistant_definitions_assistant_id;
CREATE UNIQUE INDEX IF NOT EXISTS idx_assistant_definitions_global_source_ref
    ON assistant_definitions(source, source_ref)
    WHERE user_id IS NULL AND source_ref IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_assistant_definitions_user_source_ref
    ON assistant_definitions(user_id, source, source_ref)
    WHERE user_id IS NOT NULL AND source_ref IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_assistant_definitions_global_assistant_id
    ON assistant_definitions(assistant_id)
    WHERE user_id IS NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_assistant_definitions_user_assistant_id
    ON assistant_definitions(user_id, assistant_id)
    WHERE user_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_assistant_definitions_user_updated_at
    ON assistant_definitions(user_id, updated_at DESC);

CREATE TABLE assistant_overlays_new (
    user_id                 TEXT NOT NULL DEFAULT 'system_default_user' REFERENCES users(id),
    assistant_definition_id TEXT NOT NULL,
    enabled                 INTEGER NOT NULL DEFAULT 1,
    sort_order              INTEGER NOT NULL DEFAULT 0,
    agent_id_override       TEXT,
    last_used_at            INTEGER,
    created_at              INTEGER NOT NULL,
    updated_at              INTEGER NOT NULL,
    PRIMARY KEY (user_id, assistant_definition_id),
    FOREIGN KEY (assistant_definition_id) REFERENCES assistant_definitions(id) ON DELETE CASCADE
);

INSERT INTO assistant_overlays_new (
    user_id, assistant_definition_id, enabled, sort_order, agent_id_override,
    last_used_at, created_at, updated_at
)
SELECT
    'system_default_user', assistant_definition_id, enabled, sort_order,
    agent_id_override, last_used_at, created_at, updated_at
FROM assistant_overlays;

DROP TABLE assistant_overlays;
ALTER TABLE assistant_overlays_new RENAME TO assistant_overlays;
CREATE INDEX IF NOT EXISTS idx_assistant_overlays_user_enabled
    ON assistant_overlays(user_id, enabled);
CREATE INDEX IF NOT EXISTS idx_assistant_overlays_user_sort_order
    ON assistant_overlays(user_id, sort_order);
CREATE INDEX IF NOT EXISTS idx_assistant_overlays_agent_id_override
    ON assistant_overlays(agent_id_override)
    WHERE agent_id_override IS NOT NULL;

CREATE TABLE assistant_preferences_new (
    user_id                              TEXT NOT NULL DEFAULT 'system_default_user' REFERENCES users(id),
    assistant_definition_id              TEXT NOT NULL,
    last_model_id                        TEXT,
    last_permission_value                TEXT,
    last_skill_ids                       TEXT    NOT NULL DEFAULT '[]',
    last_disabled_builtin_skill_ids      TEXT    NOT NULL DEFAULT '[]',
    last_mcp_ids                         TEXT    NOT NULL DEFAULT '[]',
    created_at                           INTEGER NOT NULL,
    updated_at                           INTEGER NOT NULL,
    last_thought_level_value             TEXT,
    PRIMARY KEY (user_id, assistant_definition_id),
    FOREIGN KEY (assistant_definition_id) REFERENCES assistant_definitions(id) ON DELETE CASCADE
);

INSERT INTO assistant_preferences_new (
    user_id, assistant_definition_id, last_model_id, last_permission_value,
    last_skill_ids, last_disabled_builtin_skill_ids, last_mcp_ids,
    created_at, updated_at, last_thought_level_value
)
SELECT
    'system_default_user', assistant_definition_id, last_model_id, last_permission_value,
    last_skill_ids, last_disabled_builtin_skill_ids, last_mcp_ids,
    created_at, updated_at, last_thought_level_value
FROM assistant_preferences;

DROP TABLE assistant_preferences;
ALTER TABLE assistant_preferences_new RENAME TO assistant_preferences;

CREATE TABLE skills_new (
    id          TEXT    PRIMARY KEY NOT NULL,
    user_id     TEXT    REFERENCES users(id),
    name        TEXT    NOT NULL,
    description TEXT,
    path        TEXT    NOT NULL,
    source      TEXT    NOT NULL DEFAULT 'user'
                            CHECK (source IN ('user', 'builtin', 'extension', 'cron')),
    enabled     INTEGER NOT NULL DEFAULT 1,
    deleted_at  INTEGER,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

INSERT INTO skills_new (
    id, user_id, name, description, path, source, enabled, deleted_at, created_at, updated_at
)
SELECT
    id,
    CASE WHEN source = 'builtin' THEN NULL ELSE 'system_default_user' END,
    name, description, path, source, enabled, deleted_at, created_at, updated_at
FROM skills;

DROP TABLE skills;
ALTER TABLE skills_new RENAME TO skills;
CREATE UNIQUE INDEX IF NOT EXISTS idx_skills_global_name
    ON skills(name)
    WHERE user_id IS NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_skills_user_name
    ON skills(user_id, name)
    WHERE user_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_skills_user_deleted_at ON skills(user_id, deleted_at);
CREATE INDEX IF NOT EXISTS idx_skills_source ON skills(source);
CREATE INDEX IF NOT EXISTS idx_skills_updated_at ON skills(updated_at DESC);

ALTER TABLE skill_import_records
    ADD COLUMN user_id TEXT NOT NULL DEFAULT 'system_default_user' REFERENCES users(id);
CREATE INDEX IF NOT EXISTS idx_skill_import_records_user_created_at
    ON skill_import_records(user_id, created_at DESC);

ALTER TABLE assistant_plugins
    ADD COLUMN owner_user_id TEXT NOT NULL DEFAULT 'system_default_user' REFERENCES users(id);
CREATE INDEX IF NOT EXISTS idx_assistant_plugins_owner_created_at
    ON assistant_plugins(owner_user_id, created_at ASC);

CREATE TABLE assistant_users_new (
    id               TEXT PRIMARY KEY NOT NULL,
    owner_user_id    TEXT NOT NULL DEFAULT 'system_default_user' REFERENCES users(id),
    platform_user_id TEXT NOT NULL,
    platform_type    TEXT NOT NULL,
    display_name     TEXT,
    authorized_at    INTEGER NOT NULL,
    last_active      INTEGER,
    session_id       TEXT,
    UNIQUE (owner_user_id, platform_user_id, platform_type)
);

INSERT INTO assistant_users_new (
    id, owner_user_id, platform_user_id, platform_type, display_name,
    authorized_at, last_active, session_id
)
SELECT
    id, 'system_default_user', platform_user_id, platform_type, display_name,
    authorized_at, last_active, session_id
FROM assistant_users;

DROP TABLE assistant_users;
ALTER TABLE assistant_users_new RENAME TO assistant_users;
CREATE INDEX IF NOT EXISTS idx_assistant_users_owner_authorized_at
    ON assistant_users(owner_user_id, authorized_at DESC);

ALTER TABLE assistant_pairing_codes
    ADD COLUMN owner_user_id TEXT NOT NULL DEFAULT 'system_default_user' REFERENCES users(id);
CREATE INDEX IF NOT EXISTS idx_pairing_codes_owner_status
    ON assistant_pairing_codes(owner_user_id, status);

PRAGMA foreign_keys = ON;

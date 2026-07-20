-- Migration 028: scope independent configuration roots by user.

PRAGMA foreign_keys = OFF;

CREATE TABLE providers_new (
    id                TEXT PRIMARY KEY NOT NULL,
    user_id           TEXT    NOT NULL DEFAULT 'system_default_user' REFERENCES users(id),
    platform          TEXT    NOT NULL,
    name              TEXT    NOT NULL,
    base_url          TEXT    NOT NULL,
    api_key_encrypted TEXT    NOT NULL,
    models            TEXT    NOT NULL DEFAULT '[]',
    enabled           INTEGER NOT NULL DEFAULT 1,
    capabilities      TEXT    NOT NULL DEFAULT '[]',
    context_limit     INTEGER,
    model_protocols   TEXT,
    model_enabled     TEXT,
    model_health      TEXT,
    bedrock_config    TEXT,
    is_full_url       INTEGER NOT NULL DEFAULT 0,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL
);

INSERT INTO providers_new (
    id, user_id, platform, name, base_url, api_key_encrypted, models, enabled,
    capabilities, context_limit, model_protocols, model_enabled, model_health,
    bedrock_config, is_full_url, created_at, updated_at
)
SELECT
    id, 'system_default_user', platform, name, base_url, api_key_encrypted,
    models, enabled, capabilities, context_limit, model_protocols,
    model_enabled, model_health, bedrock_config, is_full_url, created_at,
    updated_at
FROM providers;

DROP TABLE providers;
ALTER TABLE providers_new RENAME TO providers;
CREATE INDEX IF NOT EXISTS idx_providers_user_platform ON providers(user_id, platform);

CREATE TABLE remote_agents_new (
    id                 TEXT PRIMARY KEY NOT NULL,
    user_id            TEXT    NOT NULL DEFAULT 'system_default_user' REFERENCES users(id),
    name               TEXT    NOT NULL,
    protocol           TEXT    NOT NULL,
    url                TEXT    NOT NULL,
    auth_type          TEXT    NOT NULL,
    auth_token         TEXT,
    allow_insecure     INTEGER NOT NULL DEFAULT 0,
    avatar             TEXT,
    description        TEXT,
    device_id          TEXT,
    device_public_key  TEXT,
    device_private_key TEXT,
    device_token       TEXT,
    status             TEXT    NOT NULL DEFAULT 'unknown',
    last_connected_at  INTEGER,
    created_at         INTEGER NOT NULL,
    updated_at         INTEGER NOT NULL
);

INSERT INTO remote_agents_new (
    id, user_id, name, protocol, url, auth_type, auth_token, allow_insecure,
    avatar, description, device_id, device_public_key, device_private_key,
    device_token, status, last_connected_at, created_at, updated_at
)
SELECT
    id, 'system_default_user', name, protocol, url, auth_type, auth_token,
    allow_insecure, avatar, description, device_id, device_public_key,
    device_private_key, device_token, status, last_connected_at, created_at,
    updated_at
FROM remote_agents;

DROP TABLE remote_agents;
ALTER TABLE remote_agents_new RENAME TO remote_agents;
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

PRAGMA foreign_keys = ON;

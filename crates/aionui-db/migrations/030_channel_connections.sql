-- Migration 030: Channel connection entity (channel refactor, segment 1 of 3).
--
-- Replaces `assistant_plugins` with `channel_connections`, decoupling the
-- connection instance from the platform type (07-16 §5.2 via the 2026-07-27
-- split plan, task A1):
--
--   * `id` becomes a generated, meaning-free connection id (the legacy rows
--     used the platform type itself as the id);
--   * the platform type moves to `plugin_key`;
--   * `PRIMARY KEY (owner_user_id, id)` stays the composite identity that
--     later segments' composite foreign keys will reference;
--   * phase 1 keeps exactly one instance per (owner, plugin_key) via a
--     unique index — multi-instance is a later product decision, at which
--     point that index is dropped.
--
-- The legacy platform-type id remains recoverable as `plugin_key`, which is
-- how segment 2 backfills `channel_users.connection_id`.

CREATE TABLE IF NOT EXISTS channel_connections (
    id             TEXT    NOT NULL,
    owner_user_id  TEXT    NOT NULL DEFAULT 'system_default_user' REFERENCES users(id),
    plugin_key     TEXT    NOT NULL,
    name           TEXT    NOT NULL,
    enabled        INTEGER NOT NULL DEFAULT 0,
    config         TEXT    NOT NULL,
    status         TEXT,
    last_connected INTEGER,
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL,
    PRIMARY KEY (owner_user_id, id)
);

INSERT INTO channel_connections (
    id, owner_user_id, plugin_key, name, enabled, config, status,
    last_connected, created_at, updated_at
)
SELECT
    'conn_' || lower(hex(randomblob(16))),
    owner_user_id,
    id,
    name,
    enabled,
    config,
    status,
    last_connected,
    created_at,
    updated_at
FROM assistant_plugins;

-- Migration-fatal integrity checks (user_scope_rebuild_checks pattern):
-- inserting `ok = 0` violates the CHECK and aborts the migration.
CREATE TEMPORARY TABLE channel_refactor_checks (
    ok INTEGER NOT NULL CHECK (ok = 1)
);

-- Row conservation: every legacy plugin row became exactly one connection.
INSERT INTO channel_refactor_checks (ok)
SELECT CASE
    WHEN (SELECT COUNT(*) FROM channel_connections) = (SELECT COUNT(*) FROM assistant_plugins)
    THEN 1
    ELSE 0
END;

-- Legacy identity preserved: each (owner, legacy id) is now (owner, plugin_key).
INSERT INTO channel_refactor_checks (ok)
SELECT CASE
    WHEN NOT EXISTS (
        SELECT 1 FROM assistant_plugins p
        WHERE NOT EXISTS (
            SELECT 1 FROM channel_connections c
            WHERE c.owner_user_id = p.owner_user_id AND c.plugin_key = p.id
        )
    )
    THEN 1
    ELSE 0
END;

DROP TABLE assistant_plugins;
DROP TABLE channel_refactor_checks;

-- Phase 1: one connection per (owner, plugin_key). Dropped when multi-instance
-- lands as a product feature.
CREATE UNIQUE INDEX IF NOT EXISTS idx_channel_connections_single_instance
    ON channel_connections(owner_user_id, plugin_key);
CREATE INDEX IF NOT EXISTS idx_channel_connections_owner_created_at
    ON channel_connections(owner_user_id, created_at ASC);

-- ---------------------------------------------------------------------------
-- Segment 2: channel_users + channel_pairing_requests (channel refactor A2).
--
--   * `assistant_users` → `channel_users`: rows attach to their connection
--     (composite FK), the platform column disappears (derived from the
--     connection), `platform_user_id` becomes `external_user_id`, and
--     revocation becomes a soft delete (`status` = active|revoked with
--     `revoked_at`) so authorization history survives for audit.
--     The legacy `session_id` column is dropped (no stable semantics).
--   * `assistant_pairing_codes` → `channel_pairing_requests`: pairing rows
--     get a surrogate id, attach to their connection, and store only a
--     server-side HMAC of the code (`code_hash`) — the plaintext code never
--     touches the database. Legacy rows are NOT migrated: pairing codes are
--     10-minute artifacts whose plaintext cannot (and must not) be hashed
--     retroactively, and historical rows carry no runtime state.
--   * `conversations` gains UNIQUE(user_id, id) — the shared foundation for
--     composite cross-account foreign keys (07-16 §5.4), used by segment 3.
--
-- Legacy assistant_users rows whose platform has no connection row (the
-- plugin row was deleted after users were authorized) get a synthesized
-- disabled connection so no authorization silently disappears.
-- ---------------------------------------------------------------------------

INSERT INTO channel_connections (
    id, owner_user_id, plugin_key, name, enabled, config, created_at, updated_at
)
SELECT
    'conn_' || lower(hex(randomblob(16))),
    u.owner_user_id,
    u.platform_type,
    u.platform_type || ' (recovered)',
    0,
    '',
    strftime('%s', 'now') * 1000,
    strftime('%s', 'now') * 1000
FROM (
    SELECT DISTINCT owner_user_id, platform_type FROM assistant_users
) u
WHERE NOT EXISTS (
    SELECT 1 FROM channel_connections c
    WHERE c.owner_user_id = u.owner_user_id AND c.plugin_key = u.platform_type
);

CREATE TABLE channel_users (
    id               TEXT    PRIMARY KEY NOT NULL,
    owner_user_id    TEXT    NOT NULL REFERENCES users(id),
    connection_id    TEXT    NOT NULL,
    external_user_id TEXT    NOT NULL,
    display_name     TEXT,
    status           TEXT    NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'revoked')),
    revoked_at       INTEGER,
    authorized_at    INTEGER NOT NULL,
    last_active      INTEGER,
    FOREIGN KEY (owner_user_id, connection_id) REFERENCES channel_connections(owner_user_id, id),
    UNIQUE (owner_user_id, connection_id, external_user_id)
);

INSERT INTO channel_users (
    id, owner_user_id, connection_id, external_user_id, display_name,
    status, revoked_at, authorized_at, last_active
)
SELECT
    u.id, u.owner_user_id, c.id, u.platform_user_id, u.display_name,
    'active', NULL, u.authorized_at, u.last_active
FROM assistant_users u
JOIN channel_connections c
    ON c.owner_user_id = u.owner_user_id AND c.plugin_key = u.platform_type;

CREATE TEMPORARY TABLE channel_refactor_checks_2 (
    ok INTEGER NOT NULL CHECK (ok = 1)
);

-- Row conservation: every authorized user survived the rebuild.
INSERT INTO channel_refactor_checks_2 (ok)
SELECT CASE
    WHEN (SELECT COUNT(*) FROM channel_users) = (SELECT COUNT(*) FROM assistant_users)
    THEN 1
    ELSE 0
END;

-- Rebuild assistant_sessions so its user FK follows the renamed parent.
-- Shape is unchanged here; segment 3 reshapes it into
-- channel_conversation_bindings.
CREATE TABLE assistant_sessions_new (
    id              TEXT PRIMARY KEY NOT NULL,
    user_id         TEXT    NOT NULL REFERENCES channel_users(id) ON DELETE CASCADE,
    agent_type      TEXT    NOT NULL,
    conversation_id TEXT,
    workspace       TEXT,
    chat_id         TEXT,
    created_at      INTEGER NOT NULL,
    last_activity   INTEGER NOT NULL,
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE SET NULL
);

INSERT INTO assistant_sessions_new SELECT * FROM assistant_sessions;

INSERT INTO channel_refactor_checks_2 (ok)
SELECT CASE
    WHEN (SELECT COUNT(*) FROM assistant_sessions_new) = (SELECT COUNT(*) FROM assistant_sessions)
    THEN 1
    ELSE 0
END;

DROP TABLE assistant_sessions;
ALTER TABLE assistant_sessions_new RENAME TO assistant_sessions;
CREATE INDEX IF NOT EXISTS idx_assistant_sessions_user ON assistant_sessions(user_id);
CREATE INDEX IF NOT EXISTS idx_assistant_sessions_conversation ON assistant_sessions(conversation_id);

DROP TABLE assistant_users;
DROP TABLE channel_refactor_checks_2;

-- Pairing requests: hashed codes only; legacy 10-minute codes are dropped by
-- design (see segment header).
CREATE TABLE channel_pairing_requests (
    id                       TEXT    PRIMARY KEY NOT NULL,
    owner_user_id            TEXT    NOT NULL REFERENCES users(id),
    connection_id            TEXT    NOT NULL,
    external_user_id         TEXT    NOT NULL,
    display_name             TEXT,
    code_hash                TEXT    NOT NULL,
    status                   TEXT    NOT NULL DEFAULT 'pending'
                                     CHECK (status IN ('pending', 'approved', 'rejected', 'expired')),
    requested_at             INTEGER NOT NULL,
    expires_at               INTEGER NOT NULL,
    approved_channel_user_id TEXT    REFERENCES channel_users(id),
    FOREIGN KEY (owner_user_id, connection_id) REFERENCES channel_connections(owner_user_id, id)
);

DROP TABLE assistant_pairing_codes;

-- One pending request per (owner, connection, external user); one pending
-- request per (owner, code hash).
CREATE UNIQUE INDEX idx_channel_pairing_pending_user
    ON channel_pairing_requests(owner_user_id, connection_id, external_user_id)
    WHERE status = 'pending';
CREATE UNIQUE INDEX idx_channel_pairing_pending_hash
    ON channel_pairing_requests(owner_user_id, code_hash)
    WHERE status = 'pending';
CREATE INDEX idx_channel_pairing_owner_status_expiry
    ON channel_pairing_requests(owner_user_id, status, expires_at);

-- Shared foundation for composite cross-account FKs (07-16 §5.4).
CREATE UNIQUE INDEX IF NOT EXISTS idx_conversations_user_id_id
    ON conversations(user_id, id);

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

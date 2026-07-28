-- Migration 031: device/account scope for client_preferences (settings-dedup
-- B2, 2026-07-27 split plan; disposition table in
-- docs/superpowers/2026-07-27-settings-dedup-b1-inventory.md).
--
--   * Every preference row gets a `scope`: 'account' rows stay per-user;
--     'device' rows are machine-level (one value per machine, user_id NULL).
--   * Keys confirmed device-level by the B1 inventory are promoted to device
--     scope: `system.closeToTray`, `keepAwake`, `autoPreviewOfficeFiles`,
--     and `pet.*`. When multiple users had a copy, the most recently updated
--     value wins and the per-user copies are dropped (documented data loss —
--     these keys always described the machine, not the user).
--   * The four system_settings switches are materialized as normalized
--     account-scope keys (`system.notificationEnabled`,
--     `cron.notificationEnabled`, `system.commandQueueEnabled`,
--     `system.saveUploadToWorkspace`) so preferences become the single read
--     path; the legacy columns stay until B3 drops the table.

CREATE TABLE client_preferences_new (
    scope      TEXT    NOT NULL DEFAULT 'account' CHECK (scope IN ('device', 'account')),
    user_id    TEXT    REFERENCES users(id),
    key        TEXT    NOT NULL,
    value      TEXT    NOT NULL,
    updated_at INTEGER NOT NULL,
    CHECK (
        (scope = 'device' AND user_id IS NULL)
        OR (scope = 'account' AND user_id IS NOT NULL)
    )
);

INSERT INTO client_preferences_new (scope, user_id, key, value, updated_at)
SELECT 'account', user_id, key, value, updated_at
FROM client_preferences;

CREATE TEMPORARY TABLE client_preference_scope_checks (
    ok INTEGER NOT NULL CHECK (ok = 1)
);

INSERT INTO client_preference_scope_checks (ok)
SELECT CASE
    WHEN (SELECT COUNT(*) FROM client_preferences_new) = (SELECT COUNT(*) FROM client_preferences)
    THEN 1
    ELSE 0
END;

DROP TABLE client_preferences;
ALTER TABLE client_preferences_new RENAME TO client_preferences;

-- Scope-aware uniqueness: one device value per key, one account value per
-- (user, key).
CREATE UNIQUE INDEX idx_client_preferences_device_key
    ON client_preferences(key) WHERE scope = 'device';
CREATE UNIQUE INDEX idx_client_preferences_account_key
    ON client_preferences(user_id, key) WHERE scope = 'account';

-- Promote confirmed device-level keys: latest write wins across users.
INSERT INTO client_preferences (scope, user_id, key, value, updated_at)
SELECT 'device', NULL, key, value, updated_at
FROM (
    SELECT
        key, value, updated_at,
        ROW_NUMBER() OVER (PARTITION BY key ORDER BY updated_at DESC, user_id ASC) AS rn
    FROM client_preferences
    WHERE scope = 'account'
      AND (
          key IN ('system.closeToTray', 'keepAwake', 'autoPreviewOfficeFiles')
          OR key LIKE 'pet.%'
      )
)
WHERE rn = 1;

DELETE FROM client_preferences
WHERE scope = 'account'
  AND (
      key IN ('system.closeToTray', 'keepAwake', 'autoPreviewOfficeFiles')
      OR key LIKE 'pet.%'
  );

-- Materialize the system_settings switches as account-scope keys. INSERT OR
-- IGNORE: an existing preference row (e.g. written post-B1) is the newer
-- truth and must not be clobbered.
INSERT OR IGNORE INTO client_preferences (scope, user_id, key, value, updated_at)
SELECT 'account', s.user_id, 'system.notificationEnabled',
       CASE WHEN s.notification_enabled THEN 'true' ELSE 'false' END, s.updated_at
FROM system_settings s;

INSERT OR IGNORE INTO client_preferences (scope, user_id, key, value, updated_at)
SELECT 'account', s.user_id, 'cron.notificationEnabled',
       CASE WHEN s.cron_notification_enabled THEN 'true' ELSE 'false' END, s.updated_at
FROM system_settings s;

INSERT OR IGNORE INTO client_preferences (scope, user_id, key, value, updated_at)
SELECT 'account', s.user_id, 'system.commandQueueEnabled',
       CASE WHEN s.command_queue_enabled THEN 'true' ELSE 'false' END, s.updated_at
FROM system_settings s;

INSERT OR IGNORE INTO client_preferences (scope, user_id, key, value, updated_at)
SELECT 'account', s.user_id, 'system.saveUploadToWorkspace',
       CASE WHEN s.save_upload_to_workspace THEN 'true' ELSE 'false' END, s.updated_at
FROM system_settings s;

-- Migration validation: every migrated switch column is readable back as a
-- preference for every settings row.
INSERT INTO client_preference_scope_checks (ok)
SELECT CASE
    WHEN NOT EXISTS (
        SELECT 1 FROM system_settings s
        WHERE NOT EXISTS (
            SELECT 1 FROM client_preferences p
            WHERE p.scope = 'account' AND p.user_id = s.user_id
              AND p.key = 'system.notificationEnabled'
        )
        OR NOT EXISTS (
            SELECT 1 FROM client_preferences p
            WHERE p.scope = 'account' AND p.user_id = s.user_id
              AND p.key = 'cron.notificationEnabled'
        )
        OR NOT EXISTS (
            SELECT 1 FROM client_preferences p
            WHERE p.scope = 'account' AND p.user_id = s.user_id
              AND p.key = 'system.commandQueueEnabled'
        )
        OR NOT EXISTS (
            SELECT 1 FROM client_preferences p
            WHERE p.scope = 'account' AND p.user_id = s.user_id
              AND p.key = 'system.saveUploadToWorkspace'
        )
    )
    THEN 1
    ELSE 0
END;

DROP TABLE client_preference_scope_checks;

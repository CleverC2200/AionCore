-- Recoverable, tenant-scoped projection of GEA-owned user notifications.
CREATE TABLE IF NOT EXISTS gea_notifications (
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    tenant_id TEXT NOT NULL DEFAULT '',
    notification_id TEXT NOT NULL,
    version TEXT NOT NULL,
    status TEXT NOT NULL,
    kind TEXT NOT NULL,
    severity TEXT NOT NULL,
    title TEXT NOT NULL,
    summary TEXT,
    body TEXT,
    dismissible INTEGER NOT NULL DEFAULT 0,
    source TEXT NOT NULL,
    target TEXT NOT NULL,
    interaction_request_id TEXT,
    created_at TEXT NOT NULL,
    expires_at TEXT,
    upstream_revision TEXT NOT NULL,
    changed_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, tenant_id, notification_id)
);

CREATE INDEX IF NOT EXISTS idx_gea_notifications_scope_status
    ON gea_notifications(user_id, tenant_id, status, created_at DESC);

CREATE TABLE IF NOT EXISTS gea_notification_scopes (
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    tenant_id TEXT NOT NULL DEFAULT '',
    revision TEXT NOT NULL,
    last_synced_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, tenant_id)
);

CREATE TABLE IF NOT EXISTS gea_notification_receipts (
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    tenant_id TEXT NOT NULL DEFAULT '',
    notification_id TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    expected_version TEXT NOT NULL,
    action TEXT NOT NULL,
    receipt TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, tenant_id, notification_id, idempotency_key),
    UNIQUE (user_id, tenant_id, notification_id, expected_version, action),
    FOREIGN KEY (user_id, tenant_id, notification_id)
        REFERENCES gea_notifications(user_id, tenant_id, notification_id)
        ON DELETE CASCADE
);

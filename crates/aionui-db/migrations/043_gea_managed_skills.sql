-- Last-good GEA Resource Catalog snapshots are isolated by user, tenant, and
-- configured GEA environment. The snapshot never contains credentials.
CREATE TABLE IF NOT EXISTS gea_resource_catalogs (
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    tenant_id TEXT NOT NULL,
    environment TEXT NOT NULL,
    revision TEXT NOT NULL,
    server_time TEXT,
    snapshot TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, tenant_id, environment)
);

-- Materialized GEA skills stay separate from user-authored skills so a remote
-- catalog can never overwrite or delete local content with the same name.
CREATE TABLE IF NOT EXISTS gea_managed_skills (
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    tenant_id TEXT NOT NULL,
    environment TEXT NOT NULL,
    skill_code TEXT NOT NULL,
    version TEXT NOT NULL,
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    digest TEXT NOT NULL,
    artifact_size INTEGER NOT NULL,
    state TEXT NOT NULL,
    risk_level TEXT,
    path TEXT NOT NULL,
    catalog_revision TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, tenant_id, environment, skill_code)
);

CREATE TABLE IF NOT EXISTS gea_resource_active_scopes (
    user_id TEXT PRIMARY KEY NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    tenant_id TEXT NOT NULL,
    environment TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_gea_managed_skills_user_state
    ON gea_managed_skills(user_id, state, name);

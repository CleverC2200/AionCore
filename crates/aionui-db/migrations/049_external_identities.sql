-- Migration 049: persist external identity tuples independently from Core users.

CREATE TABLE IF NOT EXISTS external_identities (
    id         TEXT PRIMARY KEY NOT NULL,
    provider   TEXT NOT NULL CHECK (provider IN ('lark')),
    issuer     TEXT NOT NULL CHECK (length(issuer) > 0),
    tenant_id  TEXT NOT NULL CHECK (length(tenant_id) > 0),
    subject    TEXT NOT NULL CHECK (length(subject) > 0),
    user_id    TEXT NOT NULL REFERENCES users(id),
    created_at INTEGER NOT NULL,
    UNIQUE (provider, issuer, tenant_id, subject)
);

CREATE INDEX IF NOT EXISTS idx_external_identities_user
    ON external_identities(user_id);

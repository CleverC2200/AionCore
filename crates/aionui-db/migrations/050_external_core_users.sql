-- Migration 050: add passwordless Core users provisioned from external identities.

PRAGMA foreign_keys = OFF;

CREATE TABLE IF NOT EXISTS users_external_new (
    id                 TEXT PRIMARY KEY NOT NULL,
    user_type          TEXT NOT NULL DEFAULT 'local'
                           CHECK(user_type IN ('local', 'aionpro', 'external')),
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
    adopted_by         TEXT,
    adopted_at         INTEGER,
    CHECK (
        (user_type = 'local' AND password_hash IS NOT NULL)
        OR
        (user_type = 'aionpro')
        OR
        (user_type = 'external' AND password_hash IS NULL AND external_user_id IS NULL)
    ),
    CHECK (
        (external_user_id IS NULL)
        OR
        (length(external_user_id) > 0)
    )
);

INSERT INTO users_external_new (
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
    last_login,
    adopted_by,
    adopted_at
)
SELECT
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
    last_login,
    adopted_by,
    adopted_at
FROM users;

CREATE TEMP TABLE IF NOT EXISTS external_core_user_migration_checks (
    ok INTEGER NOT NULL CHECK (ok = 1)
);

INSERT INTO external_core_user_migration_checks (ok)
SELECT CASE
    WHEN (SELECT COUNT(*) FROM users_external_new) = (SELECT COUNT(*) FROM users)
    THEN 1
    ELSE 0
END;

DROP TABLE external_core_user_migration_checks;

DROP TABLE users;
ALTER TABLE users_external_new RENAME TO users;

CREATE UNIQUE INDEX IF NOT EXISTS idx_users_local_username
    ON users(username)
    WHERE user_type = 'local' AND username IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_users_email
    ON users(email)
    WHERE email IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_users_external_user
    ON users(user_type, external_user_id)
    WHERE external_user_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_users_username ON users(username);
CREATE INDEX IF NOT EXISTS idx_users_status ON users(status);

PRAGMA foreign_keys = ON;

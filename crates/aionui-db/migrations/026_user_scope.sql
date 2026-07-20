-- Migration 026: extend users for local and AionPro identity projection.

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

PRAGMA foreign_keys = ON;

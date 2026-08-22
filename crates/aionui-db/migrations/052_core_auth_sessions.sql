-- Migration 052: durable renewable sessions for passwordless external Core users.

CREATE TABLE IF NOT EXISTS core_auth_sessions (
    sid                 TEXT PRIMARY KEY NOT NULL,
    user_id             TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    current_refresh_hash TEXT NOT NULL CHECK (length(current_refresh_hash) = 64),
    previous_refresh_hash TEXT CHECK (previous_refresh_hash IS NULL OR length(previous_refresh_hash) = 64),
    last_rotation_key_hash TEXT CHECK (last_rotation_key_hash IS NULL OR length(last_rotation_key_hash) = 64),
    last_rotated_at     INTEGER,
    session_generation  INTEGER NOT NULL,
    rotation            INTEGER NOT NULL DEFAULT 0 CHECK (rotation >= 0),
    session_expires_at  INTEGER NOT NULL,
    revoked_at          INTEGER,
    revoke_reason       TEXT,
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL,
    CHECK (session_expires_at > created_at),
    CHECK ((revoked_at IS NULL AND revoke_reason IS NULL) OR revoked_at IS NOT NULL),
    CHECK (
        (previous_refresh_hash IS NULL AND last_rotation_key_hash IS NULL AND last_rotated_at IS NULL)
        OR
        (previous_refresh_hash IS NOT NULL AND last_rotation_key_hash IS NOT NULL AND last_rotated_at IS NOT NULL)
    )
);

CREATE INDEX IF NOT EXISTS idx_core_auth_sessions_user_active
    ON core_auth_sessions(user_id, revoked_at, session_expires_at);

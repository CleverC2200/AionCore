-- Recoverable local projection of GEA-owned human interaction requests.
CREATE TABLE IF NOT EXISTS gea_interaction_requests (
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    request_id TEXT NOT NULL,
    conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    version TEXT NOT NULL,
    status TEXT NOT NULL,
    kind TEXT NOT NULL,
    title TEXT NOT NULL,
    summary TEXT,
    source_label TEXT,
    allowed_actions TEXT NOT NULL,
    expires_at TEXT,
    updated_at TEXT NOT NULL,
    presentation TEXT NOT NULL,
    upstream_revision TEXT NOT NULL,
    turn_id TEXT,
    message_id TEXT NOT NULL,
    changed_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, request_id)
);

CREATE INDEX IF NOT EXISTS idx_gea_interaction_requests_user_status
    ON gea_interaction_requests(user_id, status, updated_at);

CREATE INDEX IF NOT EXISTS idx_gea_interaction_requests_conversation_status
    ON gea_interaction_requests(user_id, conversation_id, status);

-- Non-secret bootstrap data used to recreate GEA sessions after restart.
CREATE TABLE IF NOT EXISTS gea_interaction_session_bootstraps (
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    consumer_code TEXT NOT NULL,
    preparation_id TEXT,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, conversation_id)
);

CREATE TABLE IF NOT EXISTS gea_interaction_request_receipts (
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    request_id TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    expected_version TEXT NOT NULL,
    action_id TEXT NOT NULL,
    receipt TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    resume_claim_owner TEXT,
    resume_claimed_at INTEGER,
    resume_started_at INTEGER,
    resume_delivered_at INTEGER,
    finalized_at INTEGER,
    PRIMARY KEY (user_id, request_id, idempotency_key),
    FOREIGN KEY (user_id, request_id)
        REFERENCES gea_interaction_requests(user_id, request_id)
        ON DELETE CASCADE
);

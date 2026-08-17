-- Recoverable local projection of GEA-owned human interaction requests.
CREATE TABLE gea_interaction_requests (
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

CREATE INDEX idx_gea_interaction_requests_user_status
    ON gea_interaction_requests(user_id, status, updated_at);

CREATE INDEX idx_gea_interaction_requests_conversation_status
    ON gea_interaction_requests(user_id, conversation_id, status);

CREATE TABLE gea_interaction_request_receipts (
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    request_id TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    expected_version TEXT NOT NULL,
    action_id TEXT NOT NULL,
    receipt TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, request_id, idempotency_key),
    FOREIGN KEY (user_id, request_id)
        REFERENCES gea_interaction_requests(user_id, request_id)
        ON DELETE CASCADE
);

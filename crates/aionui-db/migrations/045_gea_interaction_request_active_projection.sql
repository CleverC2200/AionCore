-- Whether the row is still open according to the latest authoritative GEA
-- complete snapshot. Snapshot absence only deactivates the local projection
-- and must not invent a terminal business state.
ALTER TABLE gea_interaction_requests
    ADD COLUMN active INTEGER NOT NULL DEFAULT 1;

CREATE INDEX IF NOT EXISTS idx_gea_interaction_requests_user_active
    ON gea_interaction_requests(user_id, active, updated_at);

CREATE INDEX IF NOT EXISTS idx_gea_interaction_requests_conversation_active
    ON gea_interaction_requests(user_id, conversation_id, active);

-- Explicit, non-secret disposition trail for local rows that leave the active
-- projection during GEA-only cutover or later authoritative reconciliation.
CREATE TABLE IF NOT EXISTS gea_interaction_request_projection_audit (
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    request_id TEXT NOT NULL,
    conversation_id TEXT NOT NULL,
    local_status TEXT NOT NULL,
    source_revision TEXT NOT NULL,
    disposition TEXT NOT NULL,
    recorded_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, request_id, disposition)
);

INSERT OR IGNORE INTO gea_interaction_request_projection_audit
    (user_id, request_id, conversation_id, local_status, source_revision, disposition, recorded_at)
SELECT user_id, request_id, conversation_id, status, upstream_revision,
       'quarantined_legacy_approval', changed_at
FROM gea_interaction_requests
WHERE active = 1 AND kind = 'approval';

UPDATE gea_interaction_requests
SET active = 0
WHERE active = 1 AND kind = 'approval';

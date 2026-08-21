-- Durable at-most-once intents and receipts for native Feishu approval writes.
CREATE TABLE IF NOT EXISTS approval_action_receipts (
    idempotency_key TEXT PRIMARY KEY,
    action TEXT NOT NULL,
    payload TEXT NOT NULL,
    instance_code TEXT NOT NULL,
    task_id TEXT NOT NULL,
    receipt TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_approval_action_receipts_target
    ON approval_action_receipts(instance_code, task_id, updated_at);

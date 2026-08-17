CREATE TABLE IF NOT EXISTS voice_configurations (
    user_id                 TEXT PRIMARY KEY NOT NULL,
    configuration_encrypted TEXT NOT NULL,
    updated_at              INTEGER NOT NULL,
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
);

use aionui_db::init_database_memory;

#[tokio::test]
async fn migration_052_creates_hash_only_durable_external_sessions() {
    let db = init_database_memory().await.unwrap();
    let columns: Vec<(String, String, i64)> =
        sqlx::query_as("SELECT name, type, \"notnull\" FROM pragma_table_info('core_auth_sessions') ORDER BY cid")
            .fetch_all(db.pool())
            .await
            .unwrap();
    let names: Vec<_> = columns.iter().map(|(name, _, _)| name.as_str()).collect();
    for required in [
        "sid",
        "user_id",
        "current_refresh_hash",
        "previous_refresh_hash",
        "last_rotation_key_hash",
        "last_rotated_at",
        "session_generation",
        "rotation",
        "session_expires_at",
        "revoked_at",
        "revoke_reason",
    ] {
        assert!(names.contains(&required), "missing {required}");
    }
    assert!(
        !names
            .iter()
            .any(|name| name.contains("token") || name.contains("secret"))
    );

    sqlx::query(
        "INSERT INTO users (id, user_type, status, session_generation, created_at, updated_at) \
         VALUES ('external-user', 'external', 'active', 0, 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    assert!(
        sqlx::query(
            "INSERT INTO core_auth_sessions \
             (sid, user_id, current_refresh_hash, session_generation, rotation, session_expires_at, created_at, updated_at) \
             VALUES ('bad-hash', 'external-user', 'plaintext', 0, 0, 100, 1, 1)",
        )
        .execute(db.pool())
        .await
        .is_err()
    );
    assert!(
        sqlx::query(
            "INSERT INTO core_auth_sessions \
             (sid, user_id, current_refresh_hash, previous_refresh_hash, session_generation, rotation, \
              session_expires_at, created_at, updated_at) \
             VALUES ('orphan-previous', 'external-user', ?, ?, 0, 1, 100, 1, 1)",
        )
        .bind("a".repeat(64))
        .bind("b".repeat(64))
        .execute(db.pool())
        .await
        .is_err()
    );
}

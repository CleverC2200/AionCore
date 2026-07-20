use aionui_db::init_database_memory;
use sqlx::Row;

async fn table_columns(pool: &sqlx::SqlitePool, table: &str) -> Vec<String> {
    let rows = sqlx::query(&format!("PRAGMA table_info({table})"))
        .fetch_all(pool)
        .await
        .unwrap();
    rows.into_iter().map(|row| row.get::<String, _>("name")).collect()
}

async fn table_indexes(pool: &sqlx::SqlitePool, table: &str) -> Vec<String> {
    let rows = sqlx::query(&format!("PRAGMA index_list({table})"))
        .fetch_all(pool)
        .await
        .unwrap();
    rows.into_iter().map(|row| row.get::<String, _>("name")).collect()
}

#[tokio::test]
async fn migration_026_adds_core_user_projection_columns() {
    let db = init_database_memory().await.unwrap();
    let columns = table_columns(db.pool(), "users").await;
    for column in ["user_type", "external_user_id", "status", "session_generation"] {
        assert!(
            columns.iter().any(|existing| existing == column),
            "missing users.{column}"
        );
    }

    let indexes = table_indexes(db.pool(), "users").await;
    assert!(
        indexes.iter().any(|index| index == "idx_users_external_user"),
        "missing external user lookup index"
    );
}

#[tokio::test]
async fn migration_026_adds_user_scope_to_independent_roots() {
    let db = init_database_memory().await.unwrap();
    for (table, column) in [
        ("cron_jobs", "user_id"),
        ("providers", "user_id"),
        ("remote_agents", "user_id"),
        ("mcp_servers", "user_id"),
        ("oauth_tokens", "user_id"),
        ("system_settings", "user_id"),
        ("client_preferences", "user_id"),
        ("assistant_plugins", "owner_user_id"),
        ("assistant_users", "owner_user_id"),
        ("assistant_pairing_codes", "owner_user_id"),
    ] {
        let columns = table_columns(db.pool(), table).await;
        assert!(
            columns.iter().any(|existing| existing == column),
            "missing {table}.{column}"
        );
    }
}

#[tokio::test]
async fn migration_026_keeps_new_conversation_cron_jobs_unanchored_until_run() {
    let db = init_database_memory().await.unwrap();
    let now = aionui_common::now_ms();

    sqlx::query(
        "INSERT INTO cron_jobs (\
            id, user_id, name, enabled, schedule_kind, schedule_value, \
            schedule_description, payload_message, execution_mode, agent_config, \
            conversation_id, conversation_title, created_by, created_at, updated_at, \
            next_run_at, run_count, retry_count, max_retries, queue_enabled\
        ) VALUES (\
            'cron_unanchored', 'system_default_user', 'Unanchored', 1, 'every', '60000', \
            NULL, 'message', 'new_conversation', NULL, '', NULL, 'user', ?, ?, \
            NULL, 0, 0, 3, 0\
        )",
    )
    .bind(now)
    .bind(now)
    .execute(db.pool())
    .await
    .unwrap();

    let row = sqlx::query("SELECT user_id, conversation_id FROM cron_jobs WHERE id = 'cron_unanchored'")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("user_id"), "system_default_user");
    assert_eq!(row.get::<String, _>("conversation_id"), "");
}

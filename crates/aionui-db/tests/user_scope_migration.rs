use std::borrow::Cow;
use std::path::Path;

use aionui_db::init_database_memory;
use sqlx::Row;
use sqlx::migrate::Migrator;
use sqlx::sqlite::SqlitePoolOptions;

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

async fn run_migrations_through(pool: &sqlx::SqlitePool, max_version: i64) {
    sqlx::query("PRAGMA foreign_keys = OFF").execute(pool).await.unwrap();
    let full = Migrator::new(Path::new("migrations")).await.unwrap();
    let migrations = full
        .migrations
        .iter()
        .filter(|migration| migration.version <= max_version)
        .cloned()
        .collect::<Vec<_>>();
    let migrator = Migrator {
        migrations: Cow::Owned(migrations),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    };
    migrator.run(pool).await.unwrap();
}

async fn run_migration(pool: &sqlx::SqlitePool, version: i64) {
    run_migration_result(pool, version).await.unwrap();
}

async fn run_migration_result(pool: &sqlx::SqlitePool, version: i64) -> Result<(), sqlx::migrate::MigrateError> {
    let full = Migrator::new(Path::new("migrations")).await.unwrap();
    let migrations = full
        .migrations
        .iter()
        .filter(|migration| migration.version == version)
        .cloned()
        .collect::<Vec<_>>();
    let migrator = Migrator {
        migrations: Cow::Owned(migrations),
        ignore_missing: true,
        locking: true,
        no_tx: false,
    };
    migrator.run(pool).await
}

#[tokio::test]
async fn migration_028_adds_core_user_projection_columns() {
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
async fn migration_028_adds_user_scope_to_independent_roots() {
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
async fn migration_028_migrates_cron_skills_as_global_rows() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    run_migrations_through(&pool, 26).await;

    sqlx::query(
        "INSERT INTO skills (id, name, description, path, source, enabled, created_at, updated_at)
         VALUES ('legacy-cron-skill', 'scheduled-task', NULL, '/tmp/scheduled-task', 'cron', 1, 1, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    run_migration(&pool, 28).await;

    let row = sqlx::query("SELECT user_id, source FROM skills WHERE id = 'legacy-cron-skill'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("source"), "cron");
    assert_eq!(row.get::<Option<String>, _>("user_id"), None);
}

#[tokio::test]
async fn migration_028_keeps_new_conversation_cron_jobs_unanchored_until_run() {
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

#[tokio::test]
async fn migration_028_rejects_channel_session_cross_user_conversation() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    run_migrations_through(&pool, 26).await;

    sqlx::query(
        "INSERT INTO users (id, username, password_hash, created_at, updated_at)
         VALUES ('user_b', 'user_b', 'hash', 1, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, extra, status, created_at, updated_at)
         VALUES ('conv_b', 'user_b', 'User B Conversation', 'chat', '{}', 'pending', 1, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO assistant_users (
             id, platform_user_id, platform_type, display_name, authorized_at, last_active, session_id
         ) VALUES (
             'channel_user_a', 'platform-user', 'telegram', NULL, 1, NULL, NULL
         )",
    )
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO assistant_sessions (
             id, user_id, agent_type, conversation_id, workspace, chat_id, created_at, last_activity
         ) VALUES (
             'session_cross_user', 'channel_user_a', 'acp', 'conv_b', NULL, 'chat-1', 1, 1
         )",
    )
    .execute(&pool)
    .await
    .unwrap();

    let err = run_migration_result(&pool, 28).await.unwrap_err();
    assert!(
        err.to_string().contains("CHECK constraint failed"),
        "unexpected migration error: {err}"
    );
}

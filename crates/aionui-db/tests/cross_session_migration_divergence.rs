use std::path::Path;

use aionui_db::init_database_staged;
use sqlx::migrate::Migrator;
use sqlx::sqlite::SqlitePoolOptions;

async fn open_database(path: &Path) -> sqlx::SqlitePool {
    let url = format!("sqlite://{}?mode=rwc", path.display());
    SqlitePoolOptions::new().max_connections(1).connect(&url).await.unwrap()
}

async fn migration_description(db: &aionui_db::Database, version: i64) -> String {
    sqlx::query_scalar("SELECT description FROM _sqlx_migrations WHERE version = ? AND success = TRUE")
        .bind(version)
        .fetch_one(db.pool())
        .await
        .unwrap()
}

async fn remove_personal_team_work_schema(pool: &sqlx::SqlitePool) {
    for statement in [
        "DROP TABLE team_work_commands",
        "DROP TABLE team_work_events",
        "DROP TABLE team_work_runs",
        "DROP TABLE team_work_tasks",
    ] {
        sqlx::query(statement).execute(pool).await.unwrap();
    }
}

#[tokio::test]
async fn official_cross_session_41_is_remapped_before_personal_team_work_41_runs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aionui-backend.db");
    let db = init_database_staged(&path).await.unwrap();
    db.close().await;

    let migrations = Migrator::new(Path::new("migrations")).await.unwrap();
    let cross_session = migrations
        .migrations
        .iter()
        .find(|migration| migration.version == 48)
        .expect("cross-session migration 48");
    let pool = open_database(&path).await;
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version IN (41, 48)")
        .execute(&pool)
        .await
        .unwrap();
    remove_personal_team_work_schema(&pool).await;
    sqlx::query(
        "INSERT INTO _sqlx_migrations \
         (version, description, installed_on, success, checksum, execution_time) \
         VALUES (41, 'cross session message setting', CURRENT_TIMESTAMP, TRUE, ?, 7)",
    )
    .bind(cross_session.checksum.as_ref())
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let db = init_database_staged(&path).await.unwrap();

    assert_eq!(migration_description(&db, 41).await, "team work kernel");
    assert_eq!(migration_description(&db, 48).await, "cross session message setting");
    let team_work_table: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'team_work_tasks'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(team_work_table, 1);
    let cross_session_column: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info('system_settings') WHERE name = 'cross_session_message_enabled'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(cross_session_column, 1);
}

#[tokio::test]
async fn unknown_official_cross_session_41_checksum_fails_closed_through_staged_init() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aionui-backend.db");
    let db = init_database_staged(&path).await.unwrap();
    db.close().await;

    let pool = open_database(&path).await;
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version IN (41, 48)")
        .execute(&pool)
        .await
        .unwrap();
    remove_personal_team_work_schema(&pool).await;
    sqlx::query(
        "INSERT INTO _sqlx_migrations \
         (version, description, installed_on, success, checksum, execution_time) \
         VALUES (41, 'cross session message setting', CURRENT_TIMESTAMP, TRUE, x'00', 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let err = init_database_staged(&path)
        .await
        .expect_err("an unknown official cross-session migration checksum must not be rewritten");
    assert_eq!(err.stage(), "database.migration");
}

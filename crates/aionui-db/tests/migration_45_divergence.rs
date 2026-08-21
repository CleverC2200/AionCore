use std::path::Path;

use aionui_db::{init_database_memory, init_database_staged};
use sqlx::migrate::Migrator;
use sqlx::sqlite::SqlitePoolOptions;

async fn build_latest_database(path: &Path) -> Migrator {
    let db = init_database_staged(path).await.unwrap();
    db.close().await;

    Migrator::new(Path::new("migrations")).await.unwrap()
}

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

async fn assert_both_schemas_exist(db: &aionui_db::Database) {
    let active_column: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM pragma_table_info('gea_interaction_requests') WHERE name = 'active'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(active_column, 1);

    let approval_table: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'approval_action_receipts'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(approval_table, 1);
}

#[tokio::test]
async fn fresh_database_applies_gea_45_then_approval_46() {
    let db = init_database_memory().await.unwrap();

    assert_eq!(
        migration_description(&db, 45).await,
        "gea interaction request active projection"
    );
    assert_eq!(migration_description(&db, 46).await, "approval action receipts");
    assert_both_schemas_exist(&db).await;
}

#[tokio::test]
async fn database_with_gea_45_applies_approval_46() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aionui-backend.db");
    build_latest_database(&path).await;
    let pool = open_database(&path).await;
    sqlx::query("DROP TABLE approval_action_receipts")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 46")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    let db = init_database_staged(&path).await.unwrap();

    assert_eq!(
        migration_description(&db, 45).await,
        "gea interaction request active projection"
    );
    assert_eq!(migration_description(&db, 46).await, "approval action receipts");
    assert_both_schemas_exist(&db).await;
}

#[tokio::test]
async fn historical_approval_45_is_remapped_before_gea_45_runs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aionui-backend.db");
    let full = build_latest_database(&path).await;
    let approval = full
        .migrations
        .iter()
        .find(|migration| migration.version == 46)
        .expect("approval migration 46");

    let pool = open_database(&path).await;
    sqlx::raw_sql(
        "DROP INDEX idx_gea_interaction_requests_user_active;
         DROP INDEX idx_gea_interaction_requests_conversation_active;
         DROP TABLE gea_interaction_request_projection_audit;
         ALTER TABLE gea_interaction_requests DROP COLUMN active;
         DELETE FROM _sqlx_migrations WHERE version IN (45, 46);",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO _sqlx_migrations \
         (version, description, installed_on, success, checksum, execution_time) \
         VALUES (45, 'approval action receipts', CURRENT_TIMESTAMP, TRUE, ?, 7)",
    )
    .bind(approval.checksum.as_ref())
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let db = init_database_staged(&path).await.unwrap();

    assert_eq!(
        migration_description(&db, 45).await,
        "gea interaction request active projection"
    );
    assert_eq!(migration_description(&db, 46).await, "approval action receipts");
    assert_both_schemas_exist(&db).await;
    db.close().await;

    let reopened = init_database_staged(&path).await.unwrap();
    assert_both_schemas_exist(&reopened).await;
}

#[tokio::test]
async fn unknown_approval_45_checksum_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aionui-backend.db");
    build_latest_database(&path).await;

    let pool = open_database(&path).await;
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version IN (45, 46)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO _sqlx_migrations \
         (version, description, installed_on, success, checksum, execution_time) \
         VALUES (45, 'approval action receipts', CURRENT_TIMESTAMP, TRUE, x'00', 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let err = init_database_staged(&path)
        .await
        .expect_err("an unknown migration 45 checksum must not be rewritten");
    assert_eq!(err.stage(), "database.migration");
}

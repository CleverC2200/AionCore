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

#[tokio::test]
async fn official_sidebar_40_is_remapped_before_voice_40_runs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aionui-backend.db");
    let db = init_database_staged(&path).await.unwrap();
    db.close().await;

    let migrations = Migrator::new(Path::new("migrations")).await.unwrap();
    let sidebar = migrations
        .migrations
        .iter()
        .find(|migration| migration.version == 47)
        .expect("sidebar migration 47");
    let pool = open_database(&path).await;
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version IN (40, 47)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO _sqlx_migrations \
         (version, description, installed_on, success, checksum, execution_time) \
         VALUES (40, 'sidebar ordering and archive', CURRENT_TIMESTAMP, TRUE, ?, 7)",
    )
    .bind(sidebar.checksum.as_ref())
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let db = init_database_staged(&path).await.unwrap();

    assert_eq!(migration_description(&db, 40).await, "voice configuration");
    assert_eq!(migration_description(&db, 47).await, "sidebar ordering and archive");
    let voice_table: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'voice_configurations'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    let sidebar_table: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'user_order'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(voice_table, 1);
    assert_eq!(sidebar_table, 1);
}

#[tokio::test]
async fn unknown_sidebar_40_checksum_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aionui-backend.db");
    let db = init_database_staged(&path).await.unwrap();
    db.close().await;

    let pool = open_database(&path).await;
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version IN (40, 47)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO _sqlx_migrations \
         (version, description, installed_on, success, checksum, execution_time) \
         VALUES (40, 'sidebar ordering and archive', CURRENT_TIMESTAMP, TRUE, x'00', 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let err = init_database_staged(&path)
        .await
        .expect_err("an unknown sidebar migration 40 checksum must not be rewritten");
    assert_eq!(err.stage(), "database.migration");
}

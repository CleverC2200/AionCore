//! Migration 030: assistant_plugins → channel_connections (connection entity).
//!
//! Verifies the segment-1 backfill: legacy platform-type ids become
//! `plugin_key`, connection ids are freshly generated, config/state columns
//! are preserved, and the phase-1 single-instance index holds.

use std::borrow::Cow;
use std::path::Path;

use sqlx::Row;
use sqlx::migrate::Migrator;
use sqlx::sqlite::SqlitePoolOptions;

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
    migrator.run(pool).await.unwrap();
}

#[tokio::test]
async fn migration_030_rebuilds_plugins_as_connections() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    run_migrations_through(&pool, 29).await;

    // Legacy rows: id IS the platform type (one per owner+platform).
    sqlx::query(
        "INSERT INTO assistant_plugins (
            id, owner_user_id, type, name, enabled, config, status,
            last_connected, created_at, updated_at
         ) VALUES
            ('weixin', 'system_default_user', 'weixin', 'WeChat', 1, 'enc-config', 'running', 111, 1, 2),
            ('telegram', 'system_default_user', 'telegram', 'TG Bot', 0, 'enc-tg', NULL, NULL, 3, 4)",
    )
    .execute(&pool)
    .await
    .unwrap();

    run_migration(&pool, 30).await;

    // Old table is gone; new table holds one connection per legacy row.
    let old_table: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'assistant_plugins'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(old_table, 0);

    let rows = sqlx::query(
        "SELECT id, owner_user_id, plugin_key, name, enabled, config, status, last_connected \
         FROM channel_connections ORDER BY created_at ASC",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);

    let weixin = &rows[0];
    assert_eq!(weixin.get::<String, _>("plugin_key"), "weixin");
    // The connection id is generated and no longer the platform type.
    let conn_id = weixin.get::<String, _>("id");
    assert!(conn_id.starts_with("conn_"), "unexpected id: {conn_id}");
    assert_ne!(conn_id, "weixin");
    assert_eq!(weixin.get::<String, _>("owner_user_id"), "system_default_user");
    assert_eq!(weixin.get::<String, _>("name"), "WeChat");
    assert!(weixin.get::<bool, _>("enabled"));
    assert_eq!(weixin.get::<String, _>("config"), "enc-config");
    assert_eq!(weixin.get::<Option<String>, _>("status").as_deref(), Some("running"));
    assert_eq!(weixin.get::<Option<i64>, _>("last_connected"), Some(111));

    let telegram = &rows[1];
    assert_eq!(telegram.get::<String, _>("plugin_key"), "telegram");
    assert_ne!(telegram.get::<String, _>("id"), conn_id);

    // Phase-1 single-instance guard is present.
    let dup = sqlx::query(
        "INSERT INTO channel_connections (
            id, owner_user_id, plugin_key, name, enabled, config, created_at, updated_at
         ) VALUES ('conn_dup', 'system_default_user', 'weixin', 'Dup', 0, '', 5, 5)",
    )
    .execute(&pool)
    .await;
    let err = dup.unwrap_err().to_string();
    assert!(err.contains("UNIQUE"), "unexpected error: {err}");
}

use std::borrow::Cow;
use std::path::Path;

use sqlx::migrate::Migrator;
use sqlx::sqlite::SqlitePoolOptions;

#[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
struct PreservedUser {
    id: String,
    user_type: String,
    external_user_id: Option<String>,
    username: Option<String>,
    email: Option<String>,
    password_hash: Option<String>,
    avatar_path: Option<String>,
    jwt_secret: Option<String>,
    status: String,
    session_generation: i64,
    created_at: i64,
    updated_at: i64,
    last_login: Option<i64>,
    adopted_by: Option<String>,
    adopted_at: Option<i64>,
}

async fn run_migrations_through(pool: &sqlx::SqlitePool, max_version: i64) {
    let full = Migrator::new(Path::new("migrations")).await.unwrap();
    let migrations = full
        .migrations
        .iter()
        .filter(|migration| migration.version <= max_version)
        .cloned()
        .collect();
    Migrator {
        migrations: Cow::Owned(migrations),
        ..Migrator::DEFAULT
    }
    .run(pool)
    .await
    .unwrap();
}

async fn users(pool: &sqlx::SqlitePool) -> Vec<PreservedUser> {
    sqlx::query_as("SELECT * FROM users ORDER BY id")
        .fetch_all(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn migration_050_preserves_local_and_aionpro_users_and_evolves_constraints() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::query("PRAGMA foreign_keys = OFF; PRAGMA legacy_alter_table = ON")
        .execute(&pool)
        .await
        .unwrap();
    run_migrations_through(&pool, 49).await;

    sqlx::query(
        "INSERT INTO users (
            id, user_type, external_user_id, username, email, password_hash,
            avatar_path, jwt_secret, status, session_generation, created_at,
            updated_at, last_login, adopted_by, adopted_at
         ) VALUES
            ('local-preserved', 'local', NULL, 'local-name', 'local@example.com',
             'local-hash', '/local/avatar', 'local-jwt', 'disabled', 7, 10, 11,
             12, 'adopter', 13),
            ('aionpro-preserved', 'aionpro', 'aionpro-subject', 'pro-name',
             'pro@example.com', NULL, '/pro/avatar', 'pro-jwt', 'active', 8,
             20, 21, 22, NULL, NULL)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO external_identities (
            id, provider, issuer, tenant_id, subject, user_id, created_at
         ) VALUES (
            'preserved-mapping', 'lark', 'https://open.feishu.cn',
            'preserved-tenant', 'preserved-subject', 'local-preserved', 23
         )",
    )
    .execute(&pool)
    .await
    .unwrap();
    let before = users(&pool).await;

    run_migrations_through(&pool, 50).await;

    assert_eq!(users(&pool).await, before);
    let preserved_mapping_owner: String =
        sqlx::query_scalar("SELECT user_id FROM external_identities WHERE id = 'preserved-mapping'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(preserved_mapping_owner, "local-preserved");

    sqlx::query(
        "INSERT INTO users (
            id, user_type, external_user_id, password_hash, status,
            session_generation, created_at, updated_at
         ) VALUES ('external-valid', 'external', NULL, NULL, 'active', 0, 30, 30)",
    )
    .execute(&pool)
    .await
    .unwrap();
    for invalid in [
        "INSERT INTO users (id, user_type, external_user_id, password_hash, created_at, updated_at)
         VALUES ('external-password', 'external', NULL, 'hash', 31, 31)",
        "INSERT INTO users (id, user_type, external_user_id, password_hash, created_at, updated_at)
         VALUES ('external-platform-id', 'external', 'must-not-alias-tuple', NULL, 32, 32)",
        "INSERT INTO users (id, user_type, password_hash, created_at, updated_at)
         VALUES ('local-passwordless', 'local', NULL, 33, 33)",
        "INSERT INTO users (id, user_type, password_hash, created_at, updated_at)
         VALUES ('unsupported-type', 'lark', NULL, 34, 34)",
    ] {
        assert!(
            sqlx::query(invalid).execute(&pool).await.is_err(),
            "constraint accepted: {invalid}"
        );
    }

    assert!(
        sqlx::query(
            "INSERT INTO users (id, user_type, username, password_hash, created_at, updated_at)
             VALUES ('local-duplicate', 'local', 'local-name', 'hash', 35, 35)",
        )
        .execute(&pool)
        .await
        .is_err()
    );
    assert!(
        sqlx::query(
            "INSERT INTO users (id, user_type, external_user_id, created_at, updated_at)
             VALUES ('aionpro-duplicate', 'aionpro', 'aionpro-subject', 36, 36)",
        )
        .execute(&pool)
        .await
        .is_err()
    );

    let foreign_key_errors: Vec<(String, i64, String, i64)> = sqlx::query_as("PRAGMA foreign_key_check")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert!(foreign_key_errors.is_empty());
}

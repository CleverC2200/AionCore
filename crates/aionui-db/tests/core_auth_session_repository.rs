use std::sync::Arc;

use aionui_db::{
    CoreAuthSessionError, CreateCoreAuthSessionParams, ICoreAuthSessionRepository, RotateCoreAuthSessionParams,
    SqliteCoreAuthSessionRepository, init_database, init_database_memory,
};

async fn setup() -> (aionui_db::Database, Arc<SqliteCoreAuthSessionRepository>) {
    let db = init_database_memory().await.unwrap();
    seed_external_users(&db).await;
    let repo = Arc::new(SqliteCoreAuthSessionRepository::new(db.pool().clone()));
    (db, repo)
}

async fn seed_external_users(db: &aionui_db::Database) {
    for (id, generation) in [("external-a", 0_i64), ("external-b", 0_i64)] {
        sqlx::query(
            "INSERT INTO users (id, user_type, username, status, session_generation, created_at, updated_at) \
             VALUES (?, 'external', ?, 'active', ?, 1, 1)",
        )
        .bind(id)
        .bind(id)
        .bind(generation)
        .execute(db.pool())
        .await
        .unwrap();
    }
}

async fn create_session(repo: &SqliteCoreAuthSessionRepository, sid: &str, user_id: &str, hash: &str) {
    repo.create(CreateCoreAuthSessionParams {
        sid,
        user_id,
        current_refresh_hash: hash,
        session_generation: 0,
        session_expires_at: 100_000,
        now: 1_000,
    })
    .await
    .unwrap();
}

fn hash(byte: char) -> String {
    std::iter::repeat_n(byte, 64).collect()
}

#[tokio::test]
async fn rotation_is_atomic_idempotent_and_stores_only_hashes() {
    let (db, repo) = setup().await;
    let current = hash('a');
    let next = hash('b');
    let key = hash('c');
    create_session(&repo, "sid-a", "external-a", &current).await;

    let params = || RotateCoreAuthSessionParams {
        sid: "sid-a",
        presented_secret_hash: &current,
        replacement_secret_hash: &next,
        rotation_key_hash: &key,
        now: 2_000,
    };
    let first = repo.rotate(params()).await.unwrap();
    let retry = repo.rotate(params()).await.unwrap();
    assert_eq!(first.session.rotation, 1);
    assert_eq!(retry.session.rotation, 1);
    assert_eq!(first.session.current_refresh_hash, next);
    assert_eq!(first.session.previous_refresh_hash.as_deref(), Some(current.as_str()));
    assert_eq!(first.session.last_rotation_key_hash.as_deref(), Some(key.as_str()));

    let columns: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info('core_auth_sessions')")
        .fetch_all(db.pool())
        .await
        .unwrap();
    assert!(
        !columns
            .iter()
            .any(|name| name == "refresh_secret" || name == "refresh_token")
    );
}

#[tokio::test]
async fn replay_with_a_different_key_revokes_only_the_matching_sid() {
    let (_db, repo) = setup().await;
    let old = hash('a');
    let current = hash('b');
    let first_key = hash('c');
    let replay_key = hash('d');
    create_session(&repo, "sid-a", "external-a", &old).await;
    create_session(&repo, "sid-b", "external-a", &hash('e')).await;
    repo.rotate(RotateCoreAuthSessionParams {
        sid: "sid-a",
        presented_secret_hash: &old,
        replacement_secret_hash: &current,
        rotation_key_hash: &first_key,
        now: 2_000,
    })
    .await
    .unwrap();

    let replay = repo
        .rotate(RotateCoreAuthSessionParams {
            sid: "sid-a",
            presented_secret_hash: &old,
            replacement_secret_hash: &hash('f'),
            rotation_key_hash: &replay_key,
            now: 2_001,
        })
        .await
        .unwrap_err();
    assert!(matches!(replay, CoreAuthSessionError::Replay));
    assert!(repo.find("sid-a").await.unwrap().unwrap().revoked_at.is_some());
    assert!(repo.find("sid-b").await.unwrap().unwrap().revoked_at.is_none());
}

#[tokio::test]
async fn concurrent_different_keys_have_one_winner_then_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let db = init_database(&dir.path().join("sessions.db")).await.unwrap();
    seed_external_users(&db).await;
    let repo = Arc::new(SqliteCoreAuthSessionRepository::new(db.pool().clone()));
    let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
    let current = hash('a');
    let next = hash('b');
    create_session(&repo, "sid-race", "external-a", &current).await;
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let spawn_rotation = |key: String| {
        let repo = repo.clone();
        let barrier = barrier.clone();
        let current = current.clone();
        let next = next.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            repo.rotate(RotateCoreAuthSessionParams {
                sid: "sid-race",
                presented_secret_hash: &current,
                replacement_secret_hash: &next,
                rotation_key_hash: &key,
                now: 2_000,
            })
            .await
        })
    };
    let first = spawn_rotation(hash('c'));
    let second = spawn_rotation(hash('d'));
    barrier.wait().await;
    let results = [first.await.unwrap(), second.await.unwrap()];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(CoreAuthSessionError::Replay)))
            .count(),
        1
    );
    assert!(repo.find("sid-race").await.unwrap().unwrap().revoked_at.is_some());

    create_session(&repo, "sid-after-race", "external-b", &hash('e')).await;
    assert!(repo.find("sid-after-race").await.unwrap().is_some());
}

#[tokio::test]
async fn recent_previous_secret_can_revoke_after_a_lost_refresh_response() {
    let (_db, repo) = setup().await;
    let old = hash('a');
    let current = hash('b');
    let key = hash('c');
    create_session(&repo, "sid-a", "external-a", &old).await;
    repo.rotate(RotateCoreAuthSessionParams {
        sid: "sid-a",
        presented_secret_hash: &old,
        replacement_secret_hash: &current,
        rotation_key_hash: &key,
        now: 2_000,
    })
    .await
    .unwrap();

    let revoked = repo.revoke_matching("sid-a", &old, 2_001).await.unwrap();
    assert_eq!(revoked.revoke_reason.as_deref(), Some("matching_revoke"));
}

#[tokio::test]
async fn user_wide_revoke_invalidates_all_own_sessions_without_cross_user_impact() {
    let (_db, repo) = setup().await;
    create_session(&repo, "sid-a1", "external-a", &hash('a')).await;
    create_session(&repo, "sid-a2", "external-a", &hash('b')).await;
    create_session(&repo, "sid-b", "external-b", &hash('c')).await;

    assert_eq!(repo.revoke_user("external-a", 2_000).await.unwrap(), 1);
    assert!(repo.find("sid-a1").await.unwrap().unwrap().revoked_at.is_some());
    assert!(repo.find("sid-a2").await.unwrap().unwrap().revoked_at.is_some());
    assert!(repo.find("sid-b").await.unwrap().unwrap().revoked_at.is_none());
}

#[tokio::test]
async fn access_validation_rejects_cross_user_claims() {
    let (_db, repo) = setup().await;
    create_session(&repo, "sid-a", "external-a", &hash('a')).await;

    let error = repo
        .validate_access("sid-a", "external-b", 0, 0, 2_000)
        .await
        .unwrap_err();
    assert!(matches!(error, CoreAuthSessionError::CrossUser));
}

#[tokio::test]
async fn startup_prune_removes_only_expired_or_revoked_rows() {
    let (db, repo) = setup().await;
    create_session(&repo, "active", "external-a", &hash('a')).await;
    create_session(&repo, "expired", "external-a", &hash('b')).await;
    create_session(&repo, "revoked", "external-b", &hash('c')).await;
    sqlx::query("UPDATE core_auth_sessions SET session_expires_at = 100001 WHERE sid = 'active'")
        .execute(db.pool())
        .await
        .unwrap();
    repo.revoke_matching("revoked", &hash('c'), 2_000).await.unwrap();

    assert_eq!(repo.prune_terminal(100_000).await.unwrap(), 2);
    assert!(repo.find("active").await.unwrap().is_some());
    assert!(repo.find("expired").await.unwrap().is_none());
    assert!(repo.find("revoked").await.unwrap().is_none());
}

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
async fn migration_028_backfills_existing_independent_roots_to_default_user() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    run_migrations_through(&pool, 27).await;
    let now = 1_000_i64;

    sqlx::query(
        "INSERT INTO providers (
            id, platform, name, base_url, api_key_encrypted, models, enabled,
            capabilities, model_settings, created_at, updated_at
         ) VALUES (
            'provider_legacy', 'openai', 'OpenAI', 'https://api.example.com',
            'encrypted', '[]', 1, '[]', '{}', ?, ?
         )",
    )
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO remote_agents (
            id, name, protocol, url, auth_type, status, created_at, updated_at
         ) VALUES (
            'remote_legacy', 'Remote', 'http', 'http://127.0.0.1:1', 'none', 'unknown', ?, ?
         )",
    )
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO mcp_servers (
            id, name, enabled, transport_type, transport_config, last_test_status,
            builtin, created_at, updated_at
         ) VALUES (
            'mcp_legacy', 'legacy-mcp', 1, 'stdio', '{}', 'disconnected', 0, ?, ?
         )",
    )
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO oauth_tokens (
            server_url, access_token, token_type, created_at, updated_at
         ) VALUES (
            'https://mcp.example.com', 'encrypted-token', 'bearer', ?, ?
         )",
    )
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query("INSERT INTO client_preferences (key, value, updated_at) VALUES ('theme', 'dark', ?)")
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();

    sqlx::query(
        "INSERT INTO cron_jobs (
            id, name, enabled, schedule_kind, schedule_value, payload_message,
            execution_mode, conversation_id, created_by, created_at,
            updated_at, run_count, retry_count, max_retries, queue_enabled
         ) VALUES (
            'cron_legacy', 'Legacy Cron', 1, 'every', '60000', 'message',
            'new_conversation', '', 'user', ?, ?, 0, 0, 3, 0
         )",
    )
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();

    run_migration(&pool, 28).await;

    for (table, id_column, id_value, scope_column) in [
        ("providers", "id", "provider_legacy", "user_id"),
        ("remote_agents", "id", "remote_legacy", "user_id"),
        ("mcp_servers", "id", "mcp_legacy", "user_id"),
        ("client_preferences", "key", "theme", "user_id"),
        ("cron_jobs", "id", "cron_legacy", "user_id"),
    ] {
        let sql = format!("SELECT {scope_column} FROM {table} WHERE {id_column} = ?");
        let owner: String = sqlx::query_scalar(&sql).bind(id_value).fetch_one(&pool).await.unwrap();
        assert_eq!(
            owner, "system_default_user",
            "{table}.{scope_column} was not backfilled"
        );
    }

    let oauth_owner: String =
        sqlx::query_scalar("SELECT user_id FROM oauth_tokens WHERE server_url = 'https://mcp.example.com'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(oauth_owner, "system_default_user");
}

#[tokio::test]
async fn migration_028_classifies_global_and_user_catalog_rows() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    run_migrations_through(&pool, 27).await;
    let now = 2_000_i64;

    sqlx::query(
        "INSERT INTO agent_metadata (
            id, name, agent_type, agent_source, enabled, created_at, updated_at
         ) VALUES
            ('agent_builtin_legacy', 'Builtin Agent', 'acp', 'builtin', 1, ?, ?),
            ('agent_user_legacy', 'User Agent', 'acp', 'custom', 1, ?, ?)",
    )
    .bind(now)
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO assistant_definitions (
            id, assistant_id, source, owner_type, source_ref, name, name_i18n,
            description_i18n, avatar_type, agent_id, rule_resource_type,
            recommended_prompts, recommended_prompts_i18n, default_model_mode,
            default_permission_mode, default_skills_mode, default_skill_ids,
            custom_skill_names, default_disabled_builtin_skill_ids,
            default_mcps_mode, default_mcp_ids, created_at, updated_at
         ) VALUES
            (
                'def_builtin_legacy', 'assistant_builtin', 'builtin', 'system',
                'builtin-ref', 'Builtin Assistant', '{}', '{}', 'none',
                'agent_builtin_legacy', 'none', '[]', '{}', 'auto', 'auto',
                'auto', '[]', '[]', '[]', 'auto', '[]', ?, ?
            ),
            (
                'def_user_legacy', 'assistant_user', 'user', 'user',
                'user-ref', 'User Assistant', '{}', '{}', 'none',
                'agent_user_legacy', 'user_file', '[]', '{}', 'auto', 'auto',
                'auto', '[]', '[]', '[]', 'auto', '[]', ?, ?
            )",
    )
    .bind(now)
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO skills (id, name, description, path, source, enabled, created_at, updated_at)
         VALUES
            ('skill_builtin_legacy', 'builtin-skill', NULL, '/tmp/builtin', 'builtin', 1, ?, ?),
            ('skill_extension_legacy', 'extension-skill', NULL, '/tmp/extension', 'extension', 1, ?, ?)",
    )
    .bind(now)
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO skill_import_records (
            id, operation_id, source_label, source_name, status, error_code, created_at
         ) VALUES (
            'import_failed_legacy', 'op_legacy', 'archive.zip', 'broken-skill',
            'failed', 'INVALID_MANIFEST', ?
         )",
    )
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();

    run_migration(&pool, 28).await;

    for (table, id_column, id_value, expected_owner) in [
        ("agent_metadata", "id", "agent_builtin_legacy", None),
        ("agent_metadata", "id", "agent_user_legacy", Some("system_default_user")),
        ("assistant_definitions", "id", "def_builtin_legacy", None),
        (
            "assistant_definitions",
            "id",
            "def_user_legacy",
            Some("system_default_user"),
        ),
        ("skills", "id", "skill_builtin_legacy", None),
        ("skills", "id", "skill_extension_legacy", Some("system_default_user")),
    ] {
        let sql = format!("SELECT user_id FROM {table} WHERE {id_column} = ?");
        let owner: Option<String> = sqlx::query_scalar(&sql).bind(id_value).fetch_one(&pool).await.unwrap();
        assert_eq!(
            owner.as_deref(),
            expected_owner,
            "{table}.{id_column}={id_value} had wrong user_id"
        );
    }

    let import_owner: String =
        sqlx::query_scalar("SELECT user_id FROM skill_import_records WHERE id = 'import_failed_legacy'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(import_owner, "system_default_user");
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

async fn insert_valid_aggregate_parents(pool: &sqlx::SqlitePool) {
    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, extra, status, created_at, updated_at)
         VALUES ('conv_parent', 'system_default_user', 'Parent Conversation', 'acp', '{}', 'pending', 1, 1)",
    )
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO teams (id, user_id, name, workspace, agents, created_at, updated_at)
         VALUES ('team_parent', 'system_default_user', 'Parent Team', '', '[]', 1, 1)",
    )
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn migration_028_preserves_valid_aggregate_child_rows() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    run_migrations_through(&pool, 27).await;
    insert_valid_aggregate_parents(&pool).await;

    sqlx::query(
        "INSERT INTO messages (id, conversation_id, type, content, created_at)
         VALUES ('msg_parent', 'conv_parent', 'user', '{}', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversation_artifacts (
            id, conversation_id, cron_job_id, kind, status, payload, created_at, updated_at
         ) VALUES (
            'artifact_parent', 'conv_parent', 'cron_parent', 'skill_suggest', 'active', '{}', 1, 1
         )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversation_assistant_snapshots (
            conversation_id, assistant_definition_id, assistant_id, assistant_source,
            agent_id, rules_content, default_model_mode, default_permission_mode,
            default_thought_level_mode, default_skills_mode, resolved_skill_ids,
            resolved_disabled_builtin_skill_ids, default_mcps_mode, resolved_mcp_ids,
            created_at, updated_at
         ) VALUES (
            'conv_parent', 'definition_parent', 'assistant_parent', 'user',
            'agent_parent', '', 'auto', 'auto', 'auto', 'auto', '[]', '[]',
            'auto', '[]', 1, 1
         )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO acp_session (
            conversation_id, agent_source, agent_id, session_status, session_config
         ) VALUES (
            'conv_parent', 'builtin', 'agent_parent', 'idle', '{}'
         )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO cron_jobs (
            id, name, enabled, schedule_kind, schedule_value, payload_message,
            execution_mode, conversation_id, created_by, created_at,
            updated_at, run_count, retry_count, max_retries, queue_enabled
         ) VALUES (
            'cron_parent', 'Parent Cron', 1, 'every', '60000', 'message',
            'existing', 'conv_parent', 'user', 1, 1, 0, 0, 3, 0
         )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO mailbox (id, team_id, to_agent_id, from_agent_id, type, content, created_at)
         VALUES ('mail_parent', 'team_parent', 'a1', 'a2', 'message', 'hello', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO team_tasks (id, team_id, subject, status, created_at, updated_at)
         VALUES ('task_parent', 'team_parent', 'Task', 'pending', 1, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    run_migration(&pool, 28).await;

    for (table, id_column, id_value) in [
        ("messages", "id", "msg_parent"),
        ("conversation_artifacts", "id", "artifact_parent"),
        ("conversation_assistant_snapshots", "conversation_id", "conv_parent"),
        ("acp_session", "conversation_id", "conv_parent"),
        ("cron_jobs", "id", "cron_parent"),
        ("mailbox", "id", "mail_parent"),
        ("team_tasks", "id", "task_parent"),
    ] {
        let sql = format!("SELECT COUNT(*) FROM {table} WHERE {id_column} = ?");
        let count: i64 = sqlx::query_scalar(&sql).bind(id_value).fetch_one(&pool).await.unwrap();
        assert_eq!(count, 1, "{table} row was not preserved");
    }

    let cron_owner: String = sqlx::query_scalar("SELECT user_id FROM cron_jobs WHERE id = 'cron_parent'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(cron_owner, "system_default_user");
}

#[tokio::test]
async fn migration_028_rejects_aggregate_child_orphans() {
    for (name, setup_sql) in [
        (
            "messages",
            "INSERT INTO messages (id, conversation_id, type, content, created_at)
             VALUES ('orphan_msg', 'missing_conv', 'user', '{}', 1)",
        ),
        (
            "conversation_artifacts",
            "INSERT INTO conversation_artifacts (
                id, conversation_id, cron_job_id, kind, status, payload, created_at, updated_at
             ) VALUES (
                'orphan_artifact', 'missing_conv', NULL, 'skill_suggest', 'active', '{}', 1, 1
             )",
        ),
        (
            "conversation_assistant_snapshots",
            "INSERT INTO conversation_assistant_snapshots (
                conversation_id, assistant_definition_id, assistant_id, assistant_source,
                agent_id, rules_content, default_model_mode, default_permission_mode,
                default_thought_level_mode, default_skills_mode, resolved_skill_ids,
                resolved_disabled_builtin_skill_ids, default_mcps_mode, resolved_mcp_ids,
                created_at, updated_at
             ) VALUES (
                'missing_conv', 'definition_parent', 'assistant_parent', 'user',
                'agent_parent', '', 'auto', 'auto', 'auto', 'auto', '[]', '[]',
                'auto', '[]', 1, 1
             )",
        ),
        (
            "acp_session",
            "INSERT INTO acp_session (
                conversation_id, agent_source, agent_id, session_status, session_config
             ) VALUES (
                'missing_conv', 'builtin', 'agent_parent', 'idle', '{}'
             )",
        ),
        (
            "cron_jobs",
            "INSERT INTO cron_jobs (
                id, name, enabled, schedule_kind, schedule_value, payload_message,
                execution_mode, conversation_id, created_by, created_at,
                updated_at, run_count, retry_count, max_retries, queue_enabled
             ) VALUES (
                'orphan_cron', 'Orphan Cron', 1, 'every', '60000', 'message',
                'existing', 'missing_conv', 'user', 1, 1, 0, 0, 3, 0
             )",
        ),
        (
            "mailbox",
            "INSERT INTO mailbox (id, team_id, to_agent_id, from_agent_id, type, content, created_at)
             VALUES ('orphan_mail', 'missing_team', 'a1', 'a2', 'message', 'hello', 1)",
        ),
        (
            "team_tasks",
            "INSERT INTO team_tasks (id, team_id, subject, status, created_at, updated_at)
             VALUES ('orphan_task', 'missing_team', 'Task', 'pending', 1, 1)",
        ),
    ] {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        run_migrations_through(&pool, 27).await;
        sqlx::query(setup_sql).execute(&pool).await.unwrap();

        let err = run_migration_result(&pool, 28).await.unwrap_err();
        assert!(
            err.to_string().contains("CHECK constraint failed"),
            "unexpected migration error for {name}: {err}"
        );
    }
}

#[tokio::test]
async fn migration_028_rejects_channel_session_without_channel_user() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    run_migrations_through(&pool, 27).await;

    sqlx::query(
        "INSERT INTO assistant_sessions (
             id, user_id, agent_type, conversation_id, workspace, chat_id, created_at, last_activity
         ) VALUES (
             'session_orphan_channel_user', 'missing_channel_user', 'acp', NULL, NULL, 'chat-1', 1, 1
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

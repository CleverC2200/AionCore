//! Smoke test: starting the app twice with the same binary version
//! should be a no-op on the second run (version gate skips rewrite).

use tempfile::TempDir;

#[tokio::test]
async fn second_start_with_same_version_is_noop() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();

    let first =
        aionui_extension::materialize_if_needed(data_dir, aionui_extension::builtin_skills_corpus(), "test-1.0.0")
            .await
            .unwrap();
    assert!(first, "first call should materialize");

    let second =
        aionui_extension::materialize_if_needed(data_dir, aionui_extension::builtin_skills_corpus(), "test-1.0.0")
            .await
            .unwrap();
    assert!(!second, "second call with same version should skip");
}

#[tokio::test]
async fn content_identity_change_activates_a_new_object() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();

    let first =
        aionui_extension::materialize_if_needed(data_dir, aionui_extension::builtin_skills_corpus(), "test-1.0.0")
            .await
            .unwrap();
    assert!(first);

    let second =
        aionui_extension::materialize_if_needed(data_dir, aionui_extension::builtin_skills_corpus(), "test-2.0.0")
            .await
            .unwrap();
    assert!(second, "content identity change should activate a new object");

    let active = aionui_extension::startup_materialize::resolve_materialized_builtin_skills_dir(data_dir);
    let identity = std::fs::read_to_string(active.join(".complete")).unwrap();
    assert_eq!(identity, "test-2.0.0");
}

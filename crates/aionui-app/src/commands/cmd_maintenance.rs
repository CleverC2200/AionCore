use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use fs2::FileExt;
use serde_json::json;

use crate::cli::{Cli, MaintenanceArgs, MaintenanceCommand};
use crate::commands::error::{CliBoundaryCode, CliBoundaryError};

const SUBCOMMAND: &str = "maintenance";
const AUTOMATIC_GC_MIN_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const AUTOMATIC_GC_LIMIT: usize = 4;

#[derive(Debug, Clone)]
struct CleanupCandidate {
    path: PathBuf,
    relative_path: String,
    reason: &'static str,
    bytes: u64,
}

pub async fn run_maintenance(args: MaintenanceArgs, cli: &Cli) -> Result<ExitCode, CliBoundaryError> {
    match args.command {
        MaintenanceCommand::Status => print_status(cli).await?,
        MaintenanceCommand::Clean { apply } => clean(&cli.data_dir, apply)?,
    }
    Ok(ExitCode::SUCCESS)
}

pub fn run_automatic_maintenance(data_dir: &Path) {
    let mut candidates = collect_automatic_candidates(data_dir);
    candidates.sort_by_key(|candidate| modified_at(&candidate.path));
    let mut removed = 0usize;
    for candidate in candidates.into_iter().take(AUTOMATIC_GC_LIMIT) {
        match remove_candidate(&candidate.path) {
            Ok(()) => removed += 1,
            Err(error) => tracing::warn!(
                path = %candidate.relative_path,
                error = %error,
                "automatic maintenance could not remove regenerable cache"
            ),
        }
    }
    if removed > 0 {
        tracing::info!(removed, "automatic maintenance removed stale regenerable cache entries");
    }
}

async fn print_status(cli: &Cli) -> Result<(), CliBoundaryError> {
    let health_url = format!("http://{}:{}/health", local_host(&cli.host), cli.port);
    let health = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .map_err(|_| maintenance_error("status.client"))?
        .get(&health_url)
        .send()
        .await
        .map(|response| response.status().is_success())
        .unwrap_or(false);
    let db_path = cli.data_dir.join("aionui-backend.db");
    let lock = inspect_instance_lock(&db_path);
    let server_running = health || lock == "held";
    let builtin_identity = fs::read_to_string(cli.data_dir.join(".builtin-skills.current")).ok();
    let runtime_root = cli.data_dir.join("runtime").join("node");

    let payload = json!({
        "schema_version": 1,
        "process": {
            "reporter_pid": std::process::id(),
            "server_running": server_running,
            "background_automation": "not_managed_by_aioncore"
        },
        "configuration": {
            "app_version": cli.app_version,
            "managed_resources_mode": format!("{:?}", cli.managed_resources_mode).to_ascii_lowercase(),
            "data_dir": cli.data_dir,
        },
        "lifecycle": {
            "stopped": !server_running,
            "restart_required": false,
            "reload_required": false,
        },
        "lock": {
            "instance": lock,
            "owner": if health { "server_process" } else if lock == "held" { "peer_process" } else { "none" },
        },
        "health": {
            "url": health_url,
            "result": if health { "ok" } else if lock == "held" { "unverified_peer_port" } else { "unreachable" },
        },
        "resources": {
            "builtin_skills_identity": builtin_identity,
            "managed_node_objects": count_named_files(&runtime_root.join("objects"), ".complete"),
            "managed_node_state_entries": count_child_directories(&runtime_root.join(".state")),
        }
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&payload).map_err(|_| maintenance_error("status.serialize"))?
    );
    Ok(())
}

fn clean(data_dir: &Path, apply: bool) -> Result<(), CliBoundaryError> {
    let candidates = collect_cleanup_candidates(data_dir);
    let total_bytes = candidates.iter().map(|candidate| candidate.bytes).sum::<u64>();
    let mut removed = 0usize;
    let _guard = if apply {
        let db_path = data_dir.join("aionui-backend.db");
        match aionui_db::DataDirInstanceGuard::try_acquire(&db_path) {
            Ok(Some(guard)) => Some(guard),
            Ok(None) => return Err(maintenance_error("clean.server_running")),
            Err(_) => return Err(maintenance_error("clean.lock_unavailable")),
        }
    } else {
        None
    };

    if apply {
        for candidate in &candidates {
            remove_candidate(&candidate.path).map_err(|_| maintenance_error("clean.remove"))?;
            removed += 1;
        }
    }

    let items = candidates
        .iter()
        .map(|candidate| {
            json!({
                "path": candidate.relative_path,
                "reason": candidate.reason,
                "bytes": candidate.bytes,
            })
        })
        .collect::<Vec<_>>();
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "schema_version": 1,
            "mode": if apply { "apply" } else { "dry-run" },
            "candidate_count": candidates.len(),
            "removed_count": removed,
            "reclaimable_bytes": total_bytes,
            "items": items,
            "protected": ["aionui-backend.db", "user configuration", "installed extensions", "active builtin skills"],
        }))
        .map_err(|_| maintenance_error("clean.serialize"))?
    );
    Ok(())
}

fn collect_cleanup_candidates(data_dir: &Path) -> Vec<CleanupCandidate> {
    let mut candidates = Vec::new();
    let current_builtin = fs::read_to_string(data_dir.join(".builtin-skills.current")).ok();
    collect_children_except(
        data_dir,
        &data_dir.join(".builtin-skills.objects"),
        current_builtin.as_deref(),
        "unreferenced_builtin_skills_object",
        &mut candidates,
    );
    for name in [".builtin-skills.tmp", ".builtin-skills.old", ".builtin-skills.lock"] {
        push_candidate(
            data_dir,
            data_dir.join(name),
            "legacy_builtin_skills_artifact",
            &mut candidates,
        );
    }

    let extensions = data_dir.join("extensions");
    push_candidate(
        data_dir,
        extensions.join("index.json"),
        "regenerable_hub_index",
        &mut candidates,
    );
    collect_named_children(
        data_dir,
        &extensions,
        ".hub-install-",
        "failed_hub_staging",
        &mut candidates,
    );

    let node = data_dir.join("runtime").join("node");
    push_candidate(
        data_dir,
        node.join("objects"),
        "managed_node_content_cache",
        &mut candidates,
    );
    push_candidate(
        data_dir,
        node.join(".state"),
        "managed_node_writable_cache",
        &mut candidates,
    );
    collect_named_children(
        data_dir,
        &node,
        "node-v",
        "legacy_managed_node_version",
        &mut candidates,
    );
    collect_suffix_children(
        data_dir,
        &node,
        ".download",
        "managed_node_download_cache",
        &mut candidates,
    );
    candidates
}

fn collect_automatic_candidates(data_dir: &Path) -> Vec<CleanupCandidate> {
    let all = collect_cleanup_candidates(data_dir);
    all.into_iter()
        .filter(|candidate| {
            matches!(
                candidate.reason,
                "legacy_builtin_skills_artifact" | "failed_hub_staging" | "managed_node_download_cache"
            ) && is_older_than(&candidate.path, AUTOMATIC_GC_MIN_AGE)
        })
        .collect()
}

fn collect_children_except(
    data_dir: &Path,
    parent: &Path,
    protected_name: Option<&str>,
    reason: &'static str,
    out: &mut Vec<CleanupCandidate>,
) {
    let Ok(entries) = fs::read_dir(parent) else { return };
    for entry in entries.flatten() {
        if entry.file_name().to_str() == protected_name {
            continue;
        }
        push_candidate(data_dir, entry.path(), reason, out);
    }
}

fn collect_named_children(
    data_dir: &Path,
    parent: &Path,
    prefix: &str,
    reason: &'static str,
    out: &mut Vec<CleanupCandidate>,
) {
    let Ok(entries) = fs::read_dir(parent) else { return };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(prefix) {
            push_candidate(data_dir, entry.path(), reason, out);
        }
    }
}

fn collect_suffix_children(
    data_dir: &Path,
    parent: &Path,
    suffix: &str,
    reason: &'static str,
    out: &mut Vec<CleanupCandidate>,
) {
    let Ok(entries) = fs::read_dir(parent) else { return };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().ends_with(suffix) {
            push_candidate(data_dir, entry.path(), reason, out);
        }
    }
}

fn push_candidate(data_dir: &Path, path: PathBuf, reason: &'static str, out: &mut Vec<CleanupCandidate>) {
    if fs::symlink_metadata(&path).is_err() {
        return;
    }
    let Ok(relative) = path.strip_prefix(data_dir) else {
        return;
    };
    out.push(CleanupCandidate {
        bytes: path_bytes(&path),
        relative_path: relative.to_string_lossy().replace('\\', "/"),
        path,
        reason,
    });
}

fn remove_candidate(path: &Path) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

fn path_bytes(path: &Path) -> u64 {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return 0;
    };
    if !metadata.file_type().is_dir() {
        return metadata.len();
    }
    walkdir::WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter_map(|entry| entry.metadata().ok())
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.len())
        .sum()
}

fn inspect_instance_lock(db_path: &Path) -> &'static str {
    let lock_path = aionui_db::instance_lock_path(db_path);
    let Ok(file) = fs::File::open(lock_path) else {
        return "none";
    };
    match FileExt::try_lock_exclusive(&file) {
        Ok(()) => {
            let _ = FileExt::unlock(&file);
            "none"
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => "held",
        Err(_) => "unknown",
    }
}

fn count_child_directories(path: &Path) -> usize {
    fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .count()
}

fn count_named_files(path: &Path, name: &str) -> usize {
    walkdir::WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file() && entry.file_name() == name)
        .count()
}

fn local_host(host: &str) -> &str {
    match host {
        "0.0.0.0" | "::" => "127.0.0.1",
        other => other,
    }
}

fn is_older_than(path: &Path, age: Duration) -> bool {
    modified_at(path).elapsed().is_ok_and(|elapsed| elapsed >= age)
}

fn modified_at(path: &Path) -> SystemTime {
    fs::symlink_metadata(path)
        .and_then(|metadata| metadata.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn maintenance_error(stage: &'static str) -> CliBoundaryError {
    CliBoundaryError::new(
        CliBoundaryCode::CliMaintenanceFailed,
        SUBCOMMAND,
        "maintenance operation failed",
    )
    .with_field("stage", stage)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_dry_run_protects_business_data_and_active_builtin_object() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path();
        fs::write(data.join("aionui-backend.db"), b"business").unwrap();
        fs::write(data.join("config.json"), b"config").unwrap();
        fs::write(data.join(".builtin-skills.current"), "sha256-current").unwrap();
        fs::create_dir_all(data.join(".builtin-skills.objects/sha256-current")).unwrap();
        fs::create_dir_all(data.join(".builtin-skills.objects/sha256-old")).unwrap();
        fs::create_dir_all(data.join("extensions/installed-ext")).unwrap();
        fs::write(data.join("extensions/index.json"), b"index").unwrap();

        let candidates = collect_cleanup_candidates(data);
        let paths = candidates
            .iter()
            .map(|candidate| candidate.relative_path.as_str())
            .collect::<Vec<_>>();
        assert!(paths.contains(&".builtin-skills.objects/sha256-old"));
        assert!(paths.contains(&"extensions/index.json"));
        assert!(!paths.iter().any(|path| path.contains("sha256-current")));
        assert!(!paths.iter().any(|path| path.contains("aionui-backend.db")));
        assert!(!paths.iter().any(|path| path.contains("config.json")));
        assert!(!paths.iter().any(|path| path.contains("installed-ext")));
    }

    #[test]
    fn automatic_gc_only_selects_old_staging_artifacts() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path();
        fs::create_dir_all(data.join("extensions/.hub-install-stale")).unwrap();
        fs::create_dir_all(data.join("runtime/node/objects/sha256-current")).unwrap();

        let candidates = collect_automatic_candidates(data);
        assert!(
            candidates.is_empty(),
            "fresh staging is protected by the automatic GC grace period"
        );
    }

    #[test]
    fn clean_apply_removes_only_reported_regenerable_entries() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path();
        fs::write(data.join("aionui-backend.db"), b"business").unwrap();
        fs::write(data.join("config.json"), b"config").unwrap();
        fs::create_dir_all(data.join("runtime/node/node-v24.11.0-test")).unwrap();
        fs::write(data.join("runtime/node/node-v24.11.0-test/node"), b"runtime").unwrap();

        clean(data, true).expect("apply clean");

        assert!(!data.join("runtime/node/node-v24.11.0-test").exists());
        assert_eq!(fs::read(data.join("aionui-backend.db")).unwrap(), b"business");
        assert_eq!(fs::read(data.join("config.json")).unwrap(), b"config");
    }
}

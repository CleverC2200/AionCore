use std::fs;
use std::path::{Component, Path, PathBuf};
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
        match remove_candidate(data_dir, &candidate.path) {
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
    // The active resource reference must be sampled after excluding a running
    // server, otherwise startup could activate an object already selected here.
    let candidates = collect_cleanup_candidates(data_dir);
    let total_bytes = candidates.iter().map(|candidate| candidate.bytes).sum::<u64>();
    let mut removed = 0usize;

    if apply {
        for candidate in &candidates {
            remove_candidate(data_dir, &candidate.path).map_err(|_| maintenance_error("clean.remove"))?;
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
    let Ok(data_root) = fs::canonicalize(data_dir) else {
        return candidates;
    };
    let data_dir = data_root.as_path();
    // An unreadable, missing, or incomplete reference does not prove that any
    // object is unreferenced. Retain the whole corpus until identity is known.
    if let Some(current_builtin) = active_builtin_identity(data_dir) {
        collect_children_except(
            data_dir,
            &data_dir.join(".builtin-skills.objects"),
            Some(&current_builtin),
            "unreferenced_builtin_skills_object",
            &mut candidates,
        );
    }
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
    if !owned_directory(data_dir, parent) {
        return;
    }
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
    if !owned_directory(data_dir, parent) {
        return;
    }
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
    if !owned_directory(data_dir, parent) {
        return;
    }
    let Ok(entries) = fs::read_dir(parent) else { return };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().ends_with(suffix) {
            push_candidate(data_dir, entry.path(), reason, out);
        }
    }
}

fn push_candidate(data_dir: &Path, path: PathBuf, reason: &'static str, out: &mut Vec<CleanupCandidate>) {
    if owned_metadata(data_dir, &path).is_err() {
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

fn remove_candidate(data_dir: &Path, path: &Path) -> std::io::Result<()> {
    let data_root = fs::canonicalize(data_dir)?;
    // Recheck immediately before deletion: an ancestor may have been replaced
    // since collection. A direct symlink is safe to unlink, never to traverse.
    let metadata = owned_metadata(&data_root, path)?;
    if metadata.file_type().is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

fn owned_metadata(data_root: &Path, path: &Path) -> std::io::Result<fs::Metadata> {
    let outside_root = || std::io::Error::new(std::io::ErrorKind::PermissionDenied, "unsafe cleanup path");
    let relative = path.strip_prefix(data_root).map_err(|_| outside_root())?;
    if relative.as_os_str().is_empty() || fs::canonicalize(data_root)? != data_root {
        return Err(outside_root());
    }
    let mut current = data_root.to_path_buf();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(outside_root());
        };
        current.push(name);
        let metadata = fs::symlink_metadata(&current)?;
        if components.peek().is_none() {
            return Ok(metadata);
        }
        // Reject links even when they point elsewhere inside the data root:
        // their target may be protected business data rather than this cache.
        if !metadata.file_type().is_dir() {
            return Err(outside_root());
        }
    }
    Err(outside_root())
}

fn owned_directory(data_root: &Path, path: &Path) -> bool {
    owned_metadata(data_root, path).is_ok_and(|metadata| metadata.file_type().is_dir())
}

fn active_builtin_identity(data_root: &Path) -> Option<String> {
    let read_owned_file = |path: &Path| {
        owned_metadata(data_root, path)
            .ok()
            .filter(|metadata| metadata.file_type().is_file())?;
        fs::read_to_string(path).ok()
    };
    let identity = read_owned_file(&data_root.join(".builtin-skills.current"))?;
    let mut components = Path::new(&identity).components();
    if !matches!(components.next(), Some(Component::Normal(name)) if name.to_str() == Some(identity.as_str()))
        || components.next().is_some()
    {
        return None;
    }
    let complete = data_root
        .join(".builtin-skills.objects")
        .join(&identity)
        .join(".complete");
    (read_owned_file(&complete)? == identity).then_some(identity)
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
    fn clean_protects_business_data_and_active_builtin_object() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path();
        fs::write(data.join("aionui-backend.db"), b"business").unwrap();
        fs::write(data.join("config.json"), b"config").unwrap();
        fs::write(data.join(".builtin-skills.current"), "sha256-current").unwrap();
        fs::create_dir_all(data.join(".builtin-skills.objects/sha256-current")).unwrap();
        fs::write(
            data.join(".builtin-skills.objects/sha256-current/.complete"),
            "sha256-current",
        )
        .unwrap();
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

        clean(data, false).expect("dry-run");
        assert!(data.join(".builtin-skills.objects/sha256-old").is_dir());
        clean(data, true).expect("remove only unreferenced objects");
        assert!(!data.join(".builtin-skills.objects/sha256-old").exists());
        assert!(data.join(".builtin-skills.objects/sha256-current/.complete").is_file());
        assert!(data.join("extensions/installed-ext").is_dir());
        assert_eq!(fs::read(data.join("aionui-backend.db")).unwrap(), b"business");
        assert_eq!(fs::read(data.join("config.json")).unwrap(), b"config");
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

    #[test]
    fn clean_preserves_builtin_objects_when_active_identity_is_unknown() {
        for marker in [
            None,
            Some("../business"),
            Some("sha256-missing"),
            Some("sha256-current"),
            Some("sha256-current/"),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let data = temp.path();
            let object = data.join(".builtin-skills.objects/sha256-current");
            fs::create_dir_all(&object).unwrap();
            fs::write(object.join("SKILL.md"), b"retained skill").unwrap();
            // Even a syntactically valid reference cannot authorize GC without
            // the corresponding completed object.
            if let Some(marker) = marker {
                fs::write(data.join(".builtin-skills.current"), marker).unwrap();
            }
            fs::create_dir_all(data.join("extensions")).unwrap();
            fs::write(data.join("extensions/index.json"), b"cache").unwrap();

            clean(data, true).expect("clean with unknown active identity");

            assert_eq!(fs::read(object.join("SKILL.md")).unwrap(), b"retained skill");
            assert!(!data.join("extensions/index.json").exists());
        }
    }

    #[test]
    fn clean_apply_refuses_instance_lock_conflicts_without_removing_cache() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path();
        let cache = data.join(".builtin-skills.tmp");
        fs::write(&cache, b"retained cache").unwrap();
        let _guard = aionui_db::DataDirInstanceGuard::try_acquire(&data.join("aionui-backend.db"))
            .unwrap()
            .expect("own the instance lock");

        clean(data, false).expect("dry-run remains available while running");
        let error = clean(data, true).expect_err("running server prevents cleanup");

        assert_eq!(error, maintenance_error("clean.server_running"));
        assert_eq!(fs::read(cache).unwrap(), b"retained cache");
    }

    #[cfg(unix)]
    #[test]
    fn clean_never_traverses_symlink_ancestors_inside_or_outside_data_root() {
        use std::os::unix::fs::symlink;

        for target_inside_data in [false, true] {
            for linked_parent in ["extensions", "runtime", ".builtin-skills.objects"] {
                let temp = tempfile::tempdir().unwrap();
                let data = temp.path().join("data");
                fs::create_dir(&data).unwrap();
                let target = if target_inside_data {
                    data.join("business")
                } else {
                    temp.path().join("business")
                };
                fs::create_dir_all(target.join("node/objects")).unwrap();
                fs::write(target.join("index.json"), b"business index").unwrap();
                let staging = target.join(".hub-install-old");
                fs::write(&staging, b"business staging").unwrap();
                fs::File::open(&staging)
                    .unwrap()
                    .set_modified(SystemTime::UNIX_EPOCH)
                    .unwrap();
                fs::write(target.join("node/objects/data"), b"business runtime").unwrap();
                symlink(&target, data.join(linked_parent)).unwrap();
                // A valid reference cannot make a symlinked objects root safe.
                if linked_parent == ".builtin-skills.objects" {
                    fs::create_dir(target.join("sha256-current")).unwrap();
                    fs::write(target.join("sha256-current/.complete"), "sha256-current").unwrap();
                    fs::write(data.join(".builtin-skills.current"), "sha256-current").unwrap();
                }

                assert!(collect_cleanup_candidates(&data).is_empty());
                run_automatic_maintenance(&data);
                clean(&data, true).expect("skip unsafe ancestors");

                assert!(fs::symlink_metadata(data.join(linked_parent)).unwrap().is_symlink());
                assert_eq!(fs::read(target.join("index.json")).unwrap(), b"business index");
                assert_eq!(fs::read(staging).unwrap(), b"business staging");
                assert_eq!(fs::read(target.join("node/objects/data")).unwrap(), b"business runtime");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn clean_unlinks_direct_file_directory_and_dangling_candidates_only() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let business = temp.path().join("business");
        fs::create_dir_all(data.join("extensions")).unwrap();
        fs::create_dir_all(data.join("runtime/node/objects")).unwrap();
        fs::create_dir(&business).unwrap();
        fs::write(business.join("index.json"), b"business").unwrap();
        let file_link = data.join("extensions/index.json");
        let directory_link = data.join("runtime/node/node-v24-linked");
        let dangling_link = data.join(".builtin-skills.tmp");
        symlink(business.join("index.json"), &file_link).unwrap();
        symlink(&business, &directory_link).unwrap();
        symlink(temp.path().join("missing"), &dangling_link).unwrap();
        // Descendant symlinks of a removable directory must also leave their
        // targets alone when remove_dir_all removes the containing cache.
        symlink(&business, data.join("runtime/node/objects/business-link")).unwrap();

        clean(&data, true).expect("unlink direct candidates");

        for link in [file_link, directory_link, dangling_link] {
            assert_eq!(
                fs::symlink_metadata(link).unwrap_err().kind(),
                std::io::ErrorKind::NotFound
            );
        }
        assert!(!data.join("runtime/node/objects").exists());
        assert_eq!(fs::read(business.join("index.json")).unwrap(), b"business");
    }

    #[cfg(unix)]
    #[test]
    fn removal_rechecks_ancestor_replaced_after_collection() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let business = temp.path().join("business");
        fs::create_dir_all(data.join("extensions")).unwrap();
        fs::create_dir(&business).unwrap();
        fs::write(data.join("extensions/index.json"), b"cache").unwrap();
        fs::write(business.join("index.json"), b"business").unwrap();
        let candidate = collect_cleanup_candidates(&data).pop().expect("one index candidate");
        fs::rename(data.join("extensions"), data.join("old-extensions")).unwrap();
        symlink(&business, data.join("extensions")).unwrap();

        let error = remove_candidate(&data, &candidate.path).expect_err("reject replaced ancestor");

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(fs::read(business.join("index.json")).unwrap(), b"business");
        assert_eq!(fs::read(data.join("old-extensions/index.json")).unwrap(), b"cache");
    }
}

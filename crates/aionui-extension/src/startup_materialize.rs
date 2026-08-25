//! Startup-time materialization of the embedded builtin skills corpus.
//!
//! Each corpus is stored once under a content-addressed object directory.
//! Files converge independently through temp-file + rename transactions; a
//! completion marker is written last, then a small current-ref file is
//! atomically replaced. Interrupted starts resume the same object without
//! rewriting an already-valid tree or holding a global materialization lock.

use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use include_dir::Dir;
use tracing::{info, warn};

use crate::error::ExtensionError;

const LEGACY_VERSION_FILE: &str = ".version";
const OBJECTS_DIR_NAME: &str = ".builtin-skills.objects";
const CURRENT_REF_FILE_NAME: &str = ".builtin-skills.current";
const COMPLETE_FILE_NAME: &str = ".complete";

const STARTUP_FILE_RETRY_DELAYS: [Duration; 5] = [
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
];

/// Decide whether to materialize based on the active content identity.
/// Returns `true` if a write happened, `false` if the gate said "skip".
///
/// When `BUILTIN_SKILLS_ENV_VAR` is set and non-empty, the caller has
/// already routed `builtin_skills_dir` at the env-var path — this
/// function still runs but the gate will see whatever version the dev
/// tree has on disk (or missing, and materialize into that dev path,
/// which is wrong). Callers MUST check the env var before calling.
pub async fn materialize_if_needed(
    data_dir: &Path,
    corpus: &Dir<'static>,
    binary_version: &str,
) -> Result<bool, ExtensionError> {
    if active_object_matches(data_dir, binary_version).await {
        info!(
            identity = binary_version,
            "builtin skills up to date; skipping materialize"
        );
        return Ok(false);
    }

    info!(identity = binary_version, "materializing embedded builtin skills");

    match materialize_embedded_builtin_skills_unlocked(data_dir, corpus, binary_version).await {
        Ok(()) => {}
        Err(e) if existing_builtin_skills_looks_usable(data_dir).await => {
            warn!(
                identity = binary_version,
                error = %e,
                "failed to refresh builtin skills; continuing with existing tree"
            );
            return Ok(false);
        }
        Err(e) => return Err(e),
    }
    Ok(true)
}

/// Unconditional materialize of a content-addressed object and current ref.
/// Exposed separately for tests that want to bypass the gate.
pub async fn materialize_embedded_builtin_skills(
    data_dir: &Path,
    corpus: &Dir<'static>,
    binary_version: &str,
) -> Result<(), ExtensionError> {
    materialize_embedded_builtin_skills_unlocked(data_dir, corpus, binary_version).await
}

async fn materialize_embedded_builtin_skills_unlocked(
    data_dir: &Path,
    corpus: &Dir<'static>,
    binary_version: &str,
) -> Result<(), ExtensionError> {
    validate_content_identity(binary_version)?;
    let object = object_dir(data_dir, binary_version);

    tokio::fs::create_dir_all(data_dir).await?;
    tokio::fs::create_dir_all(&object).await?;

    if !object_complete(&object, binary_version).await {
        write_dir_recursive(corpus, &object).await?;
        prune_unexpected_object_entries(corpus, &object).await?;
        atomic_write(&object.join(COMPLETE_FILE_NAME), binary_version.as_bytes()).await?;
    }
    atomic_write(&data_dir.join(CURRENT_REF_FILE_NAME), binary_version.as_bytes()).await?;

    Ok(())
}

async fn active_object_matches(data_dir: &Path, expected: &str) -> bool {
    match tokio::fs::read_to_string(data_dir.join(CURRENT_REF_FILE_NAME)).await {
        Ok(current) if current == expected => object_complete(&object_dir(data_dir, expected), expected).await,
        _ => false,
    }
}

async fn object_complete(object: &Path, identity: &str) -> bool {
    matches!(tokio::fs::read_to_string(object.join(COMPLETE_FILE_NAME)).await, Ok(marker) if marker == identity)
}

fn object_dir(data_dir: &Path, identity: &str) -> PathBuf {
    data_dir.join(OBJECTS_DIR_NAME).join(identity)
}

fn validate_content_identity(identity: &str) -> Result<(), ExtensionError> {
    if identity.is_empty()
        || matches!(identity, "." | "..")
        || !identity
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(ExtensionError::ManifestValidation(format!(
            "Invalid builtin skills content identity: {identity}"
        )));
    }
    Ok(())
}

async fn existing_builtin_skills_looks_usable(data_dir: &Path) -> bool {
    if let Ok(current) = tokio::fs::read_to_string(data_dir.join(CURRENT_REF_FILE_NAME)).await
        && validate_content_identity(&current).is_ok()
        && object_complete(&object_dir(data_dir, &current), &current).await
    {
        return true;
    }
    data_dir
        .join(crate::constants::BUILTIN_SKILLS_DIR_NAME)
        .join(LEGACY_VERSION_FILE)
        .is_file()
}

async fn retry_startup_file_op<T, F, Fut>(operation: &str, path: &Path, mut op: F) -> std::io::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = std::io::Result<T>>,
{
    for (attempt, delay) in STARTUP_FILE_RETRY_DELAYS.iter().enumerate() {
        match op().await {
            Ok(value) => return Ok(value),
            Err(e) if is_retryable_startup_file_error(&e) => {
                warn!(
                    operation,
                    path = %path.display(),
                    attempt = attempt + 1,
                    retry_after_ms = delay.as_millis(),
                    raw_os_error = ?e.raw_os_error(),
                    error = %e,
                    "Startup file operation failed; retrying"
                );
                tokio::time::sleep(*delay).await;
            }
            Err(e) => return Err(e),
        }
    }
    op().await
}

fn is_retryable_startup_file_error(error: &std::io::Error) -> bool {
    match error.kind() {
        std::io::ErrorKind::Interrupted
        | std::io::ErrorKind::PermissionDenied
        | std::io::ErrorKind::TimedOut
        | std::io::ErrorKind::WouldBlock => true,
        _ => matches!(error.raw_os_error(), Some(5 | 32 | 33)),
    }
}

/// Recursively copy every file in an `include_dir::Dir` tree into `dest`.
/// Directories are created as needed. Matching files are reused; changed or
/// missing files converge through a task-owned temporary file and rename.
async fn write_dir_recursive(dir: &Dir<'static>, dest: &Path) -> Result<(), ExtensionError> {
    // The include_dir API is synchronous; we flatten into a Vec then
    // feed the writes through tokio::fs to stay off the reactor's thread
    // for big IO bursts.
    let mut stack: Vec<(&Dir<'static>, PathBuf)> = vec![(dir, dest.to_path_buf())];
    while let Some((d, prefix)) = stack.pop() {
        for file in d.files() {
            let rel = file.path();
            let out_path = prefix.join(rel.strip_prefix(d.path()).unwrap_or(rel));
            if let Some(parent) = out_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            if matches!(tokio::fs::read(&out_path).await, Ok(existing) if existing == file.contents()) {
                continue;
            }
            atomic_write(&out_path, file.contents()).await?;
        }
        for sub in d.dirs() {
            let sub_rel = sub.path();
            let sub_dest = prefix.join(sub_rel.strip_prefix(d.path()).unwrap_or(sub_rel));
            tokio::fs::create_dir_all(&sub_dest).await?;
            stack.push((sub, sub_dest));
        }
    }
    Ok(())
}

async fn prune_unexpected_object_entries(corpus: &Dir<'static>, object: &Path) -> Result<(), ExtensionError> {
    let mut expected = HashSet::new();
    collect_expected_paths(corpus, corpus.path(), &mut expected);

    let mut entries = walkdir::WalkDir::new(object)
        .min_depth(1)
        .follow_links(false)
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| ExtensionError::Io(std::io::Error::other(error)))?;
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.depth()));

    for entry in entries {
        let path = entry.path();
        let relative = path.strip_prefix(object).unwrap_or(path);
        let is_in_flight_temp = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with('.') && name.ends_with(".tmp"));
        if entry.file_type().is_file() && !expected.contains(relative) && !is_in_flight_temp {
            tokio::fs::remove_file(path).await?;
        } else if entry.file_type().is_dir() {
            match tokio::fs::remove_dir(path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(())
}

fn collect_expected_paths(dir: &Dir<'static>, root: &Path, expected: &mut HashSet<PathBuf>) {
    for file in dir.files() {
        expected.insert(file.path().strip_prefix(root).unwrap_or(file.path()).to_path_buf());
    }
    for subdir in dir.dirs() {
        collect_expected_paths(subdir, root, expected);
    }
}

async fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), ExtensionError> {
    let parent = path
        .parent()
        .ok_or_else(|| ExtensionError::Io(std::io::Error::other("materialized path has no parent")))?;
    tokio::fs::create_dir_all(parent).await?;
    let file_name = path.file_name().and_then(|name| name.to_str()).unwrap_or("resource");
    let sequence = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(".{file_name}.{}.{}.tmp", std::process::id(), sequence));
    tokio::fs::write(&temp, bytes).await?;
    if let Err(error) = retry_startup_file_op("activate builtin skills file", path, || replace_file(&temp, path)).await
    {
        let _ = tokio::fs::remove_file(&temp).await;
        return Err(error.into());
    }
    Ok(())
}

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[cfg(not(windows))]
async fn replace_file(temp: &Path, target: &Path) -> std::io::Result<()> {
    tokio::fs::rename(temp, target).await
}

#[cfg(windows)]
async fn replace_file(temp: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW};

    let temp = temp.to_path_buf();
    let target = target.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let source = temp.as_os_str().encode_wide().chain(Some(0)).collect::<Vec<_>>();
        let destination = target.as_os_str().encode_wide().chain(Some(0)).collect::<Vec<_>>();
        let result = unsafe {
            MoveFileExW(
                source.as_ptr(),
                destination.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if result == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    })
    .await
    .map_err(|error| std::io::Error::other(format!("builtin skills replace task failed: {error}")))?
}

/// Resolve the active content-addressed corpus, falling back to the legacy
/// projection during migration or after an interrupted current-ref update.
pub fn resolve_materialized_builtin_skills_dir(data_dir: &Path) -> PathBuf {
    if let Ok(identity) = std::fs::read_to_string(data_dir.join(CURRENT_REF_FILE_NAME))
        && validate_content_identity(&identity).is_ok()
    {
        let object = object_dir(data_dir, &identity);
        if matches!(
            std::fs::read_to_string(object.join(COMPLETE_FILE_NAME)),
            Ok(marker) if marker == identity
        ) {
            return object;
        }
    }
    data_dir.join(crate::constants::BUILTIN_SKILLS_DIR_NAME)
}

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{OnceLock, RwLock};

use sha2::{Digest, Sha256};
use walkdir::WalkDir;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedResourcesMode {
    Bundled,
    Download,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedResourceSourceKind {
    Bundled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedResourceSource {
    pub kind: ManagedResourceSourceKind,
    pub root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializedDirectory {
    pub root: PathBuf,
    pub content_sha256: String,
}

const BUNDLED_RESOURCES_ENV: &str = "AIONUI_BUNDLED_MANAGED_RESOURCES";

pub fn set_managed_resources_mode(mode: ManagedResourcesMode) {
    *mode_lock().write().expect("managed resources mode lock poisoned") = mode;
}

pub fn managed_resources_mode() -> ManagedResourcesMode {
    *mode_lock().read().expect("managed resources mode lock poisoned")
}

pub fn bundled_root_path() -> Option<PathBuf> {
    bundled_root().filter(|root| root.is_dir())
}

pub fn bundled_root_candidate() -> Option<PathBuf> {
    bundled_root()
}

pub fn requires_bundled_resources() -> bool {
    matches!(managed_resources_mode(), ManagedResourcesMode::Bundled)
}

pub fn node_sources(directory_name: &str) -> Vec<ManagedResourceSource> {
    resource_roots()
        .into_iter()
        .map(|source| ManagedResourceSource {
            root: source.root.join("node").join(directory_name),
            ..source
        })
        .filter(|source| source.root.is_dir())
        .collect()
}

pub fn cli_sources(name: &str, version: &str, target: &str) -> Vec<ManagedResourceSource> {
    resource_roots()
        .into_iter()
        .map(|source| ManagedResourceSource {
            root: source.root.join("cli").join(name).join(version).join(target),
            ..source
        })
        .filter(|source| source.root.is_dir())
        .collect()
}

pub fn export_cli_to_root(
    root: &Path,
    source_root: &Path,
    name: &str,
    version: &str,
    target: &str,
) -> std::io::Result<PathBuf> {
    let target_dir = root.join("cli").join(name).join(version).join(target);
    materialize_directory(source_root, &target_dir)?;
    Ok(target_dir)
}

pub fn export_node_runtime_to_root(root: &Path, source_root: &Path, directory_name: &str) -> std::io::Result<PathBuf> {
    let target = root.join("node").join(directory_name);
    materialize_directory(source_root, &target)?;
    Ok(target)
}

pub fn export_acp_tool_to_root(
    root: &Path,
    source_root: &Path,
    tool_slug: &str,
    version: &str,
    platform_key: &str,
) -> std::io::Result<PathBuf> {
    let target = root.join("acp").join(tool_slug).join(version).join(platform_key);
    materialize_directory(source_root, &target)?;
    Ok(target)
}

pub fn materialize_directory(source_root: &Path, target_root: &Path) -> std::io::Result<()> {
    if !source_root.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("managed resource source missing: {}", source_root.display()),
        ));
    }

    if source_root == target_root {
        return Ok(());
    }
    if let (Ok(source), Ok(target)) = (fs::canonicalize(source_root), fs::canonicalize(target_root))
        && source == target
    {
        return Ok(());
    }

    fs::create_dir_all(target_root)?;

    let mut expected = std::collections::HashSet::new();

    for entry in WalkDir::new(source_root) {
        let entry = entry?;
        let relative = entry
            .path()
            .strip_prefix(source_root)
            .expect("walkdir path should stay under source root");

        if relative.as_os_str().is_empty() {
            continue;
        }
        expected.insert(relative.to_path_buf());

        let target_path = target_root.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target_path)?;
            copy_permissions(entry.path(), &target_path)?;
            continue;
        }

        if entry.file_type().is_symlink() {
            if let Some(parent) = target_path.parent() {
                fs::create_dir_all(parent)?;
            }
            converge_symlink(entry.path(), &target_path)?;
            continue;
        }

        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent)?;
        }
        converge_file(entry.path(), &target_path)?;
    }

    prune_unexpected_entries(target_root, &expected)?;

    Ok(())
}

pub fn materialize_content_addressed_directory(
    source_root: &Path,
    objects_root: &Path,
) -> std::io::Result<MaterializedDirectory> {
    let content_sha256 = hash_directory(source_root)?;
    let identity = format!("sha256-{content_sha256}");
    let root = objects_root.join(&identity);
    let complete = root.join(".complete");

    if !matches!(fs::read_to_string(&complete), Ok(marker) if marker == identity) {
        materialize_directory(source_root, &root)?;
        atomic_write(&complete, identity.as_bytes())?;
    }

    Ok(MaterializedDirectory { root, content_sha256 })
}

fn resource_roots() -> Vec<ManagedResourceSource> {
    let mut roots = Vec::new();

    match managed_resources_mode() {
        ManagedResourcesMode::Bundled => {
            if let Some(root) = bundled_root()
                && root.is_dir()
            {
                roots.push(ManagedResourceSource {
                    kind: ManagedResourceSourceKind::Bundled,
                    root,
                });
            }
        }
        ManagedResourcesMode::Download => {}
    }

    roots
}

fn mode_lock() -> &'static RwLock<ManagedResourcesMode> {
    static MODE: OnceLock<RwLock<ManagedResourcesMode>> = OnceLock::new();
    MODE.get_or_init(|| RwLock::new(default_managed_resources_mode()))
}

fn default_managed_resources_mode() -> ManagedResourcesMode {
    ManagedResourcesMode::Download
}

fn bundled_root() -> Option<PathBuf> {
    configured_root(BUNDLED_RESOURCES_ENV).or_else(default_bundled_root)
}

fn configured_root(env_key: &str) -> Option<PathBuf> {
    std::env::var_os(env_key)
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

fn default_bundled_root() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe_dir = fs::canonicalize(exe).ok()?.parent()?.to_path_buf();
    Some(exe_dir.join("managed-resources"))
}

fn copy_permissions(source: &Path, target: &Path) -> std::io::Result<()> {
    let metadata = fs::metadata(source)?;
    fs::set_permissions(target, metadata.permissions())
}

fn converge_file(source: &Path, target: &Path) -> std::io::Result<()> {
    if files_match(source, target)? {
        return Ok(());
    }
    if target.is_dir() {
        fs::remove_dir_all(target)?;
    }
    let temp = temp_path(target);
    fs::copy(source, &temp)?;
    copy_permissions(source, &temp)?;
    replace_file(&temp, target).inspect_err(|_| {
        let _ = fs::remove_file(&temp);
    })
}

fn files_match(source: &Path, target: &Path) -> std::io::Result<bool> {
    if !target.is_file() {
        return Ok(false);
    }
    let source_metadata = fs::metadata(source)?;
    let target_metadata = fs::metadata(target)?;
    if source_metadata.len() != target_metadata.len() || source_metadata.permissions() != target_metadata.permissions()
    {
        return Ok(false);
    }
    Ok(fs::read(source)? == fs::read(target)?)
}

fn converge_symlink(source: &Path, target: &Path) -> std::io::Result<()> {
    let link_target = fs::read_link(source)?;
    if matches!(fs::read_link(target), Ok(existing) if existing == link_target) {
        return Ok(());
    }
    remove_path_if_exists(target)?;
    let temp = temp_path(target);
    create_symlink(&link_target, &temp, source)?;
    replace_file(&temp, target).inspect_err(|_| {
        let _ = fs::remove_file(&temp);
    })
}

fn prune_unexpected_entries(target_root: &Path, expected: &std::collections::HashSet<PathBuf>) -> std::io::Result<()> {
    let mut entries = WalkDir::new(target_root)
        .min_depth(1)
        .follow_links(false)
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .map_err(std::io::Error::other)?;
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.depth()));
    for entry in entries {
        let relative = entry
            .path()
            .strip_prefix(target_root)
            .expect("walkdir path should stay under target root");
        let is_in_flight_temp = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with('.') && name.ends_with(".tmp"));
        if expected.contains(relative) || is_in_flight_temp {
            continue;
        }
        if entry.file_type().is_dir() {
            match fs::remove_dir(entry.path()) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
                Err(error) => return Err(error),
            }
        } else {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

fn hash_directory(root: &Path) -> std::io::Result<String> {
    if !root.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("managed resource source missing: {}", root.display()),
        ));
    }
    let mut entries = WalkDir::new(root)
        .min_depth(1)
        .follow_links(false)
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .map_err(std::io::Error::other)?;
    entries.sort_by_key(|entry| entry.path().strip_prefix(root).unwrap_or(entry.path()).to_path_buf());

    let mut hasher = Sha256::new();
    for entry in entries {
        let relative = entry
            .path()
            .strip_prefix(root)
            .expect("walkdir path should stay under root");
        hasher.update(relative.to_string_lossy().replace('\\', "/").as_bytes());
        hasher.update([0]);
        if entry.file_type().is_dir() {
            hasher.update(b"dir");
        } else if entry.file_type().is_symlink() {
            hasher.update(b"symlink");
            hasher.update(fs::read_link(entry.path())?.to_string_lossy().as_bytes());
        } else {
            hasher.update(b"file");
            let bytes = fs::read(entry.path())?;
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
            hash_permissions(&mut hasher, &fs::metadata(entry.path())?.permissions());
        }
        hasher.update([0xff]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(unix)]
fn hash_permissions(hasher: &mut Sha256, permissions: &fs::Permissions) {
    use std::os::unix::fs::PermissionsExt;
    hasher.update((permissions.mode() & 0o777).to_le_bytes());
}

#[cfg(not(unix))]
fn hash_permissions(hasher: &mut Sha256, permissions: &fs::Permissions) {
    hasher.update([u8::from(permissions.readonly())]);
}

fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("managed resource path has no parent"))?;
    fs::create_dir_all(parent)?;
    let temp = temp_path(path);
    {
        let mut writer = fs::File::create(&temp)?;
        writer.write_all(bytes)?;
        writer.flush()?;
        writer.sync_all()?;
    }
    replace_file(&temp, path).inspect_err(|_| {
        let _ = fs::remove_file(&temp);
    })
}

fn temp_path(target: &Path) -> PathBuf {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let name = target.file_name().and_then(|name| name.to_str()).unwrap_or("resource");
    let sequence = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(".{name}.{}.{}.tmp", std::process::id(), sequence))
}

fn remove_path_if_exists(path: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(not(windows))]
fn replace_file(temp: &Path, target: &Path) -> std::io::Result<()> {
    fs::rename(temp, target)
}

#[cfg(windows)]
fn replace_file(temp: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW};

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
}

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[cfg(unix)]
fn create_symlink(link_target: &Path, target: &Path, _source: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(link_target, target)
}

#[cfg(windows)]
fn create_symlink(link_target: &Path, target: &Path, source: &Path) -> std::io::Result<()> {
    let file_type = fs::metadata(source)?;
    if file_type.is_dir() {
        std::os::windows::fs::symlink_dir(link_target, target)
    } else {
        std::os::windows::fs::symlink_file(link_target, target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_mode_is_download() {
        if !crate::test_support::run_in_env_child("managed_resources::tests::default_mode_is_download", |command| {
            command.env_remove(BUNDLED_RESOURCES_ENV);
        }) {
            return;
        }
        assert_eq!(default_managed_resources_mode(), ManagedResourcesMode::Download);
    }

    #[test]
    fn bundled_mode_uses_configured_bundled_root() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("managed");
        if !crate::test_support::run_in_env_child(
            "managed_resources::tests::bundled_mode_uses_configured_bundled_root",
            |command| {
                command.env(BUNDLED_RESOURCES_ENV, &root);
            },
        ) {
            return;
        }
        let root = PathBuf::from(std::env::var_os(BUNDLED_RESOURCES_ENV).expect("bundled root env"));
        fs::create_dir_all(root.join("node").join("node-v24.11.0-darwin-arm64")).expect("create node dir");

        set_managed_resources_mode(ManagedResourcesMode::Bundled);

        let sources = node_sources("node-v24.11.0-darwin-arm64");
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].kind, ManagedResourceSourceKind::Bundled);
        assert_eq!(sources[0].root, root.join("node").join("node-v24.11.0-darwin-arm64"));

        set_managed_resources_mode(ManagedResourcesMode::Download);
    }

    #[test]
    fn cli_sources_targets_cli_subtree_in_bundled_mode() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("managed");
        if !crate::test_support::run_in_env_child(
            "managed_resources::tests::cli_sources_targets_cli_subtree_in_bundled_mode",
            |command| {
                command.env(BUNDLED_RESOURCES_ENV, &root);
            },
        ) {
            return;
        }
        let root = PathBuf::from(std::env::var_os(BUNDLED_RESOURCES_ENV).expect("bundled root env"));
        let expected = root.join("cli").join("claude").join("2.1.215").join("darwin-arm64");
        fs::create_dir_all(&expected).expect("create cli dir");

        set_managed_resources_mode(ManagedResourcesMode::Bundled);

        let sources = cli_sources("claude", "2.1.215", "darwin-arm64");
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].kind, ManagedResourceSourceKind::Bundled);
        assert_eq!(sources[0].root, expected);

        // Download mode yields no cli sources.
        set_managed_resources_mode(ManagedResourcesMode::Download);
        assert!(cli_sources("claude", "2.1.215", "darwin-arm64").is_empty());
    }

    #[test]
    fn download_mode_ignores_configured_bundled_root() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("managed");
        if !crate::test_support::run_in_env_child(
            "managed_resources::tests::download_mode_ignores_configured_bundled_root",
            |command| {
                command.env(BUNDLED_RESOURCES_ENV, &root);
            },
        ) {
            return;
        }
        let root = PathBuf::from(std::env::var_os(BUNDLED_RESOURCES_ENV).expect("bundled root env"));
        fs::create_dir_all(root.join("node").join("node-v24.11.0-darwin-arm64")).expect("create node dir");

        set_managed_resources_mode(ManagedResourcesMode::Download);

        let sources = node_sources("node-v24.11.0-darwin-arm64");
        assert!(sources.is_empty());
        assert!(!requires_bundled_resources());

        set_managed_resources_mode(ManagedResourcesMode::Download);
    }

    #[cfg(unix)]
    #[test]
    fn materialize_directory_preserves_symlink_entries() {
        let temp = tempfile::tempdir().expect("tempdir");
        let source = temp.path().join("source-node");
        fs::create_dir_all(source.join("bin")).expect("create source");
        fs::create_dir_all(source.join("lib").join("node_modules").join("npm").join("bin")).expect("create npm bin");
        fs::write(
            source
                .join("lib")
                .join("node_modules")
                .join("npm")
                .join("bin")
                .join("npm-cli.js"),
            b"#!/usr/bin/env node\n",
        )
        .expect("write npm cli");
        std::os::unix::fs::symlink(
            Path::new("../lib/node_modules/npm/bin/npm-cli.js"),
            source.join("bin").join("npm"),
        )
        .expect("create symlink");

        let target = temp.path().join("target-node");
        materialize_directory(&source, &target).expect("materialize");

        let copied_link = target.join("bin").join("npm");
        let metadata = fs::symlink_metadata(&copied_link).expect("metadata");
        assert!(metadata.file_type().is_symlink());
        assert_eq!(
            fs::read_link(&copied_link).expect("read link"),
            PathBuf::from("../lib/node_modules/npm/bin/npm-cli.js")
        );
    }

    #[test]
    fn content_addressed_materialization_resumes_and_changes_identity_with_content() {
        let temp = tempfile::tempdir().expect("tempdir");
        let source = temp.path().join("source");
        let objects = temp.path().join("objects");
        fs::create_dir_all(source.join("bin")).expect("create source");
        fs::write(source.join("bin/node"), b"node-v1").expect("write node");

        let first = materialize_content_addressed_directory(&source, &objects).expect("first materialize");
        assert_eq!(first.content_sha256.len(), 64);
        assert_eq!(fs::read(first.root.join("bin/node")).unwrap(), b"node-v1");

        fs::remove_file(first.root.join(".complete")).expect("remove completion marker");
        fs::remove_file(first.root.join("bin/node")).expect("remove materialized file");
        fs::write(first.root.join("stale"), b"stale").expect("write stale file");
        let resumed = materialize_content_addressed_directory(&source, &objects).expect("resume materialize");
        assert_eq!(resumed, first);
        assert_eq!(fs::read(resumed.root.join("bin/node")).unwrap(), b"node-v1");
        assert!(!resumed.root.join("stale").exists());

        fs::write(source.join("bin/node"), b"node-v2").expect("update node");
        let changed = materialize_content_addressed_directory(&source, &objects).expect("changed materialize");
        assert_ne!(changed.root, first.root);
        assert!(
            first.root.is_dir(),
            "old immutable object remains available for bounded GC"
        );
        assert_eq!(fs::read(changed.root.join("bin/node")).unwrap(), b"node-v2");
    }

    #[test]
    fn materialize_directory_prunes_stale_entries_without_replacing_the_tree() {
        let temp = tempfile::tempdir().expect("tempdir");
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        fs::create_dir_all(&source).expect("create source");
        fs::create_dir_all(&target).expect("create target");
        fs::write(source.join("keep"), b"same").expect("write source");
        fs::write(target.join("keep"), b"same").expect("write target");
        fs::write(target.join("stale"), b"remove").expect("write stale");

        materialize_directory(&source, &target).expect("materialize");

        assert_eq!(fs::read(target.join("keep")).unwrap(), b"same");
        assert!(!target.join("stale").exists());
    }
}

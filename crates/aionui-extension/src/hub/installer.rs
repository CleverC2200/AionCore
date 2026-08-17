use std::io::{Cursor, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use aionui_api_types::WebSocketMessage;
use aionui_realtime::EventBroadcaster;
use serde_json::json;
use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};

use crate::constants::EXTENSION_MANIFEST_FILE;
use crate::error::ExtensionError;
use crate::manifest::{parse_manifest, validate_manifest};
use crate::registry::ExtensionRegistry;
use crate::resolvers::resolve_extension_contributions;
use crate::types::{ExtensionSource, ExtensionState, LoadedExtension};

use super::index_manager::{HubIndexEntry, HubIndexManager};

const MAX_ARCHIVE_ENTRIES: usize = 10_000;
const MAX_EXTRACTED_BYTES: u64 = 256 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Result type
// ---------------------------------------------------------------------------

/// Outcome of a Hub install/update/uninstall operation.
#[derive(Debug, Clone)]
pub struct HubResult {
    pub success: bool,
    pub msg: Option<String>,
}

impl HubResult {
    fn ok() -> Self {
        Self {
            success: true,
            msg: None,
        }
    }

    fn err(msg: impl Into<String>) -> Self {
        Self {
            success: false,
            msg: Some(msg.into()),
        }
    }
}

/// Info about an available update.
#[derive(Debug, Clone)]
pub struct HubUpdateInfo {
    pub name: String,
    pub current_version: String,
    pub latest_version: String,
}

// ---------------------------------------------------------------------------
// HubInstaller
// ---------------------------------------------------------------------------

/// Handles extension installation, update, uninstall, and verification.
///
/// Remote packages are downloaded from the source configured by
/// [`HubIndexManager`], verified, safely extracted, and then atomically moved
/// into the extension directory before the registry is reloaded.
#[derive(Clone)]
pub struct HubInstaller {
    index_manager: HubIndexManager,
    registry: ExtensionRegistry,
    broadcaster: Arc<dyn EventBroadcaster>,
}

impl HubInstaller {
    pub fn new(index_manager: HubIndexManager, registry: ExtensionRegistry) -> Self {
        let broadcaster = registry.event_broadcaster();
        Self {
            index_manager,
            registry,
            broadcaster,
        }
    }

    /// Install an extension from the Hub by name.
    ///
    /// Flow: look up in index → stage package if needed → validate manifest →
    /// verify contributions → trigger hot reload.
    pub async fn install(&self, name: &str) -> HubResult {
        if let Err(error) = validate_hub_name(name) {
            self.broadcast_state_changed(name, "failed", Some(error.clone()));
            return HubResult::err(error);
        }

        info!(name, "hub: installing extension");
        self.broadcast_state_changed(name, "installing", None);

        let entry = match self.index_manager.get_extension(name).await {
            Some(e) => e,
            None => {
                let error = format!("Extension '{name}' not found in hub index");
                self.broadcast_state_changed(name, "failed", Some(error.clone()));
                return HubResult::err(error);
            }
        };

        let target_dir = self.index_manager.install_target_dir();
        let ext_dir = target_dir.join(&entry.name);

        if !ext_dir.exists()
            && let Err(error) = self.stage_package(&entry, false).await
        {
            let error = format!("Installation failed: {error}");
            self.broadcast_state_changed(name, "failed", Some(error.clone()));
            return HubResult::err(error);
        }

        if let Err(e) = self.verify_installation(&ext_dir) {
            let error = format!("Installation verification failed: {e}");
            self.broadcast_state_changed(name, "failed", Some(error.clone()));
            return HubResult::err(error);
        }

        // Trigger hot reload to pick up the new extension.
        self.registry.hot_reload().await;
        self.broadcast_state_changed(name, "installed", None);

        info!(name, "hub: extension installed successfully");
        HubResult::ok()
    }

    /// Retry a previously failed installation.
    pub async fn retry_install(&self, name: &str) -> HubResult {
        debug!(name, "hub: retrying installation");
        self.install(name).await
    }

    /// Update an installed extension to the latest version from the index.
    ///
    pub async fn update(&self, name: &str) -> HubResult {
        if let Err(error) = validate_hub_name(name) {
            self.broadcast_state_changed(name, "failed", Some(error.clone()));
            return HubResult::err(error);
        }

        info!(name, "hub: updating extension");
        self.broadcast_state_changed(name, "updating", None);

        let entry = match self.index_manager.get_extension(name).await {
            Some(e) => e,
            None => {
                let error = format!("Extension '{name}' not found in hub index");
                self.broadcast_state_changed(name, "failed", Some(error.clone()));
                return HubResult::err(error);
            }
        };

        let target_dir = self.index_manager.install_target_dir();
        let ext_dir = target_dir.join(&entry.name);

        if !ext_dir.exists() {
            let error = format!("Extension not installed: {}", ext_dir.display());
            self.broadcast_state_changed(name, "failed", Some(error.clone()));
            return HubResult::err(error);
        }

        if entry.dist.is_some()
            && let Err(error) = self.stage_package(&entry, true).await
        {
            let error = format!("Update failed: {error}");
            self.broadcast_state_changed(name, "failed", Some(error.clone()));
            return HubResult::err(error);
        } else if let Err(error) = self.verify_installation(&ext_dir) {
            let error = format!("Update verification failed: {error}");
            self.broadcast_state_changed(name, "failed", Some(error.clone()));
            return HubResult::err(error);
        }

        self.registry.hot_reload().await;
        self.broadcast_state_changed(name, "installed", None);

        info!(name, "hub: extension updated successfully");
        HubResult::ok()
    }

    /// Uninstall an extension by removing its directory and hot-reloading.
    pub async fn uninstall(&self, name: &str) -> HubResult {
        if let Err(msg) = validate_hub_name(name) {
            self.broadcast_state_changed(name, "failed", Some(msg.clone()));
            return HubResult::err(msg);
        }

        info!(name, "hub: uninstalling extension");

        let target_dir = self.index_manager.install_target_dir();
        let ext_dir = target_dir.join(name);

        if !ext_dir.exists() {
            let error = format!("Extension '{name}' is not installed");
            self.broadcast_state_changed(name, "failed", Some(error.clone()));
            return HubResult::err(error);
        }

        if let Err(e) = std::fs::remove_dir_all(&ext_dir) {
            warn!(
                name,
                error = %e,
                "hub: failed to remove extension directory"
            );
            let error = format!("Failed to remove extension directory: {e}");
            self.broadcast_state_changed(name, "failed", Some(error.clone()));
            return HubResult::err(error);
        }

        self.registry.hot_reload().await;
        self.broadcast_state_changed(name, "uninstalled", None);

        info!(name, "hub: extension uninstalled successfully");
        HubResult::ok()
    }

    /// Check for available updates across all installed extensions.
    ///
    /// Compares installed versions against the Hub index.
    pub async fn check_updates(&self) -> Vec<HubUpdateInfo> {
        let index_list = self.index_manager.load_index().await;
        let loaded = self.registry.get_loaded_extensions().await;

        let mut updates = Vec::new();

        for hub_ext in &index_list {
            if hub_ext.bundled {
                continue;
            }

            if let Some(installed) = loaded.iter().find(|l| l.name == hub_ext.name)
                && is_newer(&hub_ext.version, &installed.version)
            {
                updates.push(HubUpdateInfo {
                    name: hub_ext.name.clone(),
                    current_version: installed.version.clone(),
                    latest_version: hub_ext.version.clone(),
                });
            }
        }

        updates
    }

    /// Verify that an extension directory contains a valid manifest
    /// and that its contributions can be resolved without errors.
    pub fn verify_installation(&self, ext_dir: &Path) -> Result<(), ExtensionError> {
        let manifest_path = ext_dir.join(EXTENSION_MANIFEST_FILE);

        if !manifest_path.exists() {
            return Err(ExtensionError::ManifestValidation(format!(
                "Manifest not found: {}",
                manifest_path.display()
            )));
        }

        let bytes = std::fs::read(&manifest_path)?;
        let manifest = parse_manifest(&bytes)?;
        validate_manifest(&manifest)?;

        // Build a temporary LoadedExtension to test contribution resolution.
        let loaded = LoadedExtension {
            manifest,
            directory: ext_dir.to_str().unwrap_or_default().to_owned(),
            source: ExtensionSource::Local,
            state: ExtensionState {
                name: "verification-check".into(),
                version: "0.0.0".into(),
                enabled: true,
                installed_at: None,
                last_activated_at: None,
            },
        };

        // Resolve contributions — this validates CSS files exist for themes,
        // route namespaces for webui, etc.
        let _contributions = resolve_extension_contributions(&loaded);

        debug!(
            dir = %ext_dir.display(),
            "hub: installation verification passed"
        );
        Ok(())
    }

    async fn stage_package(&self, entry: &HubIndexEntry, replace_existing: bool) -> Result<(), ExtensionError> {
        validate_hub_name(&entry.name).map_err(ExtensionError::ManifestValidation)?;
        let package = self.index_manager.load_package(entry).await?;
        let target_dir = self.index_manager.install_target_dir();
        std::fs::create_dir_all(&target_dir)?;

        let staging = tempfile::Builder::new()
            .prefix(".hub-install-")
            .tempdir_in(&target_dir)?;
        let staged_extension = staging.path().join("extension");
        std::fs::create_dir(&staged_extension)?;
        extract_package(&package, &staged_extension)?;
        verify_package_integrity(entry, &staged_extension)?;
        self.verify_installation(&staged_extension)?;
        verify_package_identity(entry, &staged_extension)?;

        let extension_dir = target_dir.join(&entry.name);
        if replace_existing {
            let backup_dir = staging.path().join("previous");
            std::fs::rename(&extension_dir, &backup_dir)?;
            if let Err(error) = std::fs::rename(&staged_extension, &extension_dir) {
                let _ = std::fs::rename(&backup_dir, &extension_dir);
                return Err(error.into());
            }
        } else {
            std::fs::rename(&staged_extension, &extension_dir)?;
        }

        Ok(())
    }

    fn broadcast_state_changed(&self, name: &str, status: &str, error: Option<String>) {
        self.broadcaster.broadcast(WebSocketMessage::new(
            "hub.state-changed",
            json!({
                "name": name,
                "status": status,
                "error": error,
            }),
        ));
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Validate an extension name to prevent path traversal attacks.
fn validate_hub_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(format!("Invalid extension name: '{name}'"));
    }
    Ok(())
}

/// Check if `index_version` is newer than `installed_version`.
fn is_newer(index_version: &str, installed_version: &str) -> bool {
    let Ok(idx) = semver::Version::parse(index_version) else {
        return false;
    };
    let Ok(inst) = semver::Version::parse(installed_version) else {
        return false;
    };
    idx > inst
}

fn extract_package(package: &[u8], destination: &Path) -> Result<(), ExtensionError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(package))
        .map_err(|error| ExtensionError::ManifestValidation(format!("Invalid extension archive: {error}")))?;
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        return Err(ExtensionError::ManifestValidation(
            "Extension archive contains too many entries".into(),
        ));
    }

    let mut extracted_bytes = 0_u64;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| ExtensionError::ManifestValidation(format!("Invalid extension archive: {error}")))?;
        reject_archive_symlink(&entry)?;
        let relative_path = safe_archive_path(entry.name())?;
        let output_path = destination.join(relative_path);

        if entry.is_dir() {
            std::fs::create_dir_all(&output_path)?;
            continue;
        }

        extracted_bytes = extracted_bytes.saturating_add(entry.size());
        if extracted_bytes > MAX_EXTRACTED_BYTES {
            return Err(ExtensionError::ManifestValidation(
                "Extracted extension exceeds size limit".into(),
            ));
        }
        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut output = std::fs::File::create(&output_path)?;
        std::io::copy(&mut entry, &mut output)?;
        output.flush()?;

        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&output_path, std::fs::Permissions::from_mode(mode & 0o777))?;
        }
    }
    Ok(())
}

fn safe_archive_path(name: &str) -> Result<PathBuf, ExtensionError> {
    if name.is_empty() || name.contains('\\') {
        return Err(ExtensionError::PathTraversal(name.to_string()));
    }
    let path = Path::new(name);
    if path.is_absolute() {
        return Err(ExtensionError::PathTraversal(name.to_string()));
    }

    let mut safe = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => safe.push(part),
            Component::CurDir => {}
            _ => return Err(ExtensionError::PathTraversal(name.to_string())),
        }
    }
    if safe.as_os_str().is_empty() {
        return Err(ExtensionError::PathTraversal(name.to_string()));
    }
    Ok(safe)
}

fn reject_archive_symlink(entry: &zip::read::ZipFile<'_>) -> Result<(), ExtensionError> {
    if entry.unix_mode().is_some_and(|mode| mode & 0o170000 == 0o120000) {
        return Err(ExtensionError::PathTraversal(entry.name().to_string()));
    }
    Ok(())
}

fn verify_package_integrity(entry: &HubIndexEntry, extension_dir: &Path) -> Result<(), ExtensionError> {
    let integrity = entry
        .dist
        .as_ref()
        .map(|dist| dist.integrity.as_str())
        .ok_or_else(|| ExtensionError::ManifestValidation("Extension package integrity is missing".into()))?;
    let expected = integrity
        .strip_prefix("sha256-")
        .ok_or_else(|| ExtensionError::ManifestValidation("Unsupported extension integrity algorithm".into()))?;
    let actual = hash_extension_contents(extension_dir)?;
    if !expected.eq_ignore_ascii_case(&actual) {
        return Err(ExtensionError::ManifestValidation(format!(
            "Extension package integrity mismatch: expected {expected}, got {actual}"
        )));
    }
    Ok(())
}

fn hash_extension_contents(extension_dir: &Path) -> Result<String, ExtensionError> {
    let mut files = walkdir::WalkDir::new(extension_dir)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .collect::<Vec<_>>();
    files.sort_by_key(|path| normalized_relative_path(extension_dir, path));

    let mut hasher = Sha256::new();
    for path in files {
        let relative = normalized_relative_path(extension_dir, &path);
        hasher.update(relative.as_bytes());
        hasher.update(std::fs::read(path)?);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn normalized_relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn verify_package_identity(entry: &HubIndexEntry, extension_dir: &Path) -> Result<(), ExtensionError> {
    let bytes = std::fs::read(extension_dir.join(EXTENSION_MANIFEST_FILE))?;
    let manifest = parse_manifest(&bytes)?;
    if manifest.name != entry.name || manifest.version != entry.version {
        return Err(ExtensionError::ManifestValidation(format!(
            "Package manifest identity mismatch: expected {}@{}, got {}@{}",
            entry.name, entry.version, manifest.name, manifest.version
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::index_manager::HubSourceConfig;
    use aionui_realtime::BroadcastEventBus;

    #[test]
    fn hub_result_ok() {
        let r = HubResult::ok();
        assert!(r.success);
        assert!(r.msg.is_none());
    }

    #[test]
    fn hub_result_err() {
        let r = HubResult::err("something failed");
        assert!(!r.success);
        assert_eq!(r.msg.as_deref(), Some("something failed"));
    }

    #[test]
    fn is_newer_true() {
        assert!(is_newer("2.0.0", "1.0.0"));
        assert!(is_newer("1.1.0", "1.0.0"));
        assert!(is_newer("1.0.1", "1.0.0"));
    }

    #[test]
    fn is_newer_false() {
        assert!(!is_newer("1.0.0", "1.0.0"));
        assert!(!is_newer("1.0.0", "2.0.0"));
    }

    #[test]
    fn is_newer_invalid_versions() {
        assert!(!is_newer("not-semver", "1.0.0"));
        assert!(!is_newer("1.0.0", "not-semver"));
    }

    #[test]
    fn verify_installation_no_manifest() {
        let tmp = tempfile::TempDir::new().unwrap();
        let registry = make_test_registry();
        let index_mgr = HubIndexManager::new(tmp.path().to_path_buf(), registry.clone());
        let installer = HubInstaller::new(index_mgr, registry);

        let result = installer.verify_installation(tmp.path());
        assert!(result.is_err());
    }

    #[test]
    fn verify_installation_invalid_manifest() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join(EXTENSION_MANIFEST_FILE), b"not valid json").unwrap();

        let registry = make_test_registry();
        let index_mgr = HubIndexManager::new(tmp.path().to_path_buf(), registry.clone());
        let installer = HubInstaller::new(index_mgr, registry);

        let result = installer.verify_installation(tmp.path());
        assert!(result.is_err());
    }

    #[test]
    fn verify_installation_valid_manifest() {
        let tmp = tempfile::TempDir::new().unwrap();
        let manifest = serde_json::json!({
            "name": "test-ext",
            "version": "1.0.0"
        });
        std::fs::write(
            tmp.path().join(EXTENSION_MANIFEST_FILE),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let registry = make_test_registry();
        let index_mgr = HubIndexManager::new(tmp.path().to_path_buf(), registry.clone());
        let installer = HubInstaller::new(index_mgr, registry);

        let result = installer.verify_installation(tmp.path());
        assert!(result.is_ok());
    }

    #[test]
    fn verify_installation_reserved_name_fails() {
        let tmp = tempfile::TempDir::new().unwrap();
        let manifest = serde_json::json!({
            "name": "aion-internal-ext",
            "version": "1.0.0"
        });
        std::fs::write(
            tmp.path().join(EXTENSION_MANIFEST_FILE),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let registry = make_test_registry();
        let index_mgr = HubIndexManager::new(tmp.path().to_path_buf(), registry.clone());
        let installer = HubInstaller::new(index_mgr, registry);

        let result = installer.verify_installation(tmp.path());
        assert!(result.is_err());
    }

    #[test]
    fn validate_hub_name_rejects_traversal() {
        assert!(validate_hub_name("../etc").is_err());
        assert!(validate_hub_name("foo/../../bar").is_err());
        assert!(validate_hub_name("foo\\bar").is_err());
        assert!(validate_hub_name("").is_err());
        assert!(validate_hub_name("..").is_err());
    }

    #[test]
    fn validate_hub_name_accepts_valid() {
        assert!(validate_hub_name("my-extension").is_ok());
        assert!(validate_hub_name("ext_v2").is_ok());
        assert!(validate_hub_name("a").is_ok());
    }

    fn make_test_registry() -> ExtensionRegistry {
        use crate::state::ExtensionStateStore;

        let tmp = tempfile::TempDir::new().unwrap();
        let store = ExtensionStateStore::new(tmp.path().join("states.json"));
        let bus = Arc::new(BroadcastEventBus::new(64));
        // Leak the TempDir so it lives long enough for the test.
        std::mem::forget(tmp);
        ExtensionRegistry::new(store, bus, "1.0.0".into())
    }

    #[tokio::test]
    async fn install_broadcasts_installing_then_failed_for_missing_index_entry() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = crate::state::ExtensionStateStore::new(tmp.path().join("states.json"));
        let bus = Arc::new(BroadcastEventBus::new(64));
        let registry = ExtensionRegistry::new(store, bus.clone(), "1.0.0".into());
        let index_mgr = HubIndexManager::new(tmp.path().to_path_buf(), registry.clone());
        let installer = HubInstaller::new(index_mgr, registry);
        let mut rx = bus.subscribe();

        let result = installer.install("missing-ext").await;

        assert!(!result.success);
        let first = rx.recv().await.unwrap();
        assert_eq!(first.name, "hub.state-changed");
        assert_eq!(first.data["name"], "missing-ext");
        assert_eq!(first.data["status"], "installing");

        let second = rx.recv().await.unwrap();
        assert_eq!(second.name, "hub.state-changed");
        assert_eq!(second.data["status"], "failed");
    }

    #[tokio::test]
    async fn installs_current_aionhub_zip_from_bundled_source() {
        let target = tempfile::TempDir::new().unwrap();
        let bundle = tempfile::TempDir::new().unwrap();
        let manifest = br#"{"name":"test-hub-ext","version":"1.0.0"}"#;
        let mut hasher = Sha256::new();
        hasher.update(b"aion-extension.json");
        hasher.update(manifest);
        let integrity = format!("sha256-{}", hex::encode(hasher.finalize()));

        let archive_file = std::fs::File::create(bundle.path().join("test-hub-ext.zip")).unwrap();
        let mut archive = zip::ZipWriter::new(archive_file);
        archive
            .start_file("aion-extension.json", zip::write::SimpleFileOptions::default())
            .unwrap();
        archive.write_all(manifest).unwrap();
        archive.finish().unwrap();

        let index = serde_json::json!({
            "schemaVersion": 1,
            "extensions": {
                "test-hub-ext": {
                    "name": "test-hub-ext",
                    "displayName": "Test Hub Extension",
                    "version": "1.0.0",
                    "hubs": ["acpAdapters"],
                    "contributes": {"acpAdapters": ["test"]},
                    "dist": {
                        "tarball": "test-hub-ext.zip",
                        "integrity": integrity,
                        "unpackedSize": manifest.len()
                    }
                }
            }
        });
        std::fs::write(
            bundle.path().join("index.json"),
            serde_json::to_vec_pretty(&index).unwrap(),
        )
        .unwrap();

        let store = crate::state::ExtensionStateStore::new(target.path().join("states.json"));
        let bus = Arc::new(BroadcastEventBus::new(64));
        let registry = ExtensionRegistry::new(store, bus, "1.0.0".into());
        let index_mgr = HubIndexManager::with_source_config(
            target.path().to_path_buf(),
            registry.clone(),
            HubSourceConfig {
                base_url: None,
                bundled_dir: Some(bundle.path().to_path_buf()),
            },
        );
        let installer = HubInstaller::new(index_mgr, registry);

        let result = installer.install("test-hub-ext").await;

        assert!(result.success, "install should succeed: {:?}", result.msg);
        assert!(target.path().join("test-hub-ext/aion-extension.json").is_file());
    }

    #[test]
    fn archive_paths_reject_traversal_and_backslashes() {
        assert!(safe_archive_path("../escape").is_err());
        assert!(safe_archive_path("folder\\escape").is_err());
        assert!(safe_archive_path("/absolute").is_err());
        assert_eq!(
            safe_archive_path("resources/icon.svg").unwrap(),
            PathBuf::from("resources/icon.svg")
        );
    }
}

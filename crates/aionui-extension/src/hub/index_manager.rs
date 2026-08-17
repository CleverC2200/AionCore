use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::constants::HUB_SUPPORTED_SCHEMA_VERSION;
use crate::error::ExtensionError;
use crate::registry::ExtensionRegistry;
use crate::types::{HubExtensionStatus, HubExtensionWithStatus};

// ---------------------------------------------------------------------------
// Hub index on-disk format
// ---------------------------------------------------------------------------

/// Schema envelope for a Hub index file.
const DEFAULT_HUB_BASE_URL: &str = "https://raw.githubusercontent.com/iOfficeAI/AionHub/dist-latest/";
const HUB_BASE_URL_ENV: &str = "AIONUI_HUB_URL";
const HUB_BUNDLED_DIR_ENV: &str = "AIONUI_HUB_DIR";
const HUB_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_HUB_INDEX_BYTES: usize = 5 * 1024 * 1024;
const MAX_HUB_ARCHIVE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HubIndexFile {
    /// Schema version — we only support [`HUB_SUPPORTED_SCHEMA_VERSION`].
    #[serde(default = "default_schema_version", alias = "schemaVersion")]
    schema_version: u32,
    /// Extension entries in the index.
    #[serde(default)]
    extensions: HubIndexEntries,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum HubIndexEntries {
    List(Vec<HubIndexEntry>),
    Map(BTreeMap<String, HubIndexEntry>),
}

impl Default for HubIndexEntries {
    fn default() -> Self {
        Self::List(Vec::new())
    }
}

impl HubIndexEntries {
    fn into_vec(self) -> Vec<HubIndexEntry> {
        match self {
            Self::List(entries) => entries,
            Self::Map(entries) => entries
                .into_iter()
                .map(|(name, mut entry)| {
                    if entry.name.is_empty() {
                        entry.name = name;
                    }
                    entry
                })
                .collect(),
        }
    }
}

fn default_schema_version() -> u32 {
    1
}

/// A single entry in the Hub index file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct HubIndexEntry {
    #[serde(default)]
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "displayName")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hubs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contributes: Option<serde_json::Value>,
    /// Whether this extension is bundled with the app (no download needed).
    #[serde(default)]
    pub bundled: bool,
    /// Optional download URL for remote extensions.
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "downloadUrl")]
    pub download_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dist: Option<HubDistribution>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct HubDistribution {
    pub tarball: String,
    pub integrity: String,
    #[serde(default, alias = "unpackedSize")]
    pub unpacked_size: Option<u64>,
}

/// Optional upstream sources used by the production Hub manager.
#[derive(Debug, Clone, Default)]
pub struct HubSourceConfig {
    pub base_url: Option<String>,
    pub bundled_dir: Option<PathBuf>,
}

impl HubSourceConfig {
    pub fn production_defaults() -> Self {
        let base_url = std::env::var(HUB_BASE_URL_ENV)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| Some(DEFAULT_HUB_BASE_URL.to_string()));
        let bundled_dir = std::env::var_os(HUB_BUNDLED_DIR_ENV)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        Self { base_url, bundled_dir }
    }
}

// ---------------------------------------------------------------------------
// HubIndexManager
// ---------------------------------------------------------------------------

/// Manages the Hub extension index — loads from local file, merges
/// install status from the live extension registry.
#[derive(Clone)]
pub struct HubIndexManager {
    /// Directory that contains `index.json`.
    index_dir: PathBuf,
    /// Reference to the live extension registry for status resolution.
    registry: ExtensionRegistry,
    source: HubSourceConfig,
    http_client: reqwest::Client,
}

impl HubIndexManager {
    /// Create a new index manager.
    ///
    /// - `index_dir`: directory containing the Hub `index.json`.
    /// - `registry`: live extension registry used to determine install status.
    pub fn new(index_dir: PathBuf, registry: ExtensionRegistry) -> Self {
        Self::with_source_config(index_dir, registry, HubSourceConfig::default())
    }

    /// Create a Hub manager that can refresh the index and packages from the
    /// configured AionHub source, while retaining the local index as fallback.
    pub fn with_source_config(index_dir: PathBuf, registry: ExtensionRegistry, source: HubSourceConfig) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(HUB_REQUEST_TIMEOUT)
            .build()
            .expect("hub HTTP client configuration must be valid");
        Self {
            index_dir,
            registry,
            source,
            http_client,
        }
    }

    pub fn with_default_sources(index_dir: PathBuf, registry: ExtensionRegistry) -> Self {
        Self::with_source_config(index_dir, registry, HubSourceConfig::production_defaults())
    }

    /// Load the Hub index and merge install status from the registry.
    ///
    /// Returns a list of extensions with their current status.
    pub async fn load_index(&self) -> Vec<HubExtensionWithStatus> {
        let entries = self.load_index_entries().await;
        self.merge_with_registry_status(entries).await
    }

    /// Look up a single extension by name from the index.
    pub(crate) async fn get_extension(&self, name: &str) -> Option<HubIndexEntry> {
        let entries = self.load_index_entries().await;
        entries.into_iter().find(|e| e.name == name)
    }

    /// Return the download URL for a given extension (if available).
    pub async fn get_download_url(&self, name: &str) -> Option<String> {
        let entry = self.get_extension(name).await?;
        self.remote_package_url(&entry)
    }

    /// Return the directory where extensions should be installed.
    pub fn install_target_dir(&self) -> PathBuf {
        self.index_dir.clone()
    }

    /// Return the index file path.
    fn index_file_path(&self) -> PathBuf {
        self.index_dir.join("index.json")
    }

    /// Load index entries from the remote source, local cache, or bundled fallback.
    async fn load_index_entries(&self) -> Vec<HubIndexEntry> {
        let path = self.index_file_path();
        let mut source_errors = Vec::new();

        if let Some(base_url) = &self.source.base_url {
            match self.fetch_remote_index(base_url).await {
                Ok(bytes) => match parse_index(&bytes) {
                    Ok(entries) => {
                        self.cache_index(&bytes);
                        return entries;
                    }
                    Err(error) => source_errors.push(format!("remote index invalid: {error}")),
                },
                Err(error) => source_errors.push(format!("remote index unavailable: {error}")),
            }
        }

        match load_index_from_file(&path) {
            Ok(entries) => return entries,
            Err(error) => source_errors.push(format!("cached index unavailable: {error}")),
        }

        if let Some(bundled_dir) = &self.source.bundled_dir {
            let bundled_path = bundled_dir.join("index.json");
            match load_index_from_file(&bundled_path) {
                Ok(entries) => return entries,
                Err(error) => source_errors.push(format!("bundled index unavailable: {error}")),
            }
        }

        if self.source.base_url.is_none() && self.source.bundled_dir.is_none() {
            debug!(path = %path.display(), source_errors = ?source_errors, "hub index not found or invalid");
        } else {
            warn!(
                path = %path.display(),
                source_errors = ?source_errors,
                "hub index unavailable from configured sources and local cache"
            );
        }
        Vec::new()
    }

    async fn fetch_remote_index(&self, base_url: &str) -> Result<Vec<u8>, ExtensionError> {
        let url = join_url(base_url, "index.json")?;
        let response = self
            .http_client
            .get(url)
            .send()
            .await
            .map_err(|error| ExtensionError::Remote(error.to_string()))?
            .error_for_status()
            .map_err(|error| ExtensionError::Remote(error.to_string()))?;
        if response
            .content_length()
            .is_some_and(|size| size > MAX_HUB_INDEX_BYTES as u64)
        {
            return Err(ExtensionError::Remote("hub index exceeds size limit".into()));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|error| ExtensionError::Remote(error.to_string()))?;
        if bytes.len() > MAX_HUB_INDEX_BYTES {
            return Err(ExtensionError::Remote("hub index exceeds size limit".into()));
        }
        Ok(bytes.to_vec())
    }

    fn cache_index(&self, bytes: &[u8]) {
        let path = self.index_file_path();
        if let Err(error) = std::fs::create_dir_all(&self.index_dir).and_then(|()| std::fs::write(&path, bytes)) {
            warn!(path = %path.display(), error = %error, "failed to cache hub index");
        }
    }

    pub(crate) async fn load_package(&self, entry: &HubIndexEntry) -> Result<Vec<u8>, ExtensionError> {
        let tarball = entry
            .dist
            .as_ref()
            .map(|dist| dist.tarball.as_str())
            .ok_or_else(|| ExtensionError::Remote(format!("Extension '{}' has no package metadata", entry.name)))?;
        validate_relative_package_path(tarball)?;

        if let Some(bundled_dir) = &self.source.bundled_dir {
            let package_path = bundled_dir.join(tarball);
            if package_path.is_file() {
                let bytes = std::fs::read(&package_path)?;
                if bytes.len() > MAX_HUB_ARCHIVE_BYTES {
                    return Err(ExtensionError::Remote("hub package exceeds size limit".into()));
                }
                return Ok(bytes);
            }
        }

        let url = self
            .remote_package_url(entry)
            .ok_or_else(|| ExtensionError::Remote(format!("Extension '{}' has no package source", entry.name)))?;
        let response = self
            .http_client
            .get(url)
            .send()
            .await
            .map_err(|error| ExtensionError::Remote(error.to_string()))?
            .error_for_status()
            .map_err(|error| ExtensionError::Remote(error.to_string()))?;
        if response
            .content_length()
            .is_some_and(|size| size > MAX_HUB_ARCHIVE_BYTES as u64)
        {
            return Err(ExtensionError::Remote("hub package exceeds size limit".into()));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|error| ExtensionError::Remote(error.to_string()))?;
        if bytes.len() > MAX_HUB_ARCHIVE_BYTES {
            return Err(ExtensionError::Remote("hub package exceeds size limit".into()));
        }
        Ok(bytes.to_vec())
    }

    fn remote_package_url(&self, entry: &HubIndexEntry) -> Option<String> {
        if let Some(url) = &entry.download_url {
            return Some(url.clone());
        }
        let tarball = entry.dist.as_ref()?.tarball.as_str();
        let base_url = self.source.base_url.as_deref()?;
        join_url(base_url, tarball).ok()
    }

    /// Merge index entries with live registry status.
    async fn merge_with_registry_status(&self, entries: Vec<HubIndexEntry>) -> Vec<HubExtensionWithStatus> {
        let loaded = self.registry.get_loaded_extensions().await;
        let installed: HashMap<String, String> = loaded.into_iter().map(|s| (s.name, s.version)).collect();

        entries
            .into_iter()
            .map(|entry| {
                let status = resolve_status(&entry, &installed);
                HubExtensionWithStatus {
                    name: entry.name,
                    version: entry.version,
                    display_name: entry.display_name,
                    description: entry.description,
                    author: entry.author,
                    icon: entry.icon,
                    tags: entry.tags,
                    hubs: entry.hubs,
                    contributes: entry.contributes,
                    bundled: entry.bundled,
                    status,
                }
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Index file I/O
// ---------------------------------------------------------------------------

/// Read and parse the Hub index file, returning entries.
fn load_index_from_file(path: &Path) -> Result<Vec<HubIndexEntry>, ExtensionError> {
    let bytes = std::fs::read(path)?;
    parse_index(&bytes)
}

fn parse_index(bytes: &[u8]) -> Result<Vec<HubIndexEntry>, ExtensionError> {
    let index: HubIndexFile = serde_json::from_slice(bytes)?;

    if index.schema_version != HUB_SUPPORTED_SCHEMA_VERSION {
        warn!(
            found = index.schema_version,
            expected = HUB_SUPPORTED_SCHEMA_VERSION,
            "hub index schema version mismatch — attempting best-effort parse"
        );
    }

    Ok(index.extensions.into_vec())
}

fn join_url(base_url: &str, relative: &str) -> Result<String, ExtensionError> {
    let mut normalized = base_url.to_string();
    if !normalized.ends_with('/') {
        normalized.push('/');
    }
    let base = reqwest::Url::parse(&normalized).map_err(|error| ExtensionError::Remote(error.to_string()))?;
    base.join(relative)
        .map(|url| url.to_string())
        .map_err(|error| ExtensionError::Remote(error.to_string()))
}

fn validate_relative_package_path(value: &str) -> Result<(), ExtensionError> {
    let path = Path::new(value);
    if value.is_empty()
        || value.contains('\\')
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(ExtensionError::PathTraversal(value.to_string()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Status resolution
// ---------------------------------------------------------------------------

/// Determine the runtime status of a Hub entry by checking whether
/// it is loaded in the registry.
fn resolve_status(entry: &HubIndexEntry, installed: &HashMap<String, String>) -> HubExtensionStatus {
    if entry.bundled {
        return HubExtensionStatus::Installed;
    }

    match installed.get(&entry.name) {
        Some(installed_version) => {
            if is_update_available(&entry.version, installed_version) {
                HubExtensionStatus::UpdateAvailable
            } else {
                HubExtensionStatus::Installed
            }
        }
        None => HubExtensionStatus::NotInstalled,
    }
}

/// Check if the index version is newer than the installed version.
fn is_update_available(index_version: &str, installed_version: &str) -> bool {
    let Ok(idx) = semver::Version::parse(index_version) else {
        return false;
    };
    let Ok(inst) = semver::Version::parse(installed_version) else {
        return false;
    };
    idx > inst
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_status_bundled_always_installed() {
        let entry = HubIndexEntry {
            name: "builtin-ext".into(),
            version: "1.0.0".into(),
            display_name: None,
            description: None,
            author: None,
            icon: None,
            tags: Vec::new(),
            hubs: Vec::new(),
            contributes: None,
            bundled: true,
            download_url: None,
            dist: None,
        };
        let installed = HashMap::new();
        assert_eq!(resolve_status(&entry, &installed), HubExtensionStatus::Installed);
    }

    #[test]
    fn resolve_status_not_installed() {
        let entry = HubIndexEntry {
            name: "new-ext".into(),
            version: "1.0.0".into(),
            display_name: None,
            description: None,
            author: None,
            icon: None,
            tags: Vec::new(),
            hubs: Vec::new(),
            contributes: None,
            bundled: false,
            download_url: None,
            dist: None,
        };
        let installed = HashMap::new();
        assert_eq!(resolve_status(&entry, &installed), HubExtensionStatus::NotInstalled);
    }

    #[test]
    fn resolve_status_installed_same_version() {
        let entry = HubIndexEntry {
            name: "my-ext".into(),
            version: "1.0.0".into(),
            display_name: None,
            description: None,
            author: None,
            icon: None,
            tags: Vec::new(),
            hubs: Vec::new(),
            contributes: None,
            bundled: false,
            download_url: None,
            dist: None,
        };
        let installed = HashMap::from([("my-ext".into(), "1.0.0".into())]);
        assert_eq!(resolve_status(&entry, &installed), HubExtensionStatus::Installed);
    }

    #[test]
    fn resolve_status_update_available() {
        let entry = HubIndexEntry {
            name: "my-ext".into(),
            version: "2.0.0".into(),
            display_name: None,
            description: None,
            author: None,
            icon: None,
            tags: Vec::new(),
            hubs: Vec::new(),
            contributes: None,
            bundled: false,
            download_url: None,
            dist: None,
        };
        let installed = HashMap::from([("my-ext".into(), "1.0.0".into())]);
        assert_eq!(resolve_status(&entry, &installed), HubExtensionStatus::UpdateAvailable);
    }

    #[test]
    fn resolve_status_installed_newer_than_index() {
        let entry = HubIndexEntry {
            name: "my-ext".into(),
            version: "1.0.0".into(),
            display_name: None,
            description: None,
            author: None,
            icon: None,
            tags: Vec::new(),
            hubs: Vec::new(),
            contributes: None,
            bundled: false,
            download_url: None,
            dist: None,
        };
        let installed = HashMap::from([("my-ext".into(), "2.0.0".into())]);
        // Installed version is newer — still "installed", not "update_available".
        assert_eq!(resolve_status(&entry, &installed), HubExtensionStatus::Installed);
    }

    #[test]
    fn is_update_available_newer() {
        assert!(is_update_available("2.0.0", "1.0.0"));
    }

    #[test]
    fn is_update_available_same() {
        assert!(!is_update_available("1.0.0", "1.0.0"));
    }

    #[test]
    fn is_update_available_older() {
        assert!(!is_update_available("1.0.0", "2.0.0"));
    }

    #[test]
    fn is_update_available_invalid_version() {
        assert!(!is_update_available("not-semver", "1.0.0"));
        assert!(!is_update_available("1.0.0", "not-semver"));
    }

    #[test]
    fn load_index_from_file_valid() {
        let tmp = tempfile::TempDir::new().unwrap();
        let index = HubIndexFile {
            schema_version: 1,
            extensions: HubIndexEntries::List(vec![HubIndexEntry {
                name: "test-ext".into(),
                version: "1.0.0".into(),
                display_name: Some("Test Extension".into()),
                description: Some("A test extension".into()),
                author: Some("Test Author".into()),
                icon: None,
                tags: vec!["tools".into()],
                hubs: vec!["acpAdapters".into()],
                contributes: Some(serde_json::json!({"acpAdapters": ["test"]})),
                bundled: false,
                download_url: Some("https://example.com/test-ext-1.0.0.tar.gz".into()),
                dist: None,
            }]),
        };
        let path = tmp.path().join("index.json");
        std::fs::write(&path, serde_json::to_vec_pretty(&index).unwrap()).unwrap();

        let entries = load_index_from_file(&path).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "test-ext");
        assert_eq!(entries[0].version, "1.0.0");
        assert!(!entries[0].bundled);
    }

    #[test]
    fn parse_current_aionhub_index_shape() {
        let bytes = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 1,
            "extensions": {
                "aionext-codex": {
                    "name": "aionext-codex",
                    "displayName": "Codex CLI",
                    "version": "1.0.0",
                    "description": "Codex ACP adapter",
                    "author": "Aionui Official",
                    "hubs": ["acpAdapters"],
                    "contributes": {"acpAdapters": ["codex"]},
                    "dist": {
                        "tarball": "aionext-codex.zip",
                        "integrity": "sha256-deadbeef",
                        "unpackedSize": 2190
                    }
                }
            }
        }))
        .unwrap();

        let entries = parse_index(&bytes).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "aionext-codex");
        assert_eq!(entries[0].display_name.as_deref(), Some("Codex CLI"));
        assert_eq!(entries[0].hubs, vec!["acpAdapters"]);
        assert_eq!(entries[0].contributes.as_ref().unwrap()["acpAdapters"][0], "codex");
        assert_eq!(entries[0].dist.as_ref().unwrap().tarball, "aionext-codex.zip");
    }

    #[test]
    fn load_index_from_file_not_found() {
        let result = load_index_from_file(Path::new("/nonexistent/index.json"));
        assert!(result.is_err());
    }

    #[test]
    fn load_index_from_file_invalid_json() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("index.json");
        std::fs::write(&path, b"not valid json").unwrap();

        let result = load_index_from_file(&path);
        assert!(result.is_err());
    }

    #[test]
    fn load_index_from_file_empty_extensions() {
        let tmp = tempfile::TempDir::new().unwrap();
        let index = HubIndexFile {
            schema_version: 1,
            extensions: HubIndexEntries::List(Vec::new()),
        };
        let path = tmp.path().join("index.json");
        std::fs::write(&path, serde_json::to_vec(&index).unwrap()).unwrap();

        let entries = load_index_from_file(&path).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn hub_index_entry_deserialization() {
        let json = serde_json::json!({
            "name": "my-ext",
            "version": "2.0.0",
            "display_name": "My Extension",
            "tags": ["ai", "tools"],
            "bundled": true
        });
        let entry: HubIndexEntry = serde_json::from_value(json).unwrap();
        assert_eq!(entry.name, "my-ext");
        assert_eq!(entry.version, "2.0.0");
        assert_eq!(entry.display_name.as_deref(), Some("My Extension"));
        assert_eq!(entry.tags, vec!["ai", "tools"]);
        assert!(entry.bundled);
        assert!(entry.download_url.is_none());
    }
}

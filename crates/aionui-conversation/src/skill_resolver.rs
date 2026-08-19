//! Abstraction over "what are the auto-inject skill names right now?" so
//! `ConversationService` can compute the initial snapshot without forcing
//! every test setup to stand up a real `SkillPaths` and skill repository.

use std::path::Path;
use std::sync::Arc;

use aionui_db::{IGeaResourceRepository, ISkillRepository};
pub use aionui_extension::ResolvedAgentSkill;
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use tracing::warn;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedAgentSkill {
    pub name: String,
    pub body: String,
    pub managed: Option<ManagedSkillSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ManagedSkillSnapshot {
    pub skill_code: String,
    pub version: String,
    pub digest: String,
    pub risk_level: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedSkillExecutionReport {
    pub skill: ManagedSkillSnapshot,
    pub success: bool,
    pub executed_at: String,
    pub duration_ms: u64,
    pub result_size: u64,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
}

#[async_trait]
pub trait ManagedSkillExecutionReporter: Send + Sync {
    async fn report_execution(&self, user_id: &str, report: ManagedSkillExecutionReport) -> Result<(), String>;
}

pub(crate) fn managed_skill_snapshots_from_extra(extra: &str) -> Vec<ManagedSkillSnapshot> {
    serde_json::from_str::<serde_json::Value>(extra)
        .ok()
        .and_then(|value| value.get("managed_skill_snapshots").cloned())
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default()
}

#[async_trait]
pub trait SkillResolver: Send + Sync {
    /// Returns the sorted list of auto-inject builtin skill names currently
    /// available on this installation.
    async fn auto_inject_names(&self) -> Vec<String>;

    /// Resolve each skill name to its on-disk source directory, using the
    /// same search order as `materialize_skills_for_agent`.
    async fn resolve_skills(&self, names: &[String]) -> Vec<ResolvedAgentSkill>;

    /// Resolve each skill name for a specific Core user.
    async fn resolve_skills_for_user(&self, _user_id: &str, names: &[String]) -> Vec<ResolvedAgentSkill> {
        self.resolve_skills(names).await
    }

    async fn snapshot_managed_skills_for_user(&self, _user_id: &str, _names: &[String]) -> Vec<ManagedSkillSnapshot> {
        Vec::new()
    }

    async fn resolve_skills_for_user_at_snapshot(
        &self,
        user_id: &str,
        names: &[String],
        _managed: &[ManagedSkillSnapshot],
    ) -> Vec<ResolvedAgentSkill> {
        self.resolve_skills_for_user(user_id, names).await
    }

    /// Load full skill bodies for prompt-protocol agents that request
    /// `[LOAD_SKILL: name]` in their response.
    async fn load_skill_bodies(&self, names: &[String]) -> Vec<LoadedAgentSkill> {
        let resolved = self.resolve_skills(names).await;
        load_resolved_skill_bodies(&resolved).await
    }

    /// Load full skill bodies for prompt-protocol agents under one Core user.
    async fn load_skill_bodies_for_user(&self, user_id: &str, names: &[String]) -> Vec<LoadedAgentSkill> {
        let resolved = self.resolve_skills_for_user(user_id, names).await;
        load_resolved_skill_bodies(&resolved).await
    }

    async fn load_skill_bodies_for_user_at_snapshot(
        &self,
        user_id: &str,
        names: &[String],
        managed: &[ManagedSkillSnapshot],
    ) -> Vec<LoadedAgentSkill> {
        if managed.is_empty() {
            return self.load_skill_bodies_for_user(user_id, names).await;
        }
        let resolved = self.resolve_skills_for_user_at_snapshot(user_id, names, managed).await;
        load_resolved_skill_bodies(&resolved).await
    }

    /// Create symlinks pointing at each resolved skill inside the given
    /// workspace's per-backend native skills directories. `rel_dirs` is
    /// the list of relative paths (e.g. `.claude/skills`) to populate.
    /// Returns the number of symlinks successfully created.
    async fn link_workspace_skills(&self, workspace: &Path, rel_dirs: &[&str], skills: &[ResolvedAgentSkill]) -> usize;
}

/// Production adapter backed by `aionui_extension::skill_service`.
pub struct ExtensionSkillResolver {
    paths: Arc<aionui_extension::SkillPaths>,
    skill_repo: Arc<dyn ISkillRepository>,
    managed_repo: Option<Arc<dyn IGeaResourceRepository>>,
}

impl ExtensionSkillResolver {
    pub fn new(paths: Arc<aionui_extension::SkillPaths>, skill_repo: Arc<dyn ISkillRepository>) -> Self {
        Self {
            paths,
            skill_repo,
            managed_repo: None,
        }
    }

    pub fn with_managed_repo(mut self, managed_repo: Arc<dyn IGeaResourceRepository>) -> Self {
        self.managed_repo = Some(managed_repo);
        self
    }
}

async fn load_resolved_skill_bodies(skills: &[ResolvedAgentSkill]) -> Vec<LoadedAgentSkill> {
    let mut loaded = Vec::new();
    for skill in skills {
        let skill_file = skill.source_path.join("SKILL.md");
        match tokio::fs::read_to_string(&skill_file).await {
            Ok(content) => loaded.push(LoadedAgentSkill {
                name: skill.name.clone(),
                body: extract_skill_body(&content),
                managed: None,
            }),
            Err(e) => {
                warn!(
                    skill = %skill.name,
                    path = %skill_file.display(),
                    error = %e,
                    "Failed to read requested skill body"
                );
            }
        }
    }
    loaded
}

fn extract_skill_body(content: &str) -> String {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return content.to_string();
    }

    let after_open = &trimmed[3..];
    if let Some(close_idx) = after_open.find("---") {
        let after_close = &after_open[close_idx + 3..];
        after_close.trim_start_matches('\n').to_string()
    } else {
        content.to_string()
    }
}

#[async_trait]
impl SkillResolver for ExtensionSkillResolver {
    async fn auto_inject_names(&self) -> Vec<String> {
        match aionui_extension::list_available_skills_with_repo(&self.paths, self.skill_repo.as_ref()).await {
            Ok(items) => {
                let mut names: Vec<String> = items
                    .into_iter()
                    .filter(|item| {
                        item.source == aionui_extension::SkillSource::Builtin
                            && item
                                .relative_location
                                .as_deref()
                                .is_some_and(|location| location.starts_with("auto-inject/"))
                    })
                    .map(|item| item.name)
                    .collect();
                names.sort();
                names
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "auto_inject_names: skill catalog lookup failed, falling back to empty"
                );
                Vec::new()
            }
        }
    }

    async fn resolve_skills(&self, names: &[String]) -> Vec<ResolvedAgentSkill> {
        self.resolve_skills_for_user("system_default_user", names).await
    }

    async fn resolve_skills_for_user(&self, user_id: &str, names: &[String]) -> Vec<ResolvedAgentSkill> {
        if names.is_empty() {
            return Vec::new();
        }
        // Conversation_id is validated upstream; we don't use a real one here
        // because this resolver is purely a path-resolution helper.
        let mut resolved = match aionui_extension::materialize_skills_for_agent_with_repo_for_user(
            &self.paths,
            self.skill_repo.as_ref(),
            user_id,
            "workspace-link",
            names,
        )
        .await
        {
            Ok(list) => list,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "resolve_skills failed; returning empty list"
                );
                Vec::new()
            }
        };
        if let Some(repo) = &self.managed_repo {
            for name in names {
                if resolved.iter().any(|skill| skill.name == *name) {
                    continue;
                }
                match repo.find_managed_skill_for_user(user_id, name).await {
                    Ok(Some(row)) if row.state == "active" && Path::new(&row.path).join("SKILL.md").is_file() => {
                        resolved.push(ResolvedAgentSkill {
                            name: row.skill_code,
                            source_path: row.path.into(),
                        });
                    }
                    Ok(_) => {}
                    Err(error) => tracing::warn!(
                        skill = %name,
                        error = %error,
                        "managed Skill lookup failed"
                    ),
                }
            }
        }
        resolved.sort_by(|a, b| a.name.cmp(&b.name));
        resolved
    }

    async fn snapshot_managed_skills_for_user(&self, user_id: &str, names: &[String]) -> Vec<ManagedSkillSnapshot> {
        let Some(repo) = &self.managed_repo else {
            return Vec::new();
        };
        let locally_resolved = aionui_extension::materialize_skills_for_agent_with_repo_for_user(
            &self.paths,
            self.skill_repo.as_ref(),
            user_id,
            "workspace-link",
            names,
        )
        .await
        .unwrap_or_default();
        let mut snapshots = Vec::new();
        for name in names {
            if locally_resolved.iter().any(|skill| skill.name == *name) {
                continue;
            }
            if let Ok(Some(row)) = repo.find_managed_skill_for_user(user_id, name).await
                && row.state == "active"
            {
                snapshots.push(ManagedSkillSnapshot {
                    skill_code: row.skill_code,
                    version: row.version,
                    digest: row.digest,
                    risk_level: row.risk_level,
                });
            }
        }
        snapshots.sort_by(|a, b| a.skill_code.cmp(&b.skill_code));
        snapshots
    }

    async fn resolve_skills_for_user_at_snapshot(
        &self,
        user_id: &str,
        names: &[String],
        managed: &[ManagedSkillSnapshot],
    ) -> Vec<ResolvedAgentSkill> {
        let mut resolved = match aionui_extension::materialize_skills_for_agent_with_repo_for_user(
            &self.paths,
            self.skill_repo.as_ref(),
            user_id,
            "workspace-link",
            names,
        )
        .await
        {
            Ok(list) => list,
            Err(error) => {
                tracing::warn!(error = %error, "snapshot skill resolution failed");
                Vec::new()
            }
        };
        // A managed snapshot freezes both identity and source for the lifetime of the
        // conversation. A local Skill created later must not silently replace it.
        resolved.retain(|skill| {
            !managed
                .iter()
                .any(|expected| expected.skill_code == skill.name && names.iter().any(|name| name == &skill.name))
        });
        if let Some(repo) = &self.managed_repo {
            for expected in managed {
                if !names.iter().any(|name| name == &expected.skill_code)
                    || resolved.iter().any(|skill| skill.name == expected.skill_code)
                {
                    continue;
                }
                match repo.find_managed_skill_for_user(user_id, &expected.skill_code).await {
                    Ok(Some(row))
                        if row.state == "active"
                            && row.version == expected.version
                            && row.digest == expected.digest
                            && Path::new(&row.path).join("SKILL.md").is_file() =>
                    {
                        resolved.push(ResolvedAgentSkill {
                            name: row.skill_code,
                            source_path: row.path.into(),
                        });
                    }
                    Ok(_) => tracing::warn!(
                        skill = %expected.skill_code,
                        version = %expected.version,
                        "managed Skill snapshot no longer matches the active catalog"
                    ),
                    Err(error) => tracing::warn!(
                        skill = %expected.skill_code,
                        error = %error,
                        "managed Skill snapshot lookup failed"
                    ),
                }
            }
        }
        resolved.sort_by(|a, b| a.name.cmp(&b.name));
        resolved
    }

    async fn load_skill_bodies_for_user_at_snapshot(
        &self,
        user_id: &str,
        names: &[String],
        managed: &[ManagedSkillSnapshot],
    ) -> Vec<LoadedAgentSkill> {
        let resolved = self.resolve_skills_for_user_at_snapshot(user_id, names, managed).await;
        let mut loaded = Vec::new();
        for skill in resolved {
            let skill_file = skill.source_path.join("SKILL.md");
            let managed_snapshot = if let Some(expected) = managed.iter().find(|item| item.skill_code == skill.name) {
                match &self.managed_repo {
                    Some(repo) => repo
                        .find_managed_skill_for_user(user_id, &skill.name)
                        .await
                        .ok()
                        .flatten()
                        .filter(|row| {
                            row.version == expected.version
                                && row.digest == expected.digest
                                && Path::new(&row.path) == skill.source_path
                        })
                        .map(|_| expected.clone()),
                    None => None,
                }
            } else {
                None
            };
            match tokio::fs::read(&skill_file).await {
                Ok(bytes) => {
                    if let Some(expected) = managed_snapshot.as_ref()
                        && !normalize_sha256(&format!("{:x}", Sha256::digest(&bytes)))
                            .eq_ignore_ascii_case(normalize_sha256(&expected.digest))
                    {
                        tracing::warn!(
                            skill = %skill.name,
                            path = %skill_file.display(),
                            "managed Skill content digest no longer matches the frozen snapshot"
                        );
                        continue;
                    }
                    let Ok(content) = std::str::from_utf8(&bytes) else {
                        tracing::warn!(skill = %skill.name, path = %skill_file.display(), "Skill body is not valid UTF-8");
                        continue;
                    };
                    loaded.push(LoadedAgentSkill {
                        name: skill.name,
                        body: extract_skill_body(content),
                        managed: managed_snapshot,
                    });
                }
                Err(error) => tracing::warn!(
                    skill = %skill.name,
                    path = %skill_file.display(),
                    error = %error,
                    "Failed to read requested skill body"
                ),
            }
        }
        loaded
    }

    async fn link_workspace_skills(&self, workspace: &Path, rel_dirs: &[&str], skills: &[ResolvedAgentSkill]) -> usize {
        if rel_dirs.is_empty() || skills.is_empty() {
            return 0;
        }
        match aionui_extension::link_workspace_skills(workspace, rel_dirs, skills).await {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(
                    workspace = %workspace.display(),
                    error = %e,
                    "link_workspace_skills failed"
                );
                0
            }
        }
    }
}

fn normalize_sha256(value: &str) -> &str {
    value.trim().strip_prefix("sha256:").unwrap_or(value.trim())
}

#[cfg(test)]
pub struct FixedSkillResolver {
    pub names: Vec<String>,
}

#[cfg(test)]
#[async_trait]
impl SkillResolver for FixedSkillResolver {
    async fn auto_inject_names(&self) -> Vec<String> {
        self.names.clone()
    }

    async fn resolve_skills(&self, _names: &[String]) -> Vec<ResolvedAgentSkill> {
        Vec::new()
    }

    async fn link_workspace_skills(
        &self,
        _workspace: &Path,
        _rel_dirs: &[&str],
        _skills: &[ResolvedAgentSkill],
    ) -> usize {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_db::{
        IGeaResourceRepository, ReplaceGeaResourceCatalogParams, SqliteGeaResourceRepository, SqliteSkillRepository,
        UpsertGeaManagedSkillParams, UpsertSkillParams,
    };

    fn write_skill(dir: &Path, name: &str, description: &str) {
        let skill_dir = dir.join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\nBody"),
        )
        .unwrap();
    }

    #[test]
    fn extract_skill_body_removes_frontmatter() {
        let content = "---\nname: cron\ndescription: Cron\n---\nCron body";
        assert_eq!(extract_skill_body(content), "Cron body");
    }

    #[tokio::test]
    async fn extension_resolver_reads_auto_inject_names_from_skill_catalog() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Arc::new(aionui_extension::SkillPaths {
            data_dir: tmp.path().to_path_buf(),
            user_skills_dir: tmp.path().join("skills"),
            cron_skills_dir: tmp.path().join("cron").join("skills"),
            builtin_skills_dir: tmp.path().join("builtin-skills"),
            builtin_rules_dir: tmp.path().join("builtin-rules"),
            assistant_rules_dir: tmp.path().join("assistant-rules"),
            assistant_skills_dir: tmp.path().join("assistant-skills"),
        });
        write_skill(&paths.builtin_skills_dir, "review", "Top-level builtin");
        write_skill(
            &paths.builtin_skills_dir.join("auto-inject"),
            "auto-cron",
            "Auto-injected builtin",
        );
        write_skill(&paths.cron_skills_dir, "scheduled-task", "Cron source skill");

        let db = aionui_db::init_database_memory().await.unwrap();
        let repo: Arc<dyn ISkillRepository> = Arc::new(SqliteSkillRepository::new(db.pool().clone()));
        aionui_extension::sync_skill_catalog_into_repo(paths.as_ref(), repo.as_ref())
            .await
            .unwrap();

        let resolver = ExtensionSkillResolver::new(paths, repo);

        assert_eq!(resolver.auto_inject_names().await, vec!["auto-cron".to_string()]);
    }

    #[tokio::test]
    async fn extension_resolver_resolves_user_scoped_skill_rows() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Arc::new(aionui_extension::SkillPaths {
            data_dir: tmp.path().to_path_buf(),
            user_skills_dir: tmp.path().join("skills"),
            cron_skills_dir: tmp.path().join("cron").join("skills"),
            builtin_skills_dir: tmp.path().join("builtin-skills"),
            builtin_rules_dir: tmp.path().join("builtin-rules"),
            assistant_rules_dir: tmp.path().join("assistant-rules"),
            assistant_skills_dir: tmp.path().join("assistant-skills"),
        });
        let user_a_skill = tmp.path().join("user-a-skill");
        let user_b_skill = tmp.path().join("user-b-skill");
        write_skill(&user_a_skill, "shared", "User A skill");
        write_skill(&user_b_skill, "shared", "User B skill");

        let db = aionui_db::init_database_memory().await.unwrap();
        sqlx::query(
            "INSERT INTO users (id, user_type, username, password_hash, status, session_generation, created_at, updated_at) \
             VALUES ('user_b', 'local', 'user_b', 'hash', 'active', 0, 1, 1)",
        )
        .execute(db.pool())
        .await
        .unwrap();

        let repo = Arc::new(SqliteSkillRepository::new(db.pool().clone()));
        let user_a_skill_path = user_a_skill.join("shared").to_string_lossy().into_owned();
        let user_b_skill_path = user_b_skill.join("shared").to_string_lossy().into_owned();
        repo.upsert_for_user(
            "system_default_user",
            UpsertSkillParams {
                name: "shared",
                description: Some("User A skill"),
                path: &user_a_skill_path,
                source: "user",
                enabled: true,
            },
        )
        .await
        .unwrap();
        repo.upsert_for_user(
            "user_b",
            UpsertSkillParams {
                name: "shared",
                description: Some("User B skill"),
                path: &user_b_skill_path,
                source: "user",
                enabled: true,
            },
        )
        .await
        .unwrap();

        let resolver = ExtensionSkillResolver::new(paths, repo);
        let resolved = resolver.resolve_skills_for_user("user_b", &["shared".to_owned()]).await;

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].source_path, user_b_skill.join("shared"));
    }

    #[tokio::test]
    async fn extension_resolver_loads_the_exact_managed_skill_for_the_user() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Arc::new(aionui_extension::SkillPaths {
            data_dir: tmp.path().to_path_buf(),
            user_skills_dir: tmp.path().join("skills"),
            cron_skills_dir: tmp.path().join("cron").join("skills"),
            builtin_skills_dir: tmp.path().join("builtin-skills"),
            builtin_rules_dir: tmp.path().join("builtin-rules"),
            assistant_rules_dir: tmp.path().join("assistant-rules"),
            assistant_skills_dir: tmp.path().join("assistant-skills"),
        });
        let managed_path = tmp.path().join("managed").join("forecast-v1");
        std::fs::create_dir_all(&managed_path).unwrap();
        let managed_content = "---\nname: sales-forecast\ndescription: Forecast\n---\nUse governed forecast data.";
        std::fs::write(managed_path.join("SKILL.md"), managed_content).unwrap();
        let managed_digest = format!("{:x}", Sha256::digest(managed_content.as_bytes()));

        let db = aionui_db::init_database_memory().await.unwrap();
        let skill_repo: Arc<dyn ISkillRepository> = Arc::new(SqliteSkillRepository::new(db.pool().clone()));
        let managed_repo = Arc::new(SqliteGeaResourceRepository::new(db.pool().clone()));
        let managed_path_string = managed_path.to_string_lossy().into_owned();
        let rows = [UpsertGeaManagedSkillParams {
            skill_code: "sales-forecast",
            version: "1.0.0",
            name: "Sales forecast",
            description: "Forecast",
            digest: &managed_digest,
            artifact_size: 80,
            state: "active",
            risk_level: Some("LOW"),
            path: &managed_path_string,
        }];
        managed_repo
            .set_active_scope("system_default_user", "tenant-a", "https://gea.test")
            .await
            .unwrap();
        managed_repo
            .replace_catalog(ReplaceGeaResourceCatalogParams {
                user_id: "system_default_user",
                tenant_id: "tenant-a",
                environment: "https://gea.test",
                revision: "resource-r1",
                server_time: None,
                snapshot: "{}",
                skills: &rows,
            })
            .await
            .unwrap();

        let resolver = ExtensionSkillResolver::new(paths.clone(), skill_repo.clone()).with_managed_repo(managed_repo);
        let loaded = resolver
            .load_skill_bodies_for_user("system_default_user", &["sales-forecast".to_owned()])
            .await;

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "sales-forecast");
        assert_eq!(loaded[0].body, "Use governed forecast data.");
        let snapshot = resolver
            .snapshot_managed_skills_for_user("system_default_user", &["sales-forecast".to_owned()])
            .await;
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].version, "1.0.0");
        assert_eq!(snapshot[0].digest, managed_digest);
        assert_eq!(
            resolver
                .load_skill_bodies_for_user_at_snapshot(
                    "system_default_user",
                    &["sales-forecast".to_owned()],
                    &snapshot,
                )
                .await
                .len(),
            1
        );
        let stale_snapshot = [ManagedSkillSnapshot {
            skill_code: "sales-forecast".to_owned(),
            version: "0.9.0".to_owned(),
            digest: "old".to_owned(),
            risk_level: None,
        }];
        assert!(
            resolver
                .load_skill_bodies_for_user_at_snapshot(
                    "system_default_user",
                    &["sales-forecast".to_owned()],
                    &stale_snapshot,
                )
                .await
                .is_empty()
        );

        std::fs::write(managed_path.join("SKILL.md"), "tampered").unwrap();
        assert!(
            resolver
                .load_skill_bodies_for_user_at_snapshot(
                    "system_default_user",
                    &["sales-forecast".to_owned()],
                    &snapshot,
                )
                .await
                .is_empty(),
            "tampered managed content must never execute under the catalog digest"
        );
        std::fs::write(managed_path.join("SKILL.md"), managed_content).unwrap();
        assert!(
            resolver
                .load_skill_bodies_for_user("user_b", &["sales-forecast".to_owned()])
                .await
                .is_empty()
        );

        let local_root = tmp.path().join("local-override");
        write_skill(&local_root, "sales-forecast", "Use the local forecast source.");
        let local_path = local_root.join("sales-forecast").to_string_lossy().into_owned();
        skill_repo
            .upsert_for_user(
                "system_default_user",
                UpsertSkillParams {
                    name: "sales-forecast",
                    description: Some("Local forecast override"),
                    path: &local_path,
                    source: "user",
                    enabled: true,
                },
            )
            .await
            .unwrap();

        assert!(
            resolver
                .snapshot_managed_skills_for_user("system_default_user", &["sales-forecast".to_owned()])
                .await
                .is_empty(),
            "a local Skill with the same name must not be reported as a managed execution"
        );
        let local_loaded = resolver
            .load_skill_bodies_for_user("system_default_user", &["sales-forecast".to_owned()])
            .await;
        assert_eq!(local_loaded.len(), 1);
        assert_eq!(local_loaded[0].body, "Body");
        assert!(local_loaded[0].managed.is_none());

        let frozen_loaded = resolver
            .load_skill_bodies_for_user_at_snapshot("system_default_user", &["sales-forecast".to_owned()], &snapshot)
            .await;
        assert_eq!(frozen_loaded.len(), 1);
        assert_eq!(frozen_loaded[0].body, "Use governed forecast data.");
        assert_eq!(frozen_loaded[0].managed.as_ref(), Some(&snapshot[0]));
    }
}

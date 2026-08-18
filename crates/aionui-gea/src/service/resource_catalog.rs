use std::path::{Path, PathBuf};

use aionui_api_types::{
    GeaCatalogSkill, GeaClientResourceKind, GeaClientResourceSyncResult, GeaClientResourceSyncStatus,
    GeaResourceCatalogEnvelope, GeaResourceCatalogSnapshot, ReportGeaSkillExecutionRequest,
    SyncGeaClientResourcesRequest,
};
use aionui_db::{IGeaResourceRepository, ReplaceGeaResourceCatalogParams, UpsertGeaManagedSkillParams};
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{GeaCredential, GeaService, invalid_upstream, non_empty, upstream_business_error};
use crate::error::GeaError;

const MAX_SKILL_MD_BYTES: u64 = 5 * 1024 * 1024;
const SUPPORTED_RESOURCE_SCHEMA_VERSION: u32 = 1;

struct MaterializedSkill {
    skill_code: String,
    version: String,
    name: String,
    description: String,
    digest: String,
    artifact_size: i64,
    state: String,
    risk_level: Option<String>,
    path: String,
    changed: bool,
}

impl GeaService {
    pub async fn sync_client_resources(
        &self,
        user_id: &str,
        request: SyncGeaClientResourcesRequest,
    ) -> Result<GeaClientResourceSyncResult, GeaError> {
        if request.resources.is_empty() {
            return Err(GeaError::invalid_request("resources 不能为空"));
        }
        if !request.resources.contains(&GeaClientResourceKind::Skills) {
            return Ok(GeaClientResourceSyncResult {
                changed: 0,
                failed: 0,
                revision: None,
                skipped: request.resources.len(),
                status: GeaClientResourceSyncStatus::Completed,
            });
        }

        let Some(credential) = self.credentials.read().await.get(user_id).cloned() else {
            return Ok(GeaClientResourceSyncResult {
                changed: 0,
                failed: 0,
                revision: None,
                skipped: 0,
                status: GeaClientResourceSyncStatus::NotAuthenticated,
            });
        };
        let repo = self.resource_repo()?;
        let managed_root = self.managed_skill_root()?;
        let tenant_id = credential.tenant_id.as_deref().unwrap_or("");
        let previous = repo
            .load_catalog(user_id, tenant_id, &self.base_url)
            .await
            .map_err(|error| {
                tracing::error!(error = %error, user_id, "failed to load GEA Resource Catalog cache");
                GeaError::server_error("GEA_RESOURCE_STORAGE_ERROR", "GEA 资源缓存不可用")
            })?;

        let envelope = self
            .fetch_resource_catalog(user_id, &credential, previous.as_ref().map(|row| row.revision.as_str()))
            .await?;
        match envelope.status.as_str() {
            "not_modified" => {
                let skipped = repo
                    .list_managed_skills_for_user(user_id)
                    .await
                    .map_err(|error| {
                        tracing::error!(error = %error, user_id, "failed to list cached GEA skills");
                        GeaError::server_error("GEA_RESOURCE_STORAGE_ERROR", "GEA 资源缓存不可用")
                    })?
                    .len();
                return Ok(GeaClientResourceSyncResult {
                    changed: 0,
                    failed: 0,
                    revision: envelope.revision.or_else(|| previous.map(|row| row.revision)),
                    skipped,
                    status: GeaClientResourceSyncStatus::Completed,
                });
            }
            "error" => {
                tracing::warn!(
                    user_id,
                    "GEA Resource Catalog returned an error status; keeping last-good cache"
                );
                return Ok(GeaClientResourceSyncResult {
                    changed: 0,
                    failed: 1,
                    revision: previous.map(|row| row.revision),
                    skipped: 0,
                    status: GeaClientResourceSyncStatus::Unavailable,
                });
            }
            "ok" => {}
            _ => return Err(invalid_upstream("GEA Resource Catalog status 无效")),
        }

        let snapshot = envelope
            .snapshot
            .ok_or_else(|| invalid_upstream("GEA Resource Catalog 缺少 snapshot"))?;
        validate_snapshot(&snapshot)?;
        if snapshot.tenant_id.as_deref().unwrap_or("").trim() != tenant_id {
            return Err(invalid_upstream("GEA Resource Catalog tenantId 与当前登录租户不一致"));
        }
        if envelope
            .revision
            .as_deref()
            .is_some_and(|revision| revision != snapshot.revision)
        {
            return Err(invalid_upstream("GEA Resource Catalog revision 不一致"));
        }

        let cached = repo.list_managed_skills_for_user(user_id).await.map_err(|error| {
            tracing::error!(error = %error, user_id, "failed to list cached GEA skills");
            GeaError::server_error("GEA_RESOURCE_STORAGE_ERROR", "GEA 资源缓存不可用")
        })?;
        let mut materialized = Vec::with_capacity(snapshot.skills.len());
        for skill in snapshot.skills.iter().filter(|skill| is_active_state(&skill.state)) {
            let existing = cached.iter().find(|row| {
                row.skill_code == skill.id
                    && row.version == skill.version
                    && normalize_digest(&row.digest) == normalize_digest(&skill.digest)
                    && Path::new(&row.path).join("SKILL.md").is_file()
            });
            let existing = match existing {
                Some(row) if cached_md_matches(row.path.as_str(), &skill.digest, skill.artifact_size).await => {
                    Some(row)
                }
                _ => None,
            };
            match existing {
                Some(row) => materialized.push(materialized_from_cache(skill, row.path.clone())?),
                None => {
                    materialized.push(
                        self.download_and_materialize_skill(
                            user_id,
                            tenant_id,
                            &credential,
                            managed_root.as_ref(),
                            skill,
                        )
                        .await?,
                    );
                }
            }
        }

        let snapshot_json = serde_json::to_string(&snapshot).map_err(|error| {
            tracing::error!(error = %error, user_id, "failed to encode validated GEA Resource Catalog");
            GeaError::server_error("GEA_RESOURCE_STORAGE_ERROR", "GEA 资源缓存不可用")
        })?;
        let rows = materialized
            .iter()
            .map(|skill| UpsertGeaManagedSkillParams {
                skill_code: &skill.skill_code,
                version: &skill.version,
                name: &skill.name,
                description: &skill.description,
                digest: &skill.digest,
                artifact_size: skill.artifact_size,
                state: &skill.state,
                risk_level: skill.risk_level.as_deref(),
                path: &skill.path,
            })
            .collect::<Vec<_>>();
        repo.replace_catalog(ReplaceGeaResourceCatalogParams {
            user_id,
            tenant_id,
            environment: &self.base_url,
            revision: &snapshot.revision,
            server_time: envelope.server_time.as_deref(),
            snapshot: &snapshot_json,
            skills: &rows,
        })
        .await
        .map_err(|error| {
            tracing::error!(error = %error, user_id, "failed to store validated GEA Resource Catalog");
            GeaError::server_error("GEA_RESOURCE_STORAGE_ERROR", "GEA 资源缓存不可用")
        })?;

        let changed = materialized.iter().filter(|skill| skill.changed).count();
        tracing::info!(
            user_id,
            revision = snapshot.revision,
            changed,
            skipped = materialized.len().saturating_sub(changed),
            "GEA managed Skill catalog synchronized"
        );
        Ok(GeaClientResourceSyncResult {
            changed,
            failed: 0,
            revision: Some(snapshot.revision),
            skipped: materialized.len().saturating_sub(changed),
            status: GeaClientResourceSyncStatus::Completed,
        })
    }

    fn resource_repo(&self) -> Result<&dyn IGeaResourceRepository, GeaError> {
        self.resource_repo
            .as_deref()
            .ok_or_else(|| GeaError::server_error("GEA_RESOURCE_STORAGE_ERROR", "GEA 资源缓存未配置"))
    }

    fn managed_skill_root(&self) -> Result<&PathBuf, GeaError> {
        self.managed_skill_root
            .as_deref()
            .ok_or_else(|| GeaError::server_error("GEA_RESOURCE_STORAGE_ERROR", "GEA Skill 目录未配置"))
    }

    async fn fetch_resource_catalog(
        &self,
        user_id: &str,
        credential: &GeaCredential,
        revision: Option<&str>,
    ) -> Result<GeaResourceCatalogEnvelope, GeaError> {
        let mut request = self
            .client
            .get(format!("{}/aidata/client-resource-catalog/my", self.base_url))
            .headers(credential_headers(credential)?);
        if let Some(revision) = revision {
            request = request.query(&[("revision", revision)]);
        }
        let response = request
            .send()
            .await
            .map_err(|_| GeaError::bad_gateway("GEA_NETWORK_ERROR", "无法连接 GEA 服务"))?;
        let status = response.status();
        let value = response
            .json::<Value>()
            .await
            .map_err(|_| invalid_upstream("GEA Resource Catalog 返回了无效 JSON"))?;
        if !status.is_success() {
            let error = upstream_business_error(&value, status.as_u16());
            if error.is_unauthorized() {
                self.invalidate_auth_session(user_id).await;
            }
            return Err(error);
        }
        if value.get("success").and_then(Value::as_bool) == Some(false) {
            let error = upstream_business_error(&value, status.as_u16());
            if error.is_unauthorized() {
                self.invalidate_auth_session(user_id).await;
            }
            return Err(error);
        }
        let payload = value.get("result").cloned().unwrap_or(value);
        serde_json::from_value(payload).map_err(|_| invalid_upstream("GEA Resource Catalog 结构无效"))
    }

    async fn download_and_materialize_skill(
        &self,
        user_id: &str,
        tenant_id: &str,
        credential: &GeaCredential,
        managed_root: &Path,
        skill: &GeaCatalogSkill,
    ) -> Result<MaterializedSkill, GeaError> {
        validate_catalog_skill(skill)?;
        let response = self
            .client
            .get(format!(
                "{}/aidata/client-resource-catalog/skill-artifact",
                self.base_url
            ))
            .headers(credential_headers(credential)?)
            .query(&[
                ("skillCode", skill.id.as_str()),
                ("version", skill.version.as_str()),
                ("format", "md"),
            ])
            .send()
            .await
            .map_err(|_| GeaError::bad_gateway("GEA_NETWORK_ERROR", "无法下载 GEA Skill"))?;
        let status = response.status();
        if !status.is_success() {
            let value = response.json::<Value>().await.unwrap_or(Value::Null);
            let error = upstream_business_error(&value, status.as_u16());
            if error.is_unauthorized() {
                self.invalidate_auth_session(user_id).await;
            }
            return Err(error);
        }
        let headers = response.headers().clone();
        let bytes = read_bounded_body(response, MAX_SKILL_MD_BYTES).await?;
        let actual_size = u64::try_from(bytes.len()).map_err(|_| invalid_upstream("GEA Skill 大小无效"))?;
        let expected_digest = normalize_digest(&skill.digest);
        let header_digest = required_header(&headers, "x-skill-digest")?;
        let header_size = required_header(&headers, "x-skill-size")?
            .parse::<u64>()
            .map_err(|_| invalid_upstream("GEA Skill X-Skill-Size 无效"))?;
        let header_version = required_header(&headers, "x-skill-version")?;
        if normalize_digest(&header_digest) != expected_digest {
            return Err(GeaError::conflict("ARTIFACT_DIGEST_MISMATCH", "GEA Skill 摘要校验失败"));
        }
        let actual_digest = hex_digest(&bytes);
        if actual_digest != expected_digest {
            return Err(GeaError::conflict("ARTIFACT_DIGEST_MISMATCH", "GEA Skill 摘要校验失败"));
        }
        if header_size != actual_size || skill.artifact_size != actual_size {
            return Err(GeaError::conflict("ARTIFACT_SIZE_MISMATCH", "GEA Skill 大小校验失败"));
        }
        if header_version != skill.version {
            return Err(GeaError::conflict(
                "ARTIFACT_VERSION_MISMATCH",
                "GEA Skill 版本校验失败",
            ));
        }
        let scope = hex_digest(format!("{user_id}\0{tenant_id}\0{}", self.base_url).as_bytes());
        let destination = managed_root.join(scope).join(&skill.id).join(&expected_digest);
        std::str::from_utf8(&bytes).map_err(|_| invalid_upstream("GEA SKILL.md 不是有效 UTF-8"))?;
        materialize_md_atomically(&destination, &bytes).await?;
        Ok(MaterializedSkill {
            skill_code: skill.id.clone(),
            version: skill.version.clone(),
            name: skill.name.display_value().to_owned(),
            description: skill.description.display_value().to_owned(),
            digest: expected_digest,
            artifact_size: i64::try_from(actual_size).map_err(|_| invalid_upstream("GEA Skill 大小无效"))?,
            state: normalize_state(&skill.state)?.to_owned(),
            risk_level: skill.risk_level.clone(),
            path: destination.to_string_lossy().into_owned(),
            changed: true,
        })
    }
}

impl GeaService {
    pub async fn report_skill_execution(
        &self,
        user_id: &str,
        report: ReportGeaSkillExecutionRequest,
    ) -> Result<(), GeaError> {
        let credential = self
            .credentials
            .read()
            .await
            .get(user_id)
            .cloned()
            .ok_or_else(GeaError::unauthenticated)?;
        let headers = credential_headers(&credential)?;
        let response = self
            .client
            .post(format!(
                "{}/aidata/client-resource-catalog/skill-execute/report",
                self.base_url
            ))
            .headers(headers)
            .json(&report)
            .send()
            .await
            .map_err(|_| GeaError::bad_gateway("GEA_NETWORK_ERROR", "无法连接 GEA 服务"))?;
        if response.status().is_success() {
            return Ok(());
        }
        let status = response.status();
        let value = response.json::<Value>().await.unwrap_or(Value::Null);
        let error = upstream_business_error(&value, status.as_u16());
        if error.is_unauthorized() {
            self.invalidate_auth_session(user_id).await;
        }
        Err(error)
    }
}

fn materialized_from_cache(skill: &GeaCatalogSkill, path: String) -> Result<MaterializedSkill, GeaError> {
    validate_catalog_skill(skill)?;
    Ok(MaterializedSkill {
        skill_code: skill.id.clone(),
        version: skill.version.clone(),
        name: skill.name.display_value().to_owned(),
        description: skill.description.display_value().to_owned(),
        digest: normalize_digest(&skill.digest),
        artifact_size: i64::try_from(skill.artifact_size).map_err(|_| invalid_upstream("GEA Skill 大小无效"))?,
        state: normalize_state(&skill.state)?.to_owned(),
        risk_level: skill.risk_level.clone(),
        path,
        changed: false,
    })
}

fn validate_snapshot(snapshot: &GeaResourceCatalogSnapshot) -> Result<(), GeaError> {
    if snapshot.schema_version != SUPPORTED_RESOURCE_SCHEMA_VERSION {
        return Err(invalid_upstream("GEA Resource Catalog schemaVersion 不受支持"));
    }
    if snapshot.revision.trim().is_empty() {
        return Err(invalid_upstream("GEA Resource Catalog revision 不能为空"));
    }
    let mut ids = std::collections::HashSet::new();
    for skill in &snapshot.skills {
        validate_catalog_skill(skill)?;
        if !ids.insert(skill.id.as_str()) {
            return Err(invalid_upstream("GEA Resource Catalog 包含重复 Skill"));
        }
    }
    Ok(())
}

fn validate_catalog_skill(skill: &GeaCatalogSkill) -> Result<(), GeaError> {
    if !is_safe_component(&skill.id) || skill.version.trim().is_empty() {
        return Err(invalid_upstream("GEA Resource Catalog Skill 字段无效"));
    }
    if !is_active_state(&skill.state) {
        return Ok(());
    }
    if skill.artifact_ref.trim().is_empty() {
        return Err(invalid_upstream("GEA Resource Catalog Skill 字段无效"));
    }
    let digest = normalize_digest(&skill.digest);
    if digest.len() != 64 || !digest.bytes().all(|value| value.is_ascii_hexdigit()) {
        return Err(invalid_upstream("GEA Resource Catalog Skill digest 无效"));
    }
    if skill.artifact_size == 0 || skill.artifact_size > MAX_SKILL_MD_BYTES {
        return Err(GeaError::from_http_status(
            413,
            "ARTIFACT_TOO_LARGE",
            "GEA Skill 超出大小限制",
        ));
    }
    normalize_state(&skill.state)?;
    Ok(())
}

fn normalize_state(state: &str) -> Result<&'static str, GeaError> {
    if is_active_state(state) {
        Ok("active")
    } else {
        Err(GeaError::from_http_status(403, "SKILL_OFFLINE", "GEA Skill 当前不可用"))
    }
}

fn is_active_state(state: &str) -> bool {
    matches!(state.trim().to_ascii_lowercase().as_str(), "active" | "published") || state.trim() == "已发布"
}

fn is_safe_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        && value != "."
        && value != ".."
}

fn credential_headers(credential: &GeaCredential) -> Result<HeaderMap, GeaError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-access-token",
        HeaderValue::from_str(credential.access_token.as_ref())
            .map_err(|_| GeaError::invalid_request("GEA access token 格式无效"))?,
    );
    if let Some(tenant_id) = credential.tenant_id.as_deref() {
        headers.insert(
            "x-tenant-id",
            HeaderValue::from_str(tenant_id).map_err(|_| GeaError::invalid_request("GEA tenantId 格式无效"))?,
        );
    }
    Ok(headers)
}

async fn read_bounded_body(mut response: reqwest::Response, limit: u64) -> Result<Vec<u8>, GeaError> {
    if response.content_length().is_some_and(|size| size > limit) {
        return Err(GeaError::from_http_status(
            413,
            "ARTIFACT_TOO_LARGE",
            "GEA Skill 超出大小限制",
        ));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| GeaError::bad_gateway("ARTIFACT_READ_FAILED", "GEA Skill 下载失败"))?
    {
        let next = body.len().saturating_add(chunk.len());
        if u64::try_from(next).unwrap_or(u64::MAX) > limit {
            return Err(GeaError::from_http_status(
                413,
                "ARTIFACT_TOO_LARGE",
                "GEA Skill 超出大小限制",
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn required_header(headers: &HeaderMap, name: &str) -> Result<String, GeaError> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .and_then(non_empty)
        .ok_or_else(|| invalid_upstream(format!("GEA Skill 响应缺少 {name}")))
}

fn normalize_digest(value: &str) -> String {
    value
        .trim()
        .strip_prefix("sha256:")
        .unwrap_or(value.trim())
        .to_ascii_lowercase()
}

fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

async fn materialize_md_atomically(destination: &Path, bytes: &[u8]) -> Result<(), GeaError> {
    let existing = destination.join("SKILL.md");
    if let Ok(current) = tokio::fs::read(&existing).await
        && current == bytes
    {
        return Ok(());
    }
    let parent = destination
        .parent()
        .ok_or_else(|| GeaError::server_error("GEA_RESOURCE_STORAGE_ERROR", "GEA Skill 目录无效"))?;
    tokio::fs::create_dir_all(parent).await.map_err(storage_error)?;
    let staging = parent.join(format!(".staging-{}", Uuid::now_v7()));
    tokio::fs::create_dir(&staging).await.map_err(storage_error)?;
    if let Err(error) = tokio::fs::write(staging.join("SKILL.md"), bytes).await {
        let _ = tokio::fs::remove_dir_all(&staging).await;
        return Err(storage_error(error));
    }
    if destination.exists() {
        tokio::fs::remove_dir_all(destination).await.map_err(storage_error)?;
    }
    match tokio::fs::rename(&staging, destination).await {
        Ok(()) => Ok(()),
        Err(_) if destination.join("SKILL.md").is_file() => {
            let _ = tokio::fs::remove_dir_all(&staging).await;
            Ok(())
        }
        Err(error) => {
            let _ = tokio::fs::remove_dir_all(&staging).await;
            Err(storage_error(error))
        }
    }
}

async fn cached_md_matches(path: &str, expected_digest: &str, expected_size: u64) -> bool {
    let Ok(bytes) = tokio::fs::read(Path::new(path).join("SKILL.md")).await else {
        return false;
    };
    u64::try_from(bytes.len()).ok() == Some(expected_size)
        && hex_digest(&bytes) == normalize_digest(expected_digest)
        && std::str::from_utf8(&bytes).is_ok()
}

fn storage_error(error: std::io::Error) -> GeaError {
    tracing::error!(error = %error, "failed to materialize GEA Skill");
    GeaError::server_error("GEA_RESOURCE_STORAGE_ERROR", "GEA Skill 本地保存失败")
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_api_types::SetGeaAuthSessionRequest;
    use aionui_db::{SqliteGeaResourceRepository, init_database_memory};
    use wiremock::matchers::{body_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn sync_downloads_validated_skill_and_persists_last_good() {
        let server = MockServer::start().await;
        let body = b"---\nname: sales-forecast\ndescription: Query forecasts\n---\nUse business data.";
        let digest = hex_digest(body);
        Mock::given(method("GET"))
            .and(path("/aidata/client-resource-catalog/my"))
            .and(header("x-access-token", "token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "ok",
                "revision": "resource-r1",
                "snapshot": {
                    "schemaVersion": 1,
                    "revision": "resource-r1",
                    "tenantId": "tenant-a",
                    "skills": [{
                        "id": "sales-forecast",
                        "version": "1.0.0",
                        "name": "Sales forecast",
                        "description": "Query forecasts",
                        "artifactRef": "skills/sales-forecast/1.0.0",
                        "digest": digest,
                        "artifactSize": body.len(),
                        "state": "active"
                    }]
                }
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/aidata/client-resource-catalog/skill-artifact"))
            .and(query_param("skillCode", "sales-forecast"))
            .and(query_param("version", "1.0.0"))
            .and(query_param("format", "md"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-skill-digest", digest.as_str())
                    .insert_header("x-skill-size", body.len().to_string().as_str())
                    .insert_header("x-skill-version", "1.0.0")
                    .set_body_bytes(body),
            )
            .mount(&server)
            .await;

        let db = init_database_memory().await.unwrap();
        let repo = std::sync::Arc::new(SqliteGeaResourceRepository::new(db.pool().clone()));
        let root = tempfile::tempdir().unwrap();
        let service = GeaService::new(reqwest::Client::new(), server.uri())
            .unwrap()
            .with_resource_catalog(repo.clone(), root.path().to_path_buf());
        service
            .set_auth_session(
                "system_default_user",
                SetGeaAuthSessionRequest {
                    access_token: "token".into(),
                    tenant_id: Some("tenant-a".into()),
                },
            )
            .await
            .unwrap();

        let result = service
            .sync_client_resources(
                "system_default_user",
                SyncGeaClientResourcesRequest {
                    resources: vec![GeaClientResourceKind::Skills],
                },
            )
            .await
            .unwrap();
        assert_eq!(result.status, GeaClientResourceSyncStatus::Completed);
        assert_eq!(result.changed, 1);
        let rows = repo.list_managed_skills_for_user("system_default_user").await.unwrap();
        assert_eq!(rows[0].skill_code, "sales-forecast");
        let cached_file = Path::new(&rows[0].path).join("SKILL.md");
        assert!(cached_file.is_file());
        assert_eq!(
            repo.load_catalog("system_default_user", "tenant-a", &server.uri())
                .await
                .unwrap()
                .unwrap()
                .revision,
            "resource-r1"
        );

        tokio::fs::write(&cached_file, b"tampered").await.unwrap();
        let repaired = service
            .sync_client_resources(
                "system_default_user",
                SyncGeaClientResourcesRequest {
                    resources: vec![GeaClientResourceKind::Skills],
                },
            )
            .await
            .unwrap();
        assert_eq!(repaired.changed, 1);
        assert_eq!(tokio::fs::read(&cached_file).await.unwrap(), body);
    }

    #[tokio::test]
    async fn sync_rejects_a_catalog_for_a_different_tenant() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/aidata/client-resource-catalog/my"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "ok",
                "revision": "resource-r1",
                "snapshot": {
                    "schemaVersion": 1,
                    "revision": "resource-r1",
                    "tenantId": "tenant-b",
                    "skills": []
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let db = init_database_memory().await.unwrap();
        let repo = std::sync::Arc::new(SqliteGeaResourceRepository::new(db.pool().clone()));
        let root = tempfile::tempdir().unwrap();
        let service = GeaService::new(reqwest::Client::new(), server.uri())
            .unwrap()
            .with_resource_catalog(repo.clone(), root.path().to_path_buf());
        service
            .set_auth_session(
                "system_default_user",
                SetGeaAuthSessionRequest {
                    access_token: "token".into(),
                    tenant_id: Some("tenant-a".into()),
                },
            )
            .await
            .unwrap();

        let error = service
            .sync_client_resources(
                "system_default_user",
                SyncGeaClientResourcesRequest {
                    resources: vec![GeaClientResourceKind::Skills],
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.body.code, "GEA_INVALID_RESPONSE");
        assert!(
            repo.list_managed_skills_for_user("system_default_user")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn catalog_business_error_in_a_successful_http_response_is_preserved() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/aidata/client-resource-catalog/my"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": false,
                "message": "Resource catalog endpoint is unavailable",
                "code": 404,
                "result": null
            })))
            .expect(1)
            .mount(&server)
            .await;

        let service = GeaService::new(reqwest::Client::new(), server.uri()).unwrap();
        service
            .set_auth_session(
                "system_default_user",
                SetGeaAuthSessionRequest {
                    access_token: "token".into(),
                    tenant_id: Some("tenant-a".into()),
                },
            )
            .await
            .unwrap();
        let credential = service
            .credentials
            .read()
            .await
            .get("system_default_user")
            .cloned()
            .unwrap();

        let error = service
            .fetch_resource_catalog("system_default_user", &credential, None)
            .await
            .unwrap_err();

        assert_eq!(error.body.code, "404");
        assert_eq!(error.body.message, "Resource catalog endpoint is unavailable");
    }

    #[tokio::test]
    async fn managed_skill_execution_is_reported_with_the_frozen_identity() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/aidata/client-resource-catalog/skill-execute/report"))
            .and(header("x-access-token", "token"))
            .and(body_json(serde_json::json!({
                "skillCode": "sales-forecast",
                "version": "1.0.0",
                "digest": "abcd",
                "success": true,
                "executedAt": "2026-08-18T01:02:03Z",
                "durationMs": 42,
                "resultSize": 0,
                "riskLevel": "LOW"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"success": true})))
            .expect(1)
            .mount(&server)
            .await;
        let service = GeaService::new(reqwest::Client::new(), server.uri()).unwrap();
        service
            .set_auth_session(
                "system_default_user",
                SetGeaAuthSessionRequest {
                    access_token: "token".into(),
                    tenant_id: Some("tenant-a".into()),
                },
            )
            .await
            .unwrap();

        service
            .report_skill_execution(
                "system_default_user",
                ReportGeaSkillExecutionRequest {
                    skill_code: "sales-forecast".into(),
                    version: "1.0.0".into(),
                    digest: "abcd".into(),
                    success: true,
                    executed_at: "2026-08-18T01:02:03Z".into(),
                    duration_ms: 42,
                    result_size: 0,
                    risk_level: Some("LOW".into()),
                    error_code: None,
                    error_message: None,
                },
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn catalog_error_keeps_the_previous_last_good_snapshot() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/aidata/client-resource-catalog/my"))
            .and(query_param("revision", "resource-r1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "error",
                "lastGoodRevision": "resource-r1",
                "error": {"code": "GEA_TEMPORARILY_UNAVAILABLE", "retryable": true}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let db = init_database_memory().await.unwrap();
        let repo = std::sync::Arc::new(SqliteGeaResourceRepository::new(db.pool().clone()));
        let root = tempfile::tempdir().unwrap();
        let service = GeaService::new(reqwest::Client::new(), server.uri())
            .unwrap()
            .with_resource_catalog(repo.clone(), root.path().to_path_buf());
        service
            .set_auth_session(
                "system_default_user",
                SetGeaAuthSessionRequest {
                    access_token: "token".into(),
                    tenant_id: Some("tenant-a".into()),
                },
            )
            .await
            .unwrap();
        repo.replace_catalog(ReplaceGeaResourceCatalogParams {
            user_id: "system_default_user",
            tenant_id: "tenant-a",
            environment: &server.uri(),
            revision: "resource-r1",
            server_time: None,
            snapshot: r#"{"schemaVersion":1,"revision":"resource-r1","skills":[]}"#,
            skills: &[],
        })
        .await
        .unwrap();

        let result = service
            .sync_client_resources(
                "system_default_user",
                SyncGeaClientResourcesRequest {
                    resources: vec![GeaClientResourceKind::Skills],
                },
            )
            .await
            .unwrap();

        assert_eq!(result.status, GeaClientResourceSyncStatus::Unavailable);
        assert_eq!(result.revision.as_deref(), Some("resource-r1"));
        assert_eq!(
            repo.load_catalog("system_default_user", "tenant-a", &server.uri())
                .await
                .unwrap()
                .unwrap()
                .revision,
            "resource-r1"
        );
    }

    #[tokio::test]
    async fn offline_skill_is_removed_instead_of_preserving_an_executable_cache() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/aidata/client-resource-catalog/my"))
            .and(query_param("revision", "resource-r1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "ok",
                "revision": "resource-r2",
                "snapshot": {
                    "schemaVersion": 1,
                    "revision": "resource-r2",
                    "tenantId": "tenant-a",
                    "skills": [{
                        "id": "sales-forecast",
                        "version": "1.0.0",
                        "name": "Sales forecast",
                        "description": "Withdrawn",
                        "artifactRef": "",
                        "digest": "",
                        "artifactSize": 0,
                        "state": "offline"
                    }]
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        let db = init_database_memory().await.unwrap();
        let repo = std::sync::Arc::new(SqliteGeaResourceRepository::new(db.pool().clone()));
        let root = tempfile::tempdir().unwrap();
        let cached_path = root.path().join("cached");
        std::fs::create_dir_all(&cached_path).unwrap();
        std::fs::write(cached_path.join("SKILL.md"), "Body").unwrap();
        let cached_path_string = cached_path.to_string_lossy().into_owned();
        let cached_digest = "a".repeat(64);
        let cached_rows = [UpsertGeaManagedSkillParams {
            skill_code: "sales-forecast",
            version: "1.0.0",
            name: "Sales forecast",
            description: "Forecast",
            digest: &cached_digest,
            artifact_size: 4,
            state: "active",
            risk_level: None,
            path: &cached_path_string,
        }];
        repo.set_active_scope("system_default_user", "tenant-a", &server.uri())
            .await
            .unwrap();
        repo.replace_catalog(ReplaceGeaResourceCatalogParams {
            user_id: "system_default_user",
            tenant_id: "tenant-a",
            environment: &server.uri(),
            revision: "resource-r1",
            server_time: None,
            snapshot: "{}",
            skills: &cached_rows,
        })
        .await
        .unwrap();
        let service = GeaService::new(reqwest::Client::new(), server.uri())
            .unwrap()
            .with_resource_catalog(repo.clone(), root.path().to_path_buf());
        service
            .set_auth_session(
                "system_default_user",
                SetGeaAuthSessionRequest {
                    access_token: "token".into(),
                    tenant_id: Some("tenant-a".into()),
                },
            )
            .await
            .unwrap();

        service
            .sync_client_resources(
                "system_default_user",
                SyncGeaClientResourcesRequest {
                    resources: vec![GeaClientResourceKind::Skills],
                },
            )
            .await
            .unwrap();

        assert!(
            repo.list_managed_skills_for_user("system_default_user")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn not_modified_reuses_the_previous_last_good_snapshot() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/aidata/client-resource-catalog/my"))
            .and(query_param("revision", "resource-r1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "not_modified",
                "revision": "resource-r1"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let db = init_database_memory().await.unwrap();
        let repo = std::sync::Arc::new(SqliteGeaResourceRepository::new(db.pool().clone()));
        let root = tempfile::tempdir().unwrap();
        let service = GeaService::new(reqwest::Client::new(), server.uri())
            .unwrap()
            .with_resource_catalog(repo.clone(), root.path().to_path_buf());
        service
            .set_auth_session(
                "system_default_user",
                SetGeaAuthSessionRequest {
                    access_token: "token".into(),
                    tenant_id: Some("tenant-a".into()),
                },
            )
            .await
            .unwrap();
        repo.replace_catalog(ReplaceGeaResourceCatalogParams {
            user_id: "system_default_user",
            tenant_id: "tenant-a",
            environment: &server.uri(),
            revision: "resource-r1",
            server_time: None,
            snapshot: r#"{"schemaVersion":1,"revision":"resource-r1","skills":[]}"#,
            skills: &[],
        })
        .await
        .unwrap();

        let result = service
            .sync_client_resources(
                "system_default_user",
                SyncGeaClientResourcesRequest {
                    resources: vec![GeaClientResourceKind::Skills],
                },
            )
            .await
            .unwrap();

        assert_eq!(result.status, GeaClientResourceSyncStatus::Completed);
        assert_eq!(result.changed, 0);
        assert_eq!(result.revision.as_deref(), Some("resource-r1"));
    }
}

use std::collections::HashSet;
use std::str::FromStr;
use std::time::{Duration, Instant};

use aionui_api_types::{GeaSalesPlanId, GeaSalesPlanSubmitReceipt, GeaSalesPlanSubmitRequest};
use axum::http::StatusCode;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::{GEA_CONNECT_TIMEOUT, GEA_REQUEST_TIMEOUT, GeaService, non_empty, parse_retry_after_ms, value_as_string};
use crate::error::GeaError;

const SERVICE_SCOPE: &str = "sales-plan:write";
const SERVICE_TOKEN_EXPIRY_SKEW: Duration = Duration::from_secs(30);
const MAX_SUBMIT_ATTEMPTS: usize = 3;
const MAX_SUBMIT_RETRY_DELAY: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub(super) struct SalesPlanServiceIdentity {
    client_id: String,
    client_secret: String,
    client: reqwest::Client,
}

impl SalesPlanServiceIdentity {
    pub(super) fn from_env() -> Result<Option<Self>, GeaError> {
        let client_id = std::env::var("AIONUI_GEA_SALES_PLAN_CLIENT_ID")
            .ok()
            .and_then(non_empty);
        let client_secret = std::env::var("AIONUI_GEA_SALES_PLAN_CLIENT_SECRET")
            .ok()
            .and_then(non_empty);
        match (client_id, client_secret) {
            (None, None) => Ok(None),
            (Some(client_id), Some(client_secret)) => Self::new(client_id, client_secret).map(Some),
            _ => Err(GeaError::server_error(
                "GEA_SALES_PLAN_SERVICE_CONFIG_INVALID",
                "GEA 销售计划服务凭证配置不完整",
            )),
        }
    }

    fn new(client_id: String, client_secret: String) -> Result<Self, GeaError> {
        // Service credentials and authenticated writes must remain on the
        // configured endpoint. In particular, 307/308 would forward form-body
        // secrets even when a client strips cross-origin Authorization headers.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(GEA_CONNECT_TIMEOUT)
            .timeout(GEA_REQUEST_TIMEOUT)
            .build()
            .map_err(|_| GeaError::server_error("GEA_CLIENT_INIT_FAILED", "GEA 客户端初始化失败"))?;
        Ok(Self {
            client_id,
            client_secret,
            client,
        })
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn for_test(client_id: &str, client_secret: &str) -> Self {
        Self::new(client_id.to_owned(), client_secret.to_owned()).expect("build service identity test client")
    }
}

pub(super) struct SalesPlanTokenCache {
    access_token: String,
    expires_at: Instant,
}

#[derive(Deserialize)]
struct ServiceTokenResponse {
    access_token: String,
    token_type: String,
    expires_in: u64,
    scope: String,
}

impl GeaService {
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_sales_plan_service_identity_for_test(mut self, client_id: &str, client_secret: &str) -> Self {
        self.sales_plan_service_identity = Some(SalesPlanServiceIdentity::for_test(client_id, client_secret));
        self
    }

    pub async fn sales_plan_submit(
        &self,
        idempotency_key: &str,
        request_id: &str,
        request: &GeaSalesPlanSubmitRequest,
    ) -> Result<GeaSalesPlanSubmitReceipt, GeaError> {
        validate_command_headers(idempotency_key, request_id, None)?;
        validate_submit(request)?;
        if self.sales_plan_service_identity.is_none() {
            return Err(GeaError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "GEA_SALES_PLAN_SERVICE_UNAVAILABLE",
                "GEA 销售计划服务接入未配置",
            ));
        }

        let body = upstream_json(request)?;
        let mut refreshed_after_unauthorized = false;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let token = self.sales_plan_service_token(false).await?;
            let response = self
                .sales_plan_submit_once(&token, idempotency_key, request_id, &body)
                .await;
            match response {
                Ok(value) => return extract_result(value),
                Err(error)
                    if error.status == StatusCode::UNAUTHORIZED
                        && !refreshed_after_unauthorized
                        && attempt < MAX_SUBMIT_ATTEMPTS =>
                {
                    self.sales_plan_service_token(true).await?;
                    refreshed_after_unauthorized = true;
                }
                Err(error) if should_retry_submit(&error, attempt) => {
                    let Some(delay) = submit_retry_delay(&error, attempt) else {
                        return Err(error);
                    };
                    tokio::time::sleep(delay).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn sales_plan_submit_once(
        &self,
        token: &str,
        idempotency_key: &str,
        request_id: &str,
        body: &Value,
    ) -> Result<Value, GeaError> {
        let identity = self.sales_plan_service_identity.as_ref().ok_or_else(|| {
            GeaError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "GEA_SALES_PLAN_SERVICE_UNAVAILABLE",
                "GEA 销售计划服务接入未配置",
            )
        })?;
        let mut headers = HeaderMap::new();
        let authorization = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| GeaError::server_error("GEA_SERVICE_TOKEN_INVALID", "GEA 服务 Token 格式无效"))?;
        headers.insert(AUTHORIZATION, authorization);
        insert_header(&mut headers, "idempotency-key", idempotency_key, "Idempotency-Key")?;
        insert_header(&mut headers, "x-request-id", request_id, "X-Request-Id")?;
        self.sales_plan_send(
            identity
                .client
                .post(format!("{}/api/v1/internal/sales-plans", self.base_url))
                .headers(headers)
                .json(body),
        )
        .await
    }

    async fn sales_plan_service_token(&self, force_refresh: bool) -> Result<String, GeaError> {
        let mut cache = self.sales_plan_token_cache.lock().await;
        if !force_refresh
            && let Some(cached) = cache.as_ref()
            && cached.expires_at > Instant::now() + SERVICE_TOKEN_EXPIRY_SKEW
        {
            return Ok(cached.access_token.clone());
        }
        let identity = self.sales_plan_service_identity.as_ref().ok_or_else(|| {
            GeaError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "GEA_SALES_PLAN_SERVICE_UNAVAILABLE",
                "GEA 销售计划服务接入未配置",
            )
        })?;
        let request = identity
            .client
            .post(format!("{}/api/v1/internal/auth/token", self.base_url))
            .form(&[
                ("grant_type", "client_credentials"),
                ("client_id", identity.client_id.as_str()),
                ("client_secret", identity.client_secret.as_str()),
                ("scope", SERVICE_SCOPE),
                ("requested_ttl_seconds", "600"),
            ]);
        let value = match self.sales_plan_send(request).await {
            Ok(value) => value,
            Err(error) => return Err(sanitize_service_token_error(error)),
        };
        let token: ServiceTokenResponse = extract_result(value)?;
        if !token.token_type.eq_ignore_ascii_case("bearer") || token.scope != SERVICE_SCOPE || token.expires_in == 0 {
            return Err(super::invalid_upstream("GEA 返回了无效服务 Token"));
        }
        let access_token =
            non_empty(token.access_token).ok_or_else(|| super::invalid_upstream("GEA 返回了空服务 Token"))?;
        let expires_at = Instant::now()
            .checked_add(Duration::from_secs(token.expires_in))
            .ok_or_else(|| super::invalid_upstream("GEA 返回了无效服务 Token 有效期"))?;
        *cache = Some(SalesPlanTokenCache {
            access_token: access_token.clone(),
            expires_at,
        });
        Ok(access_token)
    }

    async fn sales_plan_send(&self, request: reqwest::RequestBuilder) -> Result<Value, GeaError> {
        let response = request
            .send()
            .await
            .map_err(|_| GeaError::bad_gateway("GEA_NETWORK_ERROR", "无法连接 GEA 服务"))?;
        let status = response.status();
        let retry_after_ms = parse_retry_after_ms(response.headers());
        let value = response
            .json::<Value>()
            .await
            .map_err(|_| super::invalid_upstream("GEA 返回了无效 JSON"))?;
        if !status.is_success() || value.get("success").and_then(Value::as_bool) != Some(true) {
            let mut error = sales_plan_upstream_error(&value, status.as_u16());
            error.body.retry_after_ms = retry_after_ms.or(error.body.retry_after_ms);
            return Err(error);
        }
        Ok(value)
    }
}

fn sanitize_service_token_error(error: GeaError) -> GeaError {
    let code = match error.body.code.as_str() {
        "invalid_client" => "invalid_client",
        "temporarily_unavailable" => "temporarily_unavailable",
        "rate_limited" => "rate_limited",
        _ => "GEA_SERVICE_TOKEN_ERROR",
    };
    // Rebuild the error instead of mutating only its public body: Display also
    // stores the original message, and diagnostic IDs may echo credentials.
    let mut sanitized = GeaError::new(error.status, code, "GEA 服务身份认证失败");
    sanitized.body.retry_after_ms = error.body.retry_after_ms;
    sanitized
}

fn extract_result<T: DeserializeOwned>(value: Value) -> Result<T, GeaError> {
    let result = value
        .get("result")
        .cloned()
        .ok_or_else(|| super::invalid_upstream("GEA 响应缺少 result"))?;
    serde_json::from_value(result).map_err(|_| super::invalid_upstream("GEA result 契约不兼容"))
}

fn upstream_json<T: serde::Serialize>(request: &T) -> Result<Value, GeaError> {
    let mut value = serde_json::to_value(request).map_err(|_| GeaError::invalid_request("销售计划参数无效"))?;
    convert_decimal_fields(&mut value)?;
    Ok(value)
}

fn convert_decimal_fields(value: &mut Value) -> Result<(), GeaError> {
    match value {
        Value::Object(fields) => {
            for (name, value) in fields {
                if matches!(
                    name.as_str(),
                    "adjustQty" | "targetQty" | "targetAmount" | "baseQty" | "qty" | "price"
                ) && let Value::String(decimal) = value
                {
                    *value = Value::Number(
                        serde_json::Number::from_str(decimal)
                            .map_err(|_| GeaError::invalid_request(format!("{name} 格式无效")))?,
                    );
                } else if matches!(name.as_str(), "periodId" | "dealerCode" | "skuCode")
                    && let Value::String(identifier) = value
                {
                    validate_positive_id(&GeaSalesPlanId(identifier.clone()), name)?;
                    *value = Value::Number(
                        serde_json::Number::from_str(identifier)
                            .map_err(|_| GeaError::invalid_request(format!("{name} 格式无效")))?,
                    );
                } else {
                    convert_decimal_fields(value)?;
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                convert_decimal_fields(item)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn sales_plan_upstream_error(value: &Value, fallback_status: u16) -> GeaError {
    let code = value
        .get("errorCode")
        .or_else(|| value.get("code").filter(|code| code.is_string()))
        .and_then(value_as_string)
        .unwrap_or_else(|| "GEA_UPSTREAM_ERROR".to_owned());
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .and_then(non_empty)
        .unwrap_or_else(|| "GEA 请求未完成".to_owned());
    let category = value.get("category").and_then(Value::as_str).and_then(non_empty);
    let status = if (200..300).contains(&fallback_status) {
        super::status_for_category(category.as_deref())
    } else {
        fallback_status
    };
    let mut error = GeaError::from_http_status(status, code, message);
    if let Some(category) = category {
        error.body.category = category;
    }
    error.body.retryable = value
        .get("retryable")
        .and_then(Value::as_bool)
        .unwrap_or(error.body.retryable);
    error.body.retry_after_ms = value.get("retryAfterMs").and_then(Value::as_u64);
    error.body.request_id = value.get("requestId").and_then(Value::as_str).and_then(non_empty);
    error.body.trace_id = value.get("traceId").and_then(Value::as_str).and_then(non_empty);
    error.body.audit_id = value.get("auditId").and_then(Value::as_str).and_then(non_empty);
    error.body.details = value.get("details").cloned();
    error
}

fn validate_submit(request: &GeaSalesPlanSubmitRequest) -> Result<(), GeaError> {
    if request.items.is_empty() || request.items.len() > 5000 {
        return Err(GeaError::invalid_request("销售计划提交主键或明细数量无效"));
    }
    validate_positive_id(&request.period_id, "periodId")?;
    validate_positive_id(&request.dealer_code, "dealerCode")?;
    validate_month(&request.period_month)?;
    validate_text(&request.plan_type_code, 32, "planTypeCode")?;
    if request.channel_code.is_empty()
        || request.channel_code.len() > 12
        || !request
            .channel_code
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        return Err(GeaError::invalid_request("channelCode 格式无效"));
    }
    validate_decimal(&request.target_qty.0, 15, 3, false, "targetQty")?;
    validate_decimal(&request.target_amount.0, 16, 2, false, "targetAmount")?;
    validate_text(&request.submitter_code, 64, "submitterCode")?;
    validate_optional_text(request.org_code.as_deref(), 64, "orgCode")?;
    validate_optional_text(request.province_code.as_deref(), 64, "provinceCode")?;
    validate_optional_text(request.area_code.as_deref(), 64, "areaCode")?;
    validate_optional_text(request.base_name.as_deref(), 64, "baseName")?;
    validate_optional_text(request.submitter_name.as_deref(), 128, "submitterName")?;
    let mut sku_codes = HashSet::with_capacity(request.items.len());
    for item in &request.items {
        validate_positive_id(&item.sku_code, "skuCode")?;
        if !sku_codes.insert(item.sku_code.0.as_str()) {
            return Err(GeaError::invalid_request("items 内 skuCode 不能重复"));
        }
        validate_text(&item.product_categ_name, 128, "productCategName")?;
        validate_decimal(&item.base_qty.0, 15, 3, false, "baseQty")?;
        validate_decimal(&item.qty.0, 15, 3, false, "qty")?;
        validate_decimal(&item.price.0, 14, 4, false, "price")?;
    }
    Ok(())
}

fn validate_positive_id(value: &GeaSalesPlanId, field: &str) -> Result<(), GeaError> {
    if value.0.parse::<i64>().ok().is_none_or(|value| value <= 0) {
        return Err(GeaError::invalid_request(format!("{field} 必须为正 Long 整数")));
    }
    Ok(())
}

fn validate_optional_text(value: Option<&str>, max: usize, field: &str) -> Result<(), GeaError> {
    if value.is_some_and(|value| value.chars().count() > max) {
        return Err(GeaError::invalid_request(format!("{field} 最长 {max} 个字符")));
    }
    Ok(())
}

fn validate_decimal(
    value: &str,
    max_integer: usize,
    max_fraction: usize,
    signed: bool,
    field: &str,
) -> Result<(), GeaError> {
    let value = value.trim();
    let unsigned = if let Some(rest) = value.strip_prefix('-') {
        if !signed {
            return Err(GeaError::invalid_request(format!("{field} 不得为负数")));
        }
        rest
    } else if let Some(rest) = value.strip_prefix('+') {
        rest
    } else {
        value
    };
    let mut parts = unsigned.split('.');
    let integer = parts.next().unwrap_or_default();
    let fraction = parts.next();
    if integer.is_empty()
        || !integer.bytes().all(|byte| byte.is_ascii_digit())
        || integer.trim_start_matches('0').len().max(1) > max_integer
        || fraction.is_some_and(|part| {
            part.is_empty() || part.len() > max_fraction || !part.bytes().all(|byte| byte.is_ascii_digit())
        })
        || parts.next().is_some()
    {
        return Err(GeaError::invalid_request(format!("{field} 精度或格式无效")));
    }
    Ok(())
}

fn validate_month(value: &str) -> Result<(), GeaError> {
    let bytes = value.as_bytes();
    if bytes.len() != 7
        || bytes[4] != b'-'
        || !bytes[..4].iter().all(u8::is_ascii_digit)
        || !bytes[5..].iter().all(u8::is_ascii_digit)
        || !matches!(
            &value[5..],
            "01" | "02" | "03" | "04" | "05" | "06" | "07" | "08" | "09" | "10" | "11" | "12"
        )
    {
        return Err(GeaError::invalid_request("periodMonth 格式必须为 YYYY-MM"));
    }
    Ok(())
}

fn validate_text(value: &str, max: usize, field: &str) -> Result<(), GeaError> {
    if value.trim().is_empty() || value.chars().count() > max {
        return Err(GeaError::invalid_request(format!(
            "{field} 不能为空且最长 {max} 个字符"
        )));
    }
    Ok(())
}

fn validate_command_headers(idempotency_key: &str, request_id: &str, trace_id: Option<&str>) -> Result<(), GeaError> {
    require_header_value(idempotency_key, "Idempotency-Key")?;
    require_header_value(request_id, "X-Request-Id")?;
    if let Some(trace_id) = trace_id {
        require_header_value(trace_id, "X-Trace-Id")?;
    }
    Ok(())
}

fn require_header_value<'a>(value: &'a str, field: &str) -> Result<&'a str, GeaError> {
    non_empty_ref(value).ok_or_else(|| GeaError::invalid_request(format!("{field} 不能为空")))
}

fn non_empty_ref(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

fn insert_header(headers: &mut HeaderMap, name: &'static str, value: &str, field: &str) -> Result<(), GeaError> {
    headers.insert(
        name,
        HeaderValue::from_str(require_header_value(value, field)?)
            .map_err(|_| GeaError::invalid_request(format!("{field} 格式无效")))?,
    );
    Ok(())
}

fn should_retry_submit(error: &GeaError, attempt: usize) -> bool {
    attempt < MAX_SUBMIT_ATTEMPTS
        && (error.status == StatusCode::TOO_MANY_REQUESTS
            || error.status.is_server_error()
            || error.body.code == "GEA_NETWORK_ERROR")
}

fn submit_retry_delay(error: &GeaError, attempt: usize) -> Option<Duration> {
    if let Some(retry_after_ms) = error.body.retry_after_ms {
        let delay = Duration::from_millis(retry_after_ms);
        // Return the upstream error when its minimum wait exceeds our budget;
        // shortening Retry-After would submit before the server permits it.
        return (delay <= MAX_SUBMIT_RETRY_DELAY).then_some(delay);
    }
    let exponent = attempt.saturating_sub(1).min(3) as u32;
    Some(Duration::from_millis(250_u64.saturating_mul(2_u64.pow(exponent))))
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::{Duration, Instant};

    use aionui_api_types::{GeaSalesPlanDecimal, GeaSalesPlanId, GeaSalesPlanSubmitItem, GeaSalesPlanSubmitRequest};
    use serde_json::json;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{
        SalesPlanServiceIdentity, SalesPlanTokenCache, sales_plan_upstream_error, validate_decimal, validate_month,
        validate_submit,
    };
    use crate::service::GeaService;

    #[test]
    fn decimal_validation_preserves_contract_precision() {
        assert!(validate_decimal("-123.456", 15, 3, true, "adjustQty").is_ok());
        assert!(validate_decimal("-1", 15, 3, false, "qty").is_err());
        assert!(validate_decimal("1.2345", 15, 3, true, "adjustQty").is_err());
    }

    #[test]
    fn month_validation_matches_upstream_pattern() {
        assert!(validate_month("2026-08").is_ok());
        assert!(validate_month("2026-13").is_err());
    }

    #[test]
    fn aioncore_service_still_reads_sales_plan_identity_from_its_own_environment() {
        const CHILD_MARKER: &str = "AIONUI_GEA_SALES_PLAN_IDENTITY_TEST_CHILD";
        if std::env::var_os(CHILD_MARKER).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg(
                    "service::sales_plan_submit_service::tests::aioncore_service_still_reads_sales_plan_identity_from_its_own_environment",
                )
                .arg("--nocapture")
                .env(CHILD_MARKER, "1")
                .env("AIONUI_GEA_BASE_URL", "https://gea.example")
                .env("AIONUI_GEA_SALES_PLAN_CLIENT_ID", "sales-plan-client")
                .env("AIONUI_GEA_SALES_PLAN_CLIENT_SECRET", "sales-plan-secret")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "child test failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let service = GeaService::from_env().unwrap();
        let identity = service
            .sales_plan_service_identity
            .as_ref()
            .expect("AionCore must retain its own service identity");
        assert_eq!(identity.client_id, "sales-plan-client");
        assert_eq!(identity.client_secret, "sales-plan-secret");
    }

    #[test]
    fn upstream_error_prefers_business_error_code_over_numeric_envelope_code() {
        let error = sales_plan_upstream_error(
            &json!({
                "success": false,
                "code": 409,
                "errorCode": "PLAN_STATUS_CONFLICT",
                "message": "状态已变化",
                "category": "CONFLICT",
                "retryable": false,
                "requestId": "request-1",
                "traceId": "trace-1",
                "auditId": "audit-1"
            }),
            409,
        );
        assert_eq!(error.status, axum::http::StatusCode::CONFLICT);
        assert_eq!(error.body.code, "PLAN_STATUS_CONFLICT");
        assert_eq!(error.body.request_id.as_deref(), Some("request-1"));
        assert!(!error.body.retryable);
    }

    #[test]
    fn command_validation_preserves_independent_targets_and_rejects_invalid_items() {
        let mut duplicate_submit = submit_request();
        duplicate_submit.items.push(duplicate_submit.items[0].clone());
        assert!(
            validate_submit(&duplicate_submit)
                .unwrap_err()
                .to_string()
                .contains("不能重复")
        );

        let mut independent_targets = submit_request();
        independent_targets.target_qty = GeaSalesPlanDecimal("26445.000".to_owned());
        independent_targets.target_amount = GeaSalesPlanDecimal("1539999.99".to_owned());
        assert!(validate_submit(&independent_targets).is_ok());
        let wire = super::upstream_json(&independent_targets).unwrap();
        assert_eq!(wire["targetQty"].to_string(), "26445.000");
        assert_eq!(wire["targetAmount"].to_string(), "1539999.99");

        let mut long_optional = submit_request();
        long_optional.org_code = Some("x".repeat(65));
        assert!(
            validate_submit(&long_optional)
                .unwrap_err()
                .to_string()
                .contains("orgCode")
        );
    }

    #[tokio::test]
    async fn service_submit_uses_form_credentials_and_reuses_only_memory_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/internal/auth/token"))
            .and(header("content-type", "application/x-www-form-urlencoded"))
            .and(body_string_contains("grant_type=client_credentials"))
            .and(body_string_contains("client_id=client-1"))
            .and(body_string_contains("client_secret=secret-1"))
            .and(body_string_contains("scope=sales-plan%3Awrite"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "code": 200,
                "result": {
                    "access_token": "memory-only-token",
                    "token_type": "Bearer",
                    "expires_in": 600,
                    "scope": "sales-plan:write"
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/internal/sales-plans"))
            .and(header("authorization", "Bearer memory-only-token"))
            .and(header("idempotency-key", "submit-key-1"))
            .and(header("x-request-id", "request-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "code": 200,
                "result": {
                    "planId": "plan-1",
                    "versionId": "version-1",
                    "seq": 1,
                    "status": 1,
                    "replayed": false,
                    "requestId": "request-1",
                    "traceId": "request-1",
                    "auditId": "audit-1"
                }
            })))
            .expect(2)
            .mount(&server)
            .await;

        let mut service = GeaService::new(reqwest::Client::new(), server.uri()).unwrap();
        service.sales_plan_service_identity = Some(SalesPlanServiceIdentity::for_test("client-1", "secret-1"));
        let receipt = service
            .sales_plan_submit("submit-key-1", "request-1", &submit_request())
            .await
            .unwrap();
        let replay = service
            .sales_plan_submit("submit-key-1", "request-1", &submit_request())
            .await
            .unwrap();
        let serialized = serde_json::to_string(&receipt).unwrap();
        assert_eq!(receipt.plan_id, "plan-1");
        assert_eq!(replay.plan_id, "plan-1");
        assert!(!serialized.contains("memory-only-token"));
        assert!(!serialized.contains("secret-1"));
    }

    #[tokio::test]
    async fn service_submit_is_unavailable_without_process_credentials() {
        let server = MockServer::start().await;
        let service = GeaService::new(reqwest::Client::new(), server.uri()).unwrap();
        let error = service
            .sales_plan_submit("submit-key-1", "request-1", &submit_request())
            .await
            .unwrap_err();
        assert_eq!(error.body.code, "GEA_SALES_PLAN_SERVICE_UNAVAILABLE");
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn service_token_never_forwards_form_credentials_on_307_or_308() {
        for status in [307, 308] {
            let source = MockServer::start().await;
            let destination = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/redirect-target"))
                .respond_with(submit_ok())
                .expect(0)
                .mount(&destination)
                .await;
            Mock::given(method("POST"))
                .and(path("/api/v1/internal/auth/token"))
                .and(body_string_contains("client_secret=secret-1"))
                .respond_with(
                    ResponseTemplate::new(status)
                        .insert_header("Location", format!("{}/redirect-target", destination.uri())),
                )
                .expect(1)
                .mount(&source)
                .await;
            let service = service_identity(&source);

            let error = service
                .sales_plan_submit("submit-key-1", "request-1", &submit_request())
                .await
                .unwrap_err();

            assert_eq!(error.body.code, "GEA_SERVICE_TOKEN_ERROR");
            assert!(destination.received_requests().await.unwrap().is_empty());
            assert!(submit_requests(&source).await.is_empty());
        }
    }

    #[tokio::test]
    async fn service_submit_never_follows_307_or_308_with_bearer_or_write_body() {
        for status in [307, 308] {
            let source = MockServer::start().await;
            let destination = MockServer::start().await;
            mount_token(&source, "memory-only-token").await;
            Mock::given(method("POST"))
                .and(path("/redirect-target"))
                .respond_with(submit_ok())
                .expect(0)
                .mount(&destination)
                .await;
            Mock::given(method("POST"))
                .and(path("/api/v1/internal/sales-plans"))
                .and(header("authorization", "Bearer memory-only-token"))
                .respond_with(
                    ResponseTemplate::new(status)
                        .insert_header("Location", format!("{}/redirect-target", destination.uri()))
                        .set_body_json(json!({"success": false, "message": "redirect"})),
                )
                .expect(1)
                .mount(&source)
                .await;
            let service = service_identity(&source);

            service
                .sales_plan_submit("submit-key-1", "request-1", &submit_request())
                .await
                .unwrap_err();

            assert!(destination.received_requests().await.unwrap().is_empty());
            assert_eq!(submit_requests(&source).await.len(), 1);
        }
    }

    #[tokio::test]
    async fn unrepresentable_service_token_expiry_returns_error_without_caching_or_submitting() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/internal/auth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": {
                    "access_token": "must-not-be-cached", "token_type": "Bearer",
                    "expires_in": u64::MAX, "scope": "sales-plan:write"
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        let service = service_identity(&server);

        let error = service
            .sales_plan_submit("submit-key-1", "request-1", &submit_request())
            .await
            .unwrap_err();

        assert_eq!(error.body.code, "GEA_INVALID_RESPONSE");
        assert!(service.sales_plan_token_cache.lock().await.is_none());
        assert!(submit_requests(&server).await.is_empty());
        assert!(!error.to_string().contains("must-not-be-cached"));
    }

    #[test]
    fn submit_retry_never_shortens_the_upstream_minimum_wait() {
        let mut error =
            crate::error::GeaError::new(axum::http::StatusCode::TOO_MANY_REQUESTS, "rate_limited", "Retry later");
        for retry_after_ms in [5_001, 3_600_000, u64::MAX] {
            error.body.retry_after_ms = Some(retry_after_ms);
            assert_eq!(super::submit_retry_delay(&error, 1), None);
            assert_eq!(super::submit_retry_delay(&error, 2), None);
        }
        error.body.retry_after_ms = Some(5_000);
        assert_eq!(super::submit_retry_delay(&error, 1), Some(Duration::from_secs(5)));
        error.body.retry_after_ms = Some(1_000);
        assert_eq!(super::submit_retry_delay(&error, 1), Some(Duration::from_secs(1)));
        error.body.retry_after_ms = None;
        assert_eq!(super::submit_retry_delay(&error, 1), Some(Duration::from_millis(250)));
        assert_eq!(super::submit_retry_delay(&error, 2), Some(Duration::from_millis(500)));
    }

    #[tokio::test]
    async fn submit_returns_long_retry_after_without_repeating_the_write() {
        let server = MockServer::start().await;
        mount_token(&server, "service-token").await;
        Mock::given(method("POST"))
            .and(path("/api/v1/internal/sales-plans"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "60")
                    .set_body_json(json!({
                        "success": false, "errorCode": "rate_limited", "retryable": true
                    })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let service = service_identity(&server);
        let error = tokio::time::timeout(
            Duration::from_secs(2),
            service.sales_plan_submit("submit-key-1", "request-1", &submit_request()),
        )
        .await
        .expect("long Retry-After must return without sleeping")
        .unwrap_err();
        assert_eq!(error.body.retry_after_ms, Some(60_000));
        assert_eq!(submit_requests(&server).await.len(), 1);
    }

    #[tokio::test]
    async fn token_failures_never_echo_service_credentials_or_upstream_details() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/internal/auth/token"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "success": false,
                "code": 401,
                "errorCode": "invalid_client",
                "message": "secret-1",
                "category": "secret-1",
                "retryable": false,
                "retryAfterMs": 500,
                "requestId": "secret-1",
                "traceId": "secret-1",
                "auditId": "secret-1",
                "details": {"clientSecret": "secret-1"}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let mut service = GeaService::new(reqwest::Client::new(), server.uri()).unwrap();
        service.sales_plan_service_identity = Some(SalesPlanServiceIdentity::for_test("client-1", "secret-1"));

        let error = service
            .sales_plan_submit("submit-key-1", "request-1", &submit_request())
            .await
            .unwrap_err();
        let serialized = serde_json::to_string(&error.body).unwrap();
        assert_eq!(error.body.code, "invalid_client");
        assert_eq!(error.body.message, "GEA 服务身份认证失败");
        assert_eq!(error.body.category, "AUTHENTICATION");
        assert_eq!(error.body.retry_after_ms, Some(500));
        assert!(error.body.request_id.is_none());
        assert!(error.body.trace_id.is_none());
        assert!(error.body.audit_id.is_none());
        assert!(error.body.details.is_none());
        assert!(!serialized.contains("secret-1"));
        assert!(!error.to_string().contains("secret-1"));
        assert!(!format!("{error:?}").contains("secret-1"));
    }

    #[test]
    fn token_error_sanitization_drops_unknown_codes_that_echo_secrets() {
        let error = crate::error::GeaError::bad_gateway("secret-1", "secret-1");
        let sanitized = super::sanitize_service_token_error(error);
        assert_eq!(sanitized.body.code, "GEA_SERVICE_TOKEN_ERROR");
        assert_eq!(sanitized.body.category, "UPSTREAM");
        assert!(!format!("{sanitized:?}").contains("secret-1"));
    }

    #[tokio::test]
    async fn submit_retries_429_with_the_identical_body_and_idempotency_headers() {
        let server = MockServer::start().await;
        mount_token(&server, "memory-only-token").await;
        let calls = Arc::new(AtomicUsize::new(0));
        let responder_calls = calls.clone();
        Mock::given(method("POST"))
            .and(path("/api/v1/internal/sales-plans"))
            .and(header("authorization", "Bearer memory-only-token"))
            .and(header("idempotency-key", "submit-key-1"))
            .and(header("x-request-id", "request-1"))
            .respond_with(move |_: &wiremock::Request| {
                if responder_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    ResponseTemplate::new(429)
                        .insert_header("Retry-After", "0")
                        .set_body_json(json!({
                            "success": false, "code": 429, "errorCode": "rate_limited",
                            "message": "稍后重试", "category": "RATE_LIMIT", "retryable": true
                        }))
                } else {
                    submit_ok()
                }
            })
            .expect(2)
            .mount(&server)
            .await;
        let service = service_identity(&server);

        service
            .sales_plan_submit("submit-key-1", "request-1", &submit_request())
            .await
            .unwrap();

        let requests = submit_requests(&server).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].body, requests[1].body);
        assert_eq!(
            requests[0].headers.get("idempotency-key"),
            requests[1].headers.get("idempotency-key")
        );
        assert_eq!(
            requests[0].headers.get("x-request-id"),
            requests[1].headers.get("x-request-id")
        );
    }

    #[tokio::test]
    async fn submit_stops_after_three_retryable_upstream_failures() {
        let server = MockServer::start().await;
        mount_token(&server, "memory-only-token").await;
        Mock::given(method("POST"))
            .and(path("/api/v1/internal/sales-plans"))
            .respond_with(ResponseTemplate::new(503).set_body_json(json!({
                "success": false, "code": 503, "errorCode": "temporarily_unavailable",
                "message": "暂时不可用", "category": "UPSTREAM", "retryable": true
            })))
            .expect(3)
            .mount(&server)
            .await;
        let service = service_identity(&server);

        let error = service
            .sales_plan_submit("submit-key-1", "request-1", &submit_request())
            .await
            .unwrap_err();

        assert_eq!(error.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(submit_requests(&server).await.len(), 3);
    }

    #[tokio::test]
    async fn submit_refreshes_after_401_only_once_and_never_exceeds_three_attempts() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/internal/auth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true, "code": 200,
                "result": {"access_token": "token", "token_type": "Bearer", "expires_in": 600, "scope": "sales-plan:write"}
            })))
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/internal/sales-plans"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "success": false, "code": 401, "errorCode": "invalid_token",
                "message": "Token 无效", "category": "AUTHENTICATION", "retryable": false
            })))
            .expect(2)
            .mount(&server)
            .await;
        let service = service_identity(&server);

        let error = service
            .sales_plan_submit("submit-key-1", "request-1", &submit_request())
            .await
            .unwrap_err();

        assert_eq!(error.status, axum::http::StatusCode::UNAUTHORIZED);
        assert_eq!(submit_requests(&server).await.len(), 2);
    }

    #[tokio::test]
    async fn submit_does_not_exceed_three_posts_when_401_follows_two_retryable_failures() {
        let server = MockServer::start().await;
        mount_token(&server, "token").await;
        let calls = Arc::new(AtomicUsize::new(0));
        let responder_calls = calls.clone();
        Mock::given(method("POST"))
            .and(path("/api/v1/internal/sales-plans"))
            .respond_with(move |_: &wiremock::Request| {
                if responder_calls.fetch_add(1, Ordering::SeqCst) < 2 {
                    ResponseTemplate::new(503).set_body_json(json!({
                        "success": false, "code": 503, "errorCode": "temporarily_unavailable",
                        "message": "暂时不可用", "category": "UPSTREAM", "retryable": true
                    }))
                } else {
                    ResponseTemplate::new(401).set_body_json(json!({
                        "success": false, "code": 401, "errorCode": "invalid_token",
                        "message": "Token 无效", "category": "AUTHENTICATION", "retryable": false
                    }))
                }
            })
            .expect(3)
            .mount(&server)
            .await;
        let service = service_identity(&server);

        let error = service
            .sales_plan_submit("submit-key-1", "request-1", &submit_request())
            .await
            .unwrap_err();

        assert_eq!(error.status, axum::http::StatusCode::UNAUTHORIZED);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(submit_requests(&server).await.len(), 3);
        let token_requests = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|request| request.url.path() == "/api/v1/internal/auth/token")
            .count();
        assert_eq!(
            token_requests, 1,
            "the exhausted request budget must not refresh the token"
        );
    }

    #[tokio::test]
    async fn submit_retries_real_network_disconnects_but_stops_after_three_posts() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let accepted_by_server = accepted.clone();
        let disconnect_server = tokio::spawn(async move {
            for _ in 0..3 {
                let (stream, _) = listener.accept().await.unwrap();
                accepted_by_server.fetch_add(1, Ordering::SeqCst);
                drop(stream);
            }
        });
        let mut service = GeaService::new(reqwest::Client::new(), format!("http://{address}")).unwrap();
        service.sales_plan_service_identity = Some(SalesPlanServiceIdentity::for_test("client-1", "secret-1"));
        *service.sales_plan_token_cache.lock().await = Some(SalesPlanTokenCache {
            access_token: "memory-only-token".to_owned(),
            expires_at: Instant::now() + Duration::from_secs(600),
        });

        let error = service
            .sales_plan_submit("submit-key-1", "request-1", &submit_request())
            .await
            .unwrap_err();
        tokio::time::timeout(Duration::from_secs(2), disconnect_server)
            .await
            .expect("three disconnected requests must reach the listener")
            .unwrap();

        assert_eq!(error.body.code, "GEA_NETWORK_ERROR");
        assert_eq!(accepted.load(Ordering::SeqCst), 3);
    }

    fn submit_request() -> GeaSalesPlanSubmitRequest {
        GeaSalesPlanSubmitRequest {
            period_id: GeaSalesPlanId("202608".to_owned()),
            period_month: "2026-08".to_owned(),
            plan_type_code: "monthly".to_owned(),
            channel_code: "gea".to_owned(),
            dealer_code: GeaSalesPlanId("1001".to_owned()),
            org_code: Some("org-1".to_owned()),
            province_code: None,
            area_code: None,
            base_name: None,
            target_qty: GeaSalesPlanDecimal("2.000".to_owned()),
            target_amount: GeaSalesPlanDecimal("20.00".to_owned()),
            submitter_code: "aioncore".to_owned(),
            submitter_name: Some("AionCore".to_owned()),
            items: vec![GeaSalesPlanSubmitItem {
                sku_code: GeaSalesPlanId("42".to_owned()),
                product_categ_name: "产品".to_owned(),
                base_qty: GeaSalesPlanDecimal("1.000".to_owned()),
                qty: GeaSalesPlanDecimal("2.000".to_owned()),
                price: GeaSalesPlanDecimal("10.0000".to_owned()),
            }],
        }
    }

    fn service_identity(server: &MockServer) -> GeaService {
        let mut service = GeaService::new(reqwest::Client::new(), server.uri()).unwrap();
        service.sales_plan_service_identity = Some(SalesPlanServiceIdentity::for_test("client-1", "secret-1"));
        service
    }

    async fn mount_token(server: &MockServer, token: &'static str) {
        Mock::given(method("POST"))
            .and(path("/api/v1/internal/auth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true, "code": 200,
                "result": {"access_token": token, "token_type": "Bearer", "expires_in": 600, "scope": "sales-plan:write"}
            })))
            .expect(1)
            .mount(server)
            .await;
    }

    async fn submit_requests(server: &MockServer) -> Vec<wiremock::Request> {
        server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|request| request.url.path() == "/api/v1/internal/sales-plans")
            .collect()
    }

    fn submit_ok() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "success": true, "code": 200,
            "result": {
                "planId": "plan-1", "versionId": "version-1", "seq": 1, "status": 1,
                "replayed": false, "requestId": "request-1", "traceId": "request-1", "auditId": "audit-1"
            }
        }))
    }
}

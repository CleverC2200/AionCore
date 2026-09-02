use axum::http::HeaderValue;
use serde_json::Value;

use super::{GeaService, ensure_success, invalid_upstream, parse_retry_after_ms, upstream_business_error};
use crate::error::GeaError;

impl GeaService {
    pub async fn list_sales_plan_periods(&self, user_id: &str, raw_query: Option<&str>) -> Result<Value, GeaError> {
        self.get_sales_plan_result(user_id, "/sales-plan/periods", raw_query)
            .await
    }

    pub async fn list_sales_plans(&self, user_id: &str, raw_query: Option<&str>) -> Result<Value, GeaError> {
        self.get_sales_plan_result(user_id, "/sales-plan/plans", raw_query)
            .await
    }

    pub async fn get_sales_plan(&self, user_id: &str, plan_id: &str) -> Result<Value, GeaError> {
        self.get_sales_plan_result(
            user_id,
            &format!("/sales-plan/plans/{}", super::encode_path_segment(plan_id)),
            None,
        )
        .await
    }

    pub async fn list_sales_plan_versions(&self, user_id: &str, plan_id: &str) -> Result<Value, GeaError> {
        self.get_sales_plan_result(
            user_id,
            &format!("/sales-plan/plans/{}/versions", super::encode_path_segment(plan_id)),
            None,
        )
        .await
    }

    pub async fn list_sales_plan_logs(&self, user_id: &str, plan_id: &str) -> Result<Value, GeaError> {
        self.get_sales_plan_result(
            user_id,
            &format!("/sales-plan/plans/{}/logs", super::encode_path_segment(plan_id)),
            None,
        )
        .await
    }

    pub async fn list_sales_plan_version_skus(&self, user_id: &str, version_id: &str) -> Result<Value, GeaError> {
        self.get_sales_plan_result(
            user_id,
            &format!(
                "/sales-plan/plans/versions/{}/skus",
                super::encode_path_segment(version_id)
            ),
            None,
        )
        .await
    }

    pub async fn compare_sales_plan_versions(
        &self,
        user_id: &str,
        plan_id: &str,
        raw_query: Option<&str>,
    ) -> Result<Value, GeaError> {
        self.get_sales_plan_result(
            user_id,
            &format!("/sales-plan/plans/{}/compare", super::encode_path_segment(plan_id)),
            raw_query,
        )
        .await
    }

    pub async fn act_on_sales_plan_version(
        &self,
        user_id: &str,
        version_id: &str,
        idempotency_key: &str,
        request_id: &str,
        body: &Value,
    ) -> Result<Value, GeaError> {
        let credential = self.sales_plan_credential(user_id).await?;
        let mut headers = self.user_headers(&credential)?;
        headers.insert(
            "idempotency-key",
            HeaderValue::from_str(idempotency_key)
                .map_err(|_| GeaError::invalid_request("Idempotency-Key 格式无效"))?,
        );
        headers.insert(
            "x-request-id",
            HeaderValue::from_str(request_id).map_err(|_| GeaError::invalid_request("X-Request-Id 格式无效"))?,
        );
        let path = format!(
            "/sales-plan/plans/versions/{}/actions",
            super::encode_path_segment(version_id)
        );
        let result = async {
            let response = self
                .client
                .post(format!("{}{}", self.base_url, path))
                .headers(headers)
                .json(body)
                .send()
                .await
                .map_err(|_| GeaError::bad_gateway("GEA_NETWORK_ERROR", "无法连接 GEA 服务"))?;
            let status = response.status();
            let retry_after_ms = parse_retry_after_ms(response.headers());
            let value = response
                .json::<Value>()
                .await
                .map_err(|_| invalid_upstream("GEA 返回了无效 JSON"))?;
            if !status.is_success() {
                let mut error = upstream_business_error(&value, status.as_u16());
                error.body.retry_after_ms = retry_after_ms;
                return Err(error);
            }
            ensure_success(&value)?;
            sales_plan_result(value)
        }
        .await;
        if matches!(&result, Err(error) if error.is_unauthorized()) {
            self.invalidate_auth_session(user_id).await;
        }
        result
    }

    async fn get_sales_plan_result(
        &self,
        user_id: &str,
        path: &str,
        raw_query: Option<&str>,
    ) -> Result<Value, GeaError> {
        let credential = self.sales_plan_credential(user_id).await?;
        let path = match raw_query.filter(|query| !query.is_empty()) {
            Some(query) => format!("{path}?{query}"),
            None => path.to_owned(),
        };
        let value = self
            .get_for_user_path(user_id, &credential, &path, &uuid::Uuid::now_v7().to_string())
            .await?;
        sales_plan_result(value)
    }

    async fn sales_plan_credential(&self, user_id: &str) -> Result<super::GeaCredential, GeaError> {
        let credential = self.credential(user_id).await?;
        if credential
            .tenant_id
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
        {
            return Err(GeaError::invalid_request("销售计划接口要求有效的 GEA tenantId"));
        }
        Ok(credential)
    }
}

fn sales_plan_result(value: Value) -> Result<Value, GeaError> {
    value
        .get("result")
        .cloned()
        .ok_or_else(|| invalid_upstream("GEA 销售计划响应缺少 result"))
}

use serde_json::{Value, json};

const MODEL_RESULT_MAX_BYTES: usize = 8_000;

pub(super) enum BusinessDataAction<'a> {
    Inspect { model: Option<&'a str> },
    Query,
    Other,
}

#[derive(Debug)]
pub(super) struct ProjectionError {
    pub code: &'static str,
    pub details: Option<Value>,
}

pub(super) fn project_business_data_result(
    action: BusinessDataAction<'_>,
    result: Value,
) -> Result<Value, ProjectionError> {
    let parsed = match &result {
        Value::String(value) => serde_json::from_str::<Value>(value).ok(),
        Value::Object(_) => Some(result.clone()),
        _ => None,
    };
    let Some(parsed) = parsed else {
        return Ok(result);
    };

    match action {
        BusinessDataAction::Inspect { model } => project_inspect_result(parsed, model),
        BusinessDataAction::Query => project_query_result(parsed),
        BusinessDataAction::Other => Ok(result),
    }
}

fn project_inspect_result(parsed: Value, requested_model: Option<&str>) -> Result<Value, ProjectionError> {
    let Some(models) = parsed.get("semanticModel").and_then(Value::as_array) else {
        return Ok(parsed);
    };
    let status = parsed
        .get("status")
        .cloned()
        .unwrap_or_else(|| Value::String("completed".to_owned()));

    let projected = if let Some(requested_model) = requested_model {
        let Some(model) = models
            .iter()
            .find(|model| model.get("name").and_then(Value::as_str) == Some(requested_model))
        else {
            let available_models = models
                .iter()
                .filter_map(|model| model.get("name").and_then(Value::as_str))
                .collect::<Vec<_>>();
            return Err(ProjectionError {
                code: "GEA_MCP_SEMANTIC_MODEL_NOT_FOUND",
                details: Some(json!({ "availableModels": available_models })),
            });
        };
        json!({
            "status": status,
            "action": "inspect",
            "semanticModel": [model]
        })
    } else {
        let catalog = models
            .iter()
            .filter_map(|model| {
                let name = model.get("name")?.as_str()?;
                Some(json!({
                    "name": name,
                    "title": model.get("title").cloned().unwrap_or(Value::Null),
                    "description": model.get("description").cloned().unwrap_or(Value::Null),
                    "measureCount": model.get("measures").and_then(Value::as_array).map_or(0, Vec::len),
                    "dimensionCount": model.get("dimensions").and_then(Value::as_array).map_or(0, Vec::len),
                    "segmentCount": model.get("segments").and_then(Value::as_array).map_or(0, Vec::len)
                }))
            })
            .collect::<Vec<_>>();
        json!({
            "status": status,
            "action": "inspect",
            "semanticModel": catalog,
            "nextStep": "Call inspect again with model set to one exact catalog name before building a query."
        })
    };

    serialize(projected)
}

fn project_query_result(mut result: Value) -> Result<Value, ProjectionError> {
    let Some(object) = result.as_object_mut() else {
        return Ok(result);
    };
    object.insert(
        "resultGuidance".to_owned(),
        Value::String(
            "completeness=complete means previewRows contains every returned row. completeness=preview means previewRows is intentionally bounded; rowCount remains authoritative. For totals, trends, or risk analysis, submit aggregated Cube queries with measures and only the required low-cardinality dimensions."
                .to_owned(),
        ),
    );

    annotate_dataset_completeness(&mut result, usize::MAX);
    update_result_completeness(&mut result);
    if serialized_len(&result)? <= MODEL_RESULT_MAX_BYTES {
        return serialize(result);
    }

    for preview_limit in [5, 3, 1, 0] {
        annotate_dataset_completeness(&mut result, preview_limit);
        update_result_completeness(&mut result);
        if serialized_len(&result)? <= MODEL_RESULT_MAX_BYTES {
            return serialize(result);
        }
    }

    let datasets = result
        .get("datasets")
        .and_then(Value::as_array)
        .map(|datasets| {
            datasets
                .iter()
                .filter_map(|dataset| {
                    let dataset = dataset.as_object()?;
                    Some(json!({
                        "name": dataset.get("name").cloned().unwrap_or(Value::Null),
                        "rowCount": dataset.get("rowCount").cloned().unwrap_or(Value::Null),
                        "returnedRowCount": 0,
                        "previewRows": [],
                        "previewRowCount": 0,
                        "previewTruncated": true,
                        "completeness": "preview",
                        "hasMore": true
                    }))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    serialize(json!({
        "status": result.get("status").cloned().unwrap_or_else(|| Value::String("completed".to_owned())),
        "action": "query",
        "executedQueryCount": result.get("executedQueryCount").cloned().unwrap_or(Value::Null),
        "datasets": datasets,
        "completeness": "preview",
        "resultGuidance": result.get("resultGuidance").cloned().unwrap_or(Value::Null)
    }))
}

fn annotate_dataset_completeness(result: &mut Value, limit: usize) {
    let Some(datasets) = result.get_mut("datasets").and_then(Value::as_array_mut) else {
        return;
    };
    for dataset in datasets {
        let Some(dataset) = dataset.as_object_mut() else {
            continue;
        };
        let row_count = dataset.get("rowCount").and_then(Value::as_u64);
        let Some(rows) = dataset.get_mut("previewRows").and_then(Value::as_array_mut) else {
            continue;
        };
        rows.truncate(limit);
        let returned_row_count = rows.len();
        let has_more = row_count.is_some_and(|row_count| row_count > returned_row_count as u64);
        dataset.insert("returnedRowCount".to_owned(), json!(returned_row_count));
        dataset.insert("previewRowCount".to_owned(), json!(returned_row_count));
        dataset.insert("previewTruncated".to_owned(), json!(has_more));
        dataset.insert(
            "completeness".to_owned(),
            Value::String(if has_more { "preview" } else { "complete" }.to_owned()),
        );
        dataset.insert("hasMore".to_owned(), json!(has_more));
    }
}

fn update_result_completeness(result: &mut Value) {
    let is_preview = result
        .get("datasets")
        .and_then(Value::as_array)
        .is_some_and(|datasets| {
            datasets
                .iter()
                .any(|dataset| dataset.get("hasMore") == Some(&Value::Bool(true)))
        });
    if let Some(object) = result.as_object_mut() {
        object.insert(
            "completeness".to_owned(),
            Value::String(if is_preview { "preview" } else { "complete" }.to_owned()),
        );
    }
}

fn serialized_len(value: &Value) -> Result<usize, ProjectionError> {
    serde_json::to_string(value)
        .map(|value| value.len())
        .map_err(|_| invalid_response())
}

fn serialize(value: Value) -> Result<Value, ProjectionError> {
    serde_json::to_string(&value)
        .map(Value::String)
        .map_err(|_| invalid_response())
}

fn invalid_response() -> ProjectionError {
    ProjectionError {
        code: "GEA_MCP_BACKEND_RESPONSE_INVALID",
        details: None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{BusinessDataAction, MODEL_RESULT_MAX_BYTES, project_business_data_result};

    fn semantic_model_result() -> Value {
        Value::String(
            json!({
                "status": "completed",
                "action": "inspect",
                "semanticModel": [
                    {
                        "name": "sales",
                        "title": "Sales",
                        "description": "Sales model",
                        "measures": [{ "name": "sales.amount", "title": "Amount", "type": "number" }],
                        "dimensions": []
                    },
                    {
                        "name": "inventory_age_snapshot",
                        "title": "Inventory age snapshot",
                        "description": "Inventory model",
                        "measures": [{ "name": "inventory_age_snapshot.current_inventory", "type": "number" }],
                        "dimensions": [{ "name": "inventory_age_snapshot.snapshot_date", "type": "time" }]
                    },
                    {
                        "name": "safety_stock_snapshot",
                        "title": "Safety stock snapshot",
                        "description": "Safety stock model",
                        "measures": [],
                        "dimensions": []
                    }
                ]
            })
            .to_string(),
        )
    }

    fn parse_projected(result: Value) -> Value {
        serde_json::from_str(result.as_str().expect("string result")).expect("valid projected json")
    }

    #[test]
    fn inspect_without_model_returns_complete_catalog_index() {
        let projected =
            project_business_data_result(BusinessDataAction::Inspect { model: None }, semantic_model_result())
                .expect("project catalog");
        let catalog = parse_projected(projected);

        assert_eq!(catalog["semanticModel"].as_array().map(Vec::len), Some(3));
        assert_eq!(catalog["semanticModel"][1]["name"], "inventory_age_snapshot");
        assert_eq!(catalog["semanticModel"][1]["measureCount"], 1);
        assert_eq!(catalog["semanticModel"][1]["dimensionCount"], 1);
        assert!(catalog["semanticModel"][1].get("measures").is_none());
    }

    #[test]
    fn inspect_with_model_returns_only_requested_schema() {
        let projected = project_business_data_result(
            BusinessDataAction::Inspect {
                model: Some("inventory_age_snapshot"),
            },
            semantic_model_result(),
        )
        .expect("project selected model");
        let selected = parse_projected(projected);

        assert_eq!(selected["semanticModel"].as_array().map(Vec::len), Some(1));
        assert_eq!(selected["semanticModel"][0]["name"], "inventory_age_snapshot");
    }

    #[test]
    fn inspect_rejects_unknown_model_with_available_names() {
        let error = project_business_data_result(
            BusinessDataAction::Inspect { model: Some("missing") },
            semantic_model_result(),
        )
        .expect_err("unknown model must fail");

        assert_eq!(error.code, "GEA_MCP_SEMANTIC_MODEL_NOT_FOUND");
        assert_eq!(
            error
                .details
                .as_ref()
                .and_then(|details| details["availableModels"].as_array())
                .map(Vec::len),
            Some(3)
        );
    }

    #[test]
    fn bounded_query_preserves_all_rows_and_marks_complete() {
        let rows = (0..8)
            .map(|index| json!({ "category": format!("category-{index}"), "inventory": index * 100 }))
            .collect::<Vec<_>>();
        let projected = project_business_data_result(
            BusinessDataAction::Query,
            Value::String(
                json!({
                    "status": "completed",
                    "action": "query",
                    "datasets": [{ "name": "inventory", "rowCount": 8, "fields": [], "previewRows": rows }]
                })
                .to_string(),
            ),
        )
        .expect("project bounded query");
        let parsed = parse_projected(projected);

        assert_eq!(parsed["completeness"], "complete");
        assert_eq!(parsed["datasets"][0]["returnedRowCount"], 8);
        assert_eq!(parsed["datasets"][0]["previewRows"].as_array().map(Vec::len), Some(8));
        assert_eq!(parsed["datasets"][0]["completeness"], "complete");
        assert_eq!(parsed["datasets"][0]["hasMore"], false);
    }

    #[test]
    fn oversized_query_returns_bounded_valid_preview_with_explicit_completeness() {
        let rows = (0..40)
            .map(|index| {
                json!({
                    "category": format!("category-{index}"),
                    "description": "x".repeat(240),
                    "inventory": index * 100
                })
            })
            .collect::<Vec<_>>();
        let projected = project_business_data_result(
            BusinessDataAction::Query,
            Value::String(
                json!({
                    "status": "completed",
                    "action": "query",
                    "executedQueryCount": 2,
                    "datasets": [
                        { "name": "inventory", "rowCount": 500, "fields": [], "previewRows": rows },
                        { "name": "overage", "rowCount": 500, "fields": [], "previewRows": rows }
                    ]
                })
                .to_string(),
            ),
        )
        .expect("project query preview");
        let text = projected.as_str().expect("string result");
        let parsed: Value = serde_json::from_str(text).expect("bounded result remains valid json");

        assert!(text.len() <= MODEL_RESULT_MAX_BYTES);
        assert_eq!(parsed["completeness"], "preview");
        assert_eq!(parsed["datasets"][0]["rowCount"], 500);
        assert_eq!(parsed["datasets"][0]["completeness"], "preview");
        assert_eq!(parsed["datasets"][0]["hasMore"], true);
        assert!(
            parsed["datasets"][0]["previewRows"]
                .as_array()
                .is_some_and(|rows| rows.len() < 40)
        );
    }
}

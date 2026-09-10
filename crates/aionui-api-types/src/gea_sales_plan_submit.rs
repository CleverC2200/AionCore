use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use serde_json::Value;
use utoipa::ToSchema;

/// An exact decimal value exchanged with GEA.
///
/// The local API serializes decimals as strings so JavaScript clients do not
/// silently lose precision. Upstream JSON numbers and strings are both
/// accepted.
#[derive(Debug, Clone, PartialEq, Eq, ToSchema)]
#[schema(value_type = String, example = "123.450")]
pub struct GeaSalesPlanDecimal(pub String);

impl Serialize for GeaSalesPlanDecimal {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for GeaSalesPlanDecimal {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match Value::deserialize(deserializer)? {
            Value::String(value) if !value.trim().is_empty() => Ok(Self(value)),
            Value::Number(value) => Ok(Self(value.to_string())),
            _ => Err(D::Error::custom("expected a decimal string or number")),
        }
    }
}

impl fmt::Display for GeaSalesPlanDecimal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A GEA `Long` identifier exposed to JavaScript as an exact decimal string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, ToSchema)]
#[schema(value_type = String, example = "9007199254740993")]
pub struct GeaSalesPlanId(pub String);

impl Serialize for GeaSalesPlanId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for GeaSalesPlanId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match Value::deserialize(deserializer)? {
            Value::String(value) if !value.trim().is_empty() => Ok(Self(value)),
            Value::Number(value) => Ok(Self(value.to_string())),
            _ => Err(D::Error::custom("expected an identifier string or integer")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GeaSalesPlanSubmitItem {
    pub sku_code: GeaSalesPlanId,
    pub product_categ_name: String,
    pub base_qty: GeaSalesPlanDecimal,
    pub qty: GeaSalesPlanDecimal,
    pub price: GeaSalesPlanDecimal,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GeaSalesPlanSubmitRequest {
    pub period_id: GeaSalesPlanId,
    pub period_month: String,
    pub plan_type_code: String,
    pub channel_code: String,
    pub dealer_code: GeaSalesPlanId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub province_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub area_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_name: Option<String>,
    pub target_qty: GeaSalesPlanDecimal,
    pub target_amount: GeaSalesPlanDecimal,
    pub submitter_code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submitter_name: Option<String>,
    pub items: Vec<GeaSalesPlanSubmitItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GeaSalesPlanSubmitReceipt {
    pub plan_id: String,
    pub version_id: String,
    pub seq: u32,
    pub status: u8,
    pub replayed: bool,
    pub request_id: String,
    pub trace_id: String,
    pub audit_id: String,
}

#[cfg(test)]
mod tests {
    use super::{GeaSalesPlanDecimal, GeaSalesPlanId};

    #[test]
    fn decimal_accepts_upstream_number_and_serializes_as_string() {
        let value: GeaSalesPlanDecimal = serde_json::from_str("123.450").expect("decimal must parse");
        assert_eq!(value.0, "123.450");
        assert_eq!(
            serde_json::to_string(&value).expect("decimal must serialize"),
            "\"123.450\""
        );
    }

    #[test]
    fn decimal_preserves_maximum_contract_precision_without_f64_rounding() {
        let value: GeaSalesPlanDecimal =
            serde_json::from_str("999999999999999.999").expect("maximum quantity must parse");
        assert_eq!(value.0, "999999999999999.999");
        assert_eq!(serde_json::to_string(&value).unwrap(), "\"999999999999999.999\"");
    }

    #[test]
    fn long_identifier_above_javascript_safe_integer_serializes_as_string() {
        let value: GeaSalesPlanId = serde_json::from_str("9007199254740993").expect("long id must parse");
        assert_eq!(value.0, "9007199254740993");
        assert_eq!(serde_json::to_string(&value).unwrap(), "\"9007199254740993\"");
    }
}

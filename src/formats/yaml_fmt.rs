//! YAML support.

use serde_json::Value;

pub(super) fn parse(text: &str) -> Result<Value, String> {
    serde_yaml::from_str(text).map_err(|e| e.to_string())
}

pub(super) fn dump(value: &Value) -> Result<String, String> {
    serde_yaml::to_string(value).map_err(|e| e.to_string())
}

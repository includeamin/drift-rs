//! TOML support. Datetimes and `nan`/`inf` have no JSON form; see [`Hints`].

use super::Hints;
use crate::join_pointer;
use serde_json::Value;

pub(super) fn parse(text: &str, hints: &mut Hints) -> Result<Value, String> {
    let table: toml::Value = toml::from_str(text).map_err(|e| e.to_string())?;
    Ok(toml_to_json(table, "", hints))
}

pub(super) fn dump(value: &Value, hints: &Hints) -> Result<String, String> {
    let table = json_to_toml(value, "", hints).map_err(|e| e.to_string())?;
    toml::to_string_pretty(&table).map_err(|e| e.to_string())
}

fn toml_to_json(value: toml::Value, path: &str, hints: &mut Hints) -> Value {
    match value {
        toml::Value::String(v) => Value::String(v),
        toml::Value::Integer(v) => Value::Number(v.into()),
        // JSON has no NaN or infinity; keep them as strings rather than losing them.
        toml::Value::Float(v) => serde_json::Number::from_f64(v).map_or_else(
            || {
                hints.special_floats.insert(path.into());
                Value::String(
                    if v.is_nan() {
                        "nan"
                    } else if v > 0.0 {
                        "inf"
                    } else {
                        "-inf"
                    }
                    .into(),
                )
            },
            Value::Number,
        ),
        toml::Value::Boolean(v) => Value::Bool(v),
        toml::Value::Datetime(v) => {
            hints.datetimes.insert(path.into());
            Value::String(v.to_string())
        }
        toml::Value::Array(v) => Value::Array(
            v.into_iter()
                .enumerate()
                .map(|(i, item)| toml_to_json(item, &join_pointer(path, &i.to_string()), hints))
                .collect(),
        ),
        toml::Value::Table(v) => Value::Object(
            v.into_iter()
                .map(|(k, item)| {
                    let child = join_pointer(path, &k);
                    (k, toml_to_json(item, &child, hints))
                })
                .collect(),
        ),
    }
}

fn json_to_toml(
    value: &Value,
    path: &str,
    hints: &Hints,
) -> Result<toml::Value, Box<dyn std::error::Error>> {
    Ok(match value {
        Value::Object(map) => toml::Value::Table(
            map.iter()
                .map(|(key, value)| {
                    Ok((key.clone(), json_to_toml(value, &join_pointer(path, key), hints)?))
                })
                .collect::<Result<_, Box<dyn std::error::Error>>>()?,
        ),
        Value::Array(values) => toml::Value::Array(
            values
                .iter()
                .enumerate()
                .map(|(i, value)| json_to_toml(value, &join_pointer(path, &i.to_string()), hints))
                .collect::<Result<_, _>>()?,
        ),
        Value::String(text) => restore_toml_type(text, path, hints),
        Value::Number(value) => value
            .as_i64()
            .map(toml::Value::Integer)
            .or_else(|| value.as_f64().map(toml::Value::Float))
            .ok_or("invalid TOML number")?,
        Value::Bool(value) => toml::Value::Boolean(*value),
        Value::Null => return Err("null cannot be represented in TOML".into()),
    })
}

/// A string at a path that held a datetime or `nan`/`inf` in the source goes
/// back to that type, if it still reads as one. Anything else stays a string.
fn restore_toml_type(text: &str, path: &str, hints: &Hints) -> toml::Value {
    if hints.datetimes.contains(path) {
        if let Ok(datetime) = text.parse::<toml::value::Datetime>() {
            return toml::Value::Datetime(datetime);
        }
    }
    if hints.special_floats.contains(path) {
        match text {
            "nan" => return toml::Value::Float(f64::NAN),
            "inf" => return toml::Value::Float(f64::INFINITY),
            "-inf" => return toml::Value::Float(f64::NEG_INFINITY),
            _ => {}
        }
    }
    toml::Value::String(text.into())
}

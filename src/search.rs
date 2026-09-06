use crate::{split_pointer, Delta, DriftError};
use regex::Regex;
use serde_json::Value;

pub fn path_matches(pattern: &str, path: &str) -> Result<bool, DriftError> {
    let pattern = split_pointer(pattern)?;
    let path = split_pointer(path)?;
    fn go(pattern: &[String], path: &[String]) -> bool {
        if pattern.is_empty() {
            return path.is_empty();
        }
        if pattern[0] == "**" {
            (0..=path.len()).any(|i| go(&pattern[1..], &path[i..]))
        } else {
            !path.is_empty()
                && (pattern[0] == "*" || pattern[0] == path[0])
                && go(&pattern[1..], &path[1..])
        }
    }
    Ok(go(&pattern, &path))
}

fn get<'a>(document: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = document;
    for segment in split_pointer(path).ok()? {
        current = match current {
            Value::Object(map) => map.get(&segment)?,
            Value::Array(items) => items.get(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}

pub fn filter_operations(
    operations: &[Delta],
    old: Option<&Value>,
    paths: &[String],
    fields: &[String],
    values: &[String],
    operation_types: &[String],
    invert: bool,
) -> Result<Vec<Delta>, DriftError> {
    let field_regex = fields
        .iter()
        .map(|p| {
            Regex::new(p)
                .map_err(|e| DriftError::Regex(format!("invalid --field regular expression: {e}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let value_regex = values
        .iter()
        .map(|p| {
            Regex::new(p)
                .map_err(|e| DriftError::Regex(format!("invalid --grep regular expression: {e}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(operations
        .iter()
        .filter(|operation| {
            let path_ok = paths.is_empty()
                || paths.iter().any(|p| path_matches(p, &operation.path).unwrap_or(false));
            let field_ok = fields.is_empty()
                || split_pointer(&operation.path)
                    .ok()
                    .and_then(|parts| parts.last().cloned())
                    .is_some_and(|field| field_regex.iter().any(|r| r.is_match(&field)));
            let mut haystacks = vec![
                Value::String(operation.path.clone()),
                operation.value.clone().unwrap_or(Value::Null),
            ];
            if matches!(operation.op, crate::Operation::Remove | crate::Operation::Replace) {
                if let Some(value) = old.and_then(|doc| get(doc, &operation.path)) {
                    haystacks.insert(1, value.clone());
                }
            }
            if let Some(from) = &operation.from_path {
                haystacks.push(Value::String(from.clone()));
            }
            let value_ok = values.is_empty()
                || value_regex.iter().any(|r| {
                    haystacks
                        .iter()
                        .any(|v| serde_json::to_string(v).is_ok_and(|text| r.is_match(&text)))
                });
            let type_ok = operation_types.is_empty()
                || operation_types.iter().any(|kind| kind == operation.op.as_str());
            (path_ok && field_ok && value_ok && type_ok) != invert
        })
        .cloned()
        .collect())
}

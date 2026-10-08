use crate::{split_pointer, Delta, DriftError};
use regex::Regex;
use serde_json::Value;

/// Whether `path` matches the glob `pattern`, both JSON Pointers.
///
/// `*` stands for exactly one token and `**` for any number, including none:
/// `/users/*/name` matches `/users/0/name`, and `/a/**` matches `/a` and
/// everything under it.
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

/// The value each `remove`/`replace` operation overwrites, looked up in the
/// document as it is when that operation runs.
///
/// Paths are relative to the document at that step of the patch, not to the
/// original, so the operations are replayed. If one fails to apply, the rest
/// get `None`.
fn overwritten_values(old: &Value, operations: &[Delta]) -> Vec<Option<Value>> {
    let mut document = old.clone();
    let mut live = true;
    operations
        .iter()
        .map(|operation| {
            if !live {
                return None;
            }
            let before =
                matches!(operation.op, crate::Operation::Remove | crate::Operation::Replace)
                    .then(|| get(&document, &operation.path).cloned())
                    .flatten();
            match crate::patch(std::mem::take(&mut document), std::slice::from_ref(operation)) {
                Ok(next) => document = next,
                Err(_) => live = false,
            }
            before
        })
        .collect()
}

/// Keeps the operations that match every given filter, or those that do not
/// when `invert` is set. An empty filter list matches everything.
///
/// - `paths`: glob patterns for [`path_matches`]; any may match.
/// - `fields`: regular expressions for the last token of the path.
/// - `values`: regular expressions tried against the operation's path and
///   JSON-serialised value and, with `old` given, the value a `remove` or
///   `replace` overwrites.
/// - `operation_types`: names such as `"add"` (see [`Operation::as_str`](crate::Operation::as_str)).
///
/// `old` is the document the operations were computed against; without it,
/// `values` cannot see overwritten values.
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
    let overwritten = match old {
        Some(old) if !values.is_empty() => overwritten_values(old, operations),
        _ => vec![None; operations.len()],
    };
    Ok(operations
        .iter()
        .zip(overwritten)
        .filter(|(operation, overwritten)| {
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
            if let Some(value) = overwritten {
                haystacks.insert(1, value.clone());
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
        .map(|(operation, _)| operation.clone())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{diff_with, DiffOptions, Operation};
    use serde_json::json;

    #[test]
    fn grep_sees_old_values_when_keyed_items_shift() {
        // After the front insert, "b" sits at /2 in the new array but /1 in the old one.
        let old = json!([{"id": 1, "v": "a"}, {"id": 2, "v": "b"}]);
        let new = json!([{"id": 0, "v": "z"}, {"id": 1, "v": "a"}, {"id": 2, "v": "B"}]);
        let options = DiffOptions { array_keys: vec!["id".into()] };
        let ops = diff_with(&new, &old, &options);
        let kept =
            filter_operations(&ops, Some(&old), &[], &[], &["\"b\"".into()], &[], false).unwrap();
        assert_eq!(kept.len(), 1, "{ops:?}");
        assert_eq!(kept[0].op, Operation::Replace);
        assert_eq!(kept[0].path, "/2/v");
    }

    #[test]
    fn grep_old_values_with_positional_removals() {
        let old = json!({"xs": [1, 2, 3, 4]});
        let new = json!({"xs": [1]});
        let ops = crate::diff(&new, &old);
        let kept =
            filter_operations(&ops, Some(&old), &[], &[], &["^3$".into()], &[], false).unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].path, "/xs/2");
    }

    #[test]
    fn double_star_matches_zero_or_more_tokens() {
        assert!(path_matches("/a/**", "/a").unwrap());
        assert!(path_matches("/a/**", "/a/b/c").unwrap());
        assert!(path_matches("/users/*/name", "/users/0/name").unwrap());
        assert!(!path_matches("/users/*/name", "/users/0/1/name").unwrap());
        assert!(!path_matches("/a/**", "/b").unwrap());
    }
}

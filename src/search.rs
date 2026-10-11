use crate::{split_pointer, Delta, DriftError};
use regex::Regex;
use serde_json::Value;

/// Whether `path` matches the glob `pattern`, both JSON Pointers.
///
/// `*` stands for exactly one token and `**` for any number, including none:
/// `/users/*/name` matches `/users/0/name`, and `/a/**` matches `/a` and
/// everything under it.
pub fn path_matches(pattern: &str, path: &str) -> Result<bool, DriftError> {
    Ok(glob_match(&split_pointer(pattern)?, &split_pointer(path)?))
}

/// Matches the tokens of a path against the tokens of a glob pattern.
pub(crate) fn glob_match<P: AsRef<str>, T: AsRef<str>>(pattern: &[P], path: &[T]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((first, rest)) if first.as_ref() == "**" => {
            (0..=path.len()).any(|skip| glob_match(rest, &path[skip..]))
        }
        Some((first, rest)) => path.split_first().is_some_and(|(token, remaining)| {
            (first.as_ref() == "*" || first.as_ref() == token.as_ref())
                && glob_match(rest, remaining)
        }),
    }
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
    let filter = OperationFilter::new(paths, fields, values, operation_types, invert)?;
    let overwritten = match old {
        Some(old) if !values.is_empty() => overwritten_values(old, operations),
        _ => vec![None; operations.len()],
    };
    Ok(operations
        .iter()
        .zip(overwritten)
        .filter(|(operation, overwritten)| filter.keep(operation, overwritten.as_ref()))
        .map(|(operation, _)| operation.clone())
        .collect())
}

/// The filters of [`filter_operations`], applied to one operation at a time, for
/// callers that handle operations as they arrive (see
/// [`diff_records_each`](crate::diff_records_each)) instead of holding a list.
///
/// ```
/// use drift::{Delta, Operation, OperationFilter};
///
/// let only_adds = OperationFilter::new(&[], &[], &[], &["add".to_string()], false)?;
/// assert!(only_adds.keep(&Delta::new(Operation::Add, "/a"), None));
/// assert!(!only_adds.keep(&Delta::new(Operation::Remove, "/a"), None));
/// # Ok::<(), drift::DriftError>(())
/// ```
pub struct OperationFilter {
    paths: Vec<String>,
    fields: Vec<Regex>,
    values: Vec<Regex>,
    operation_types: Vec<String>,
    invert: bool,
}

impl OperationFilter {
    /// Builds the filter; the arguments mean what they do for [`filter_operations`].
    /// An invalid regular expression is an error here, not on first use.
    pub fn new(
        paths: &[String],
        fields: &[String],
        values: &[String],
        operation_types: &[String],
        invert: bool,
    ) -> Result<Self, DriftError> {
        let compile = |patterns: &[String], what: &str| {
            patterns
                .iter()
                .map(|p| {
                    Regex::new(p).map_err(|e| {
                        DriftError::Regex(format!("invalid --{what} regular expression: {e}"))
                    })
                })
                .collect::<Result<Vec<_>, _>>()
        };
        Ok(Self {
            paths: paths.to_vec(),
            fields: compile(fields, "field")?,
            values: compile(values, "grep")?,
            operation_types: operation_types.to_vec(),
            invert,
        })
    }

    /// Whether to keep `operation`. `overwritten` is the value a `remove` or
    /// `replace` overwrote, which only the value patterns look at.
    pub fn keep(&self, operation: &Delta, overwritten: Option<&Value>) -> bool {
        let path_ok = self.paths.is_empty()
            || self.paths.iter().any(|p| path_matches(p, &operation.path).unwrap_or(false));
        let field_ok = self.fields.is_empty()
            || split_pointer(&operation.path)
                .ok()
                .and_then(|parts| parts.last().cloned())
                .is_some_and(|field| self.fields.iter().any(|r| r.is_match(&field)));
        let value_ok = self.values.is_empty() || {
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
            self.values.iter().any(|r| {
                haystacks
                    .iter()
                    .any(|v| serde_json::to_string(v).is_ok_and(|text| r.is_match(&text)))
            })
        };
        let type_ok = self.operation_types.is_empty()
            || self.operation_types.iter().any(|kind| kind == operation.op.as_str());
        (path_ok && field_ok && value_ok && type_ok) != self.invert
    }
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
        let options = DiffOptions::new().array_key("id");
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

    #[test]
    fn the_per_operation_filter_agrees_with_filtering_a_list() {
        let ops = vec![
            Delta::with_value(Operation::Add, "/users/0/name", json!("Ada")),
            Delta::new(Operation::Remove, "/users/1"),
            Delta::with_value(Operation::Replace, "/users/2/email", json!("x@y.z")),
            Delta::with_value(Operation::Replace, "/meta/updatedAt", json!("2026-01-01")),
            Delta::with_value(Operation::Add, "/meta/tags/-", json!(["a"])),
            Delta {
                op: Operation::Move,
                path: "/users/0".into(),
                value: None,
                from_path: Some("/users/3".into()),
            },
        ];
        let strings = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let path_sets = [vec![], strings(&["/users/**"]), strings(&["/meta/*", "/users/0"])];
        let field_sets = [vec![], strings(&["^name$"]), strings(&["mail", "At$"])];
        let value_sets = [vec![], strings(&["Ada"]), strings(&["\\[\"a\"\\]", "x@y"])];
        let type_sets = [vec![], strings(&["add"]), strings(&["replace", "move"])];
        for paths in &path_sets {
            for fields in &field_sets {
                for values in &value_sets {
                    for types in &type_sets {
                        for invert in [false, true] {
                            let list =
                                filter_operations(&ops, None, paths, fields, values, types, invert)
                                    .unwrap();
                            let filter =
                                OperationFilter::new(paths, fields, values, types, invert).unwrap();
                            let one_by_one: Vec<Delta> =
                                ops.iter().filter(|op| filter.keep(op, None)).cloned().collect();
                            assert_eq!(
                                one_by_one, list,
                                "{paths:?} {fields:?} {values:?} {types:?} invert={invert}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_bad_pattern_is_reported_when_the_filter_is_built() {
        let bad = vec!["(".to_string()];
        assert!(matches!(
            OperationFilter::new(&[], &bad, &[], &[], false),
            Err(DriftError::Regex(_))
        ));
        assert!(matches!(
            OperationFilter::new(&[], &[], &bad, &[], false),
            Err(DriftError::Regex(_))
        ));
    }
}

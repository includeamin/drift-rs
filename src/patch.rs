use crate::{split_pointer, Delta, DriftError, Operation};
use serde_json::Value;

fn array_index(segment: &str, length: usize, allow_end: bool) -> Result<usize, DriftError> {
    if segment.is_empty()
        || (segment.len() > 1 && segment.starts_with('0'))
        || !segment.chars().all(|c| c.is_ascii_digit())
    {
        return Err(DriftError::Index(segment.into()));
    }
    let index = segment.parse::<usize>().map_err(|_| DriftError::Index(segment.into()))?;
    if index > length || (index == length && !allow_end) {
        return Err(DriftError::Index(segment.into()));
    }
    Ok(index)
}

fn get<'a>(document: &'a Value, path: &str) -> Result<&'a Value, DriftError> {
    let mut current = document;
    for segment in split_pointer(path)? {
        current = match current {
            Value::Object(map) => {
                map.get(&segment).ok_or_else(|| DriftError::Missing(path.into()))?
            }
            Value::Array(items) => &items[array_index(&segment, items.len(), false)?],
            _ => {
                return Err(DriftError::Operation {
                    operation: "read".into(),
                    path: path.into(),
                    reason: "cannot traverse a scalar".into(),
                })
            }
        };
    }
    Ok(current)
}

fn parent_mut<'a>(
    document: &'a mut Value,
    path: &str,
) -> Result<(&'a mut Value, String), DriftError> {
    let mut parts = split_pointer(path)?;
    let segment = parts.pop().ok_or_else(|| DriftError::Operation {
        operation: "modify".into(),
        path: path.into(),
        reason: "the root has no parent".into(),
    })?;
    let mut current = document;
    for part in parts {
        current = match current {
            Value::Object(map) => {
                map.get_mut(&part).ok_or_else(|| DriftError::Missing(part.clone()))?
            }
            Value::Array(items) => {
                let index = array_index(&part, items.len(), false)?;
                &mut items[index]
            }
            _ => {
                return Err(DriftError::Operation {
                    operation: "modify".into(),
                    path: path.into(),
                    reason: "cannot traverse a scalar".into(),
                })
            }
        };
    }
    Ok((current, segment))
}

fn add(document: Value, path: &str, value: Value) -> Result<Value, DriftError> {
    if path.is_empty() {
        return Ok(value);
    }
    let mut document = document;
    let (parent, segment) = parent_mut(&mut document, path)?;
    match parent {
        Value::Object(map) => {
            map.insert(segment, value);
        }
        Value::Array(items) => {
            let index = if segment == "-" {
                items.len()
            } else {
                array_index(&segment, items.len(), true)?
            };
            items.insert(index, value);
        }
        _ => {
            return Err(DriftError::Operation {
                operation: "add".into(),
                path: path.into(),
                reason: "cannot add to a scalar".into(),
            })
        }
    }
    Ok(document)
}

fn remove(mut document: Value, path: &str) -> Result<Value, DriftError> {
    let (parent, segment) = parent_mut(&mut document, path)?;
    match parent {
        Value::Object(map) => {
            map.remove(&segment).ok_or(DriftError::Missing(segment))?;
        }
        Value::Array(items) => {
            items.remove(array_index(&segment, items.len(), false)?);
        }
        _ => {
            return Err(DriftError::Operation {
                operation: "remove".into(),
                path: path.into(),
                reason: "cannot remove from a scalar".into(),
            })
        }
    }
    Ok(document)
}

fn replace(mut document: Value, path: &str, value: Value) -> Result<Value, DriftError> {
    if path.is_empty() {
        return Ok(value);
    }
    let (parent, segment) = parent_mut(&mut document, path)?;
    match parent {
        Value::Object(map) => {
            if !map.contains_key(&segment) {
                return Err(DriftError::Missing(segment));
            }
            map.insert(segment, value);
        }
        Value::Array(items) => {
            let index = array_index(&segment, items.len(), false)?;
            items[index] = value;
        }
        _ => {
            return Err(DriftError::Operation {
                operation: "replace".into(),
                path: path.into(),
                reason: "cannot replace in a scalar".into(),
            })
        }
    }
    Ok(document)
}

/// Applies RFC 6902 JSON Patch operations to a document.
///
/// # Performance Characteristics
/// - **Time Complexity**: O(n * m) where n is operations count, m is path depth
/// - **Space Complexity**: O(d) where d is the maximum nesting depth
/// - **Mutations**: In-place modifications; no intermediate copies for most operations
/// - **Array Operations**: Insert/remove in middle of arrays is O(n) due to element shifting
/// - **Path Lookup**: Each operation requires tree traversal; deeply nested paths are slower
///
/// # Optimization Tips
/// - Batch related operations to reduce path traversals
/// - Avoid inserting into middle of large arrays (append instead)
/// - Operations are applied sequentially; order matters for correctness
/// - Consider applying operations to specific subtrees when possible
///
/// # Example
/// ```
/// use drift::{diff, patch};
/// use serde_json::json;
///
/// let doc = json!({"x": 1});
/// let new = json!({"x": 2});
/// let ops = diff(&new, &doc);
/// let result = patch(doc, &ops).unwrap();
/// assert_eq!(result, new);
/// ```
pub fn patch(mut document: Value, operations: &[Delta]) -> Result<Value, DriftError> {
    for operation in operations {
        document = match operation.op {
            Operation::Add => {
                add(document, &operation.path, operation.value.clone().unwrap_or(Value::Null))?
            }
            Operation::Remove => remove(document, &operation.path)?,
            Operation::Replace => {
                replace(document, &operation.path, operation.value.clone().unwrap_or(Value::Null))?
            }
            Operation::Test => {
                if get(&document, &operation.path)?
                    != operation.value.as_ref().unwrap_or(&Value::Null)
                {
                    return Err(DriftError::Test(operation.path.clone()));
                }
                document
            }
            Operation::Copy | Operation::Move => {
                let from = operation.from_path.as_ref().ok_or_else(|| DriftError::Operation {
                    operation: operation.op.as_str().into(),
                    path: operation.path.clone(),
                    reason: "missing from path".into(),
                })?;
                let value = get(&document, from)?.clone();
                let document = if operation.op == Operation::Move {
                    remove(document, from)?
                } else {
                    document
                };
                add(document, &operation.path, value)?
            }
        };
    }
    Ok(document)
}

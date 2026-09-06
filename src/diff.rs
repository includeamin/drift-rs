use crate::{join_pointer, Delta, Operation};
use serde_json::Value;
use std::collections::BTreeSet;

fn strict_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Null, Value::Null)
        | (Value::Bool(_), Value::Bool(_))
        | (Value::String(_), Value::String(_)) => left == right,
        (Value::Number(_), Value::Number(_)) => left == right,
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| strict_equal(x, y))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter().all(|(k, v)| b.get(k).is_some_and(|other| strict_equal(v, other)))
        }
        _ => false,
    }
}

pub(crate) fn diff_inner(new: &Value, old: &Value, path: &str, output: &mut Vec<Delta>) {
    match (new, old) {
        (Value::Object(new_obj), Value::Object(old_obj)) => {
            let old_keys: BTreeSet<_> = old_obj.keys().collect();
            let new_keys: BTreeSet<_> = new_obj.keys().collect();
            for key in old_keys.difference(&new_keys) {
                output.push(Delta::new(Operation::Remove, join_pointer(path, key)));
            }
            for key in new_keys.difference(&old_keys) {
                output.push(Delta::with_value(
                    Operation::Add,
                    join_pointer(path, key),
                    new_obj[*key].clone(),
                ));
            }
            for key in old_keys.intersection(&new_keys) {
                diff_inner(&new_obj[*key], &old_obj[*key], &join_pointer(path, key), output);
            }
        }
        (Value::Array(new_items), Value::Array(old_items)) => {
            for (index, (new_value, old_value)) in new_items.iter().zip(old_items).enumerate() {
                diff_inner(new_value, old_value, &join_pointer(path, &index.to_string()), output);
            }
            for index in (new_items.len()..old_items.len()).rev() {
                output.push(Delta::new(Operation::Remove, join_pointer(path, &index.to_string())));
            }
            for (index, item) in new_items.iter().enumerate().skip(old_items.len()) {
                output.push(Delta::with_value(
                    Operation::Add,
                    join_pointer(path, &index.to_string()),
                    item.clone(),
                ));
            }
        }
        _ if !strict_equal(new, old) => {
            output.push(Delta::with_value(Operation::Replace, path, new.clone()))
        }
        _ => {}
    }
}

/// Calculates RFC 6902 JSON Patch operations needed to transform `old` into `new`.
///
/// # Performance Characteristics
/// - **Time Complexity**: O(n) where n is the total number of nodes across both values
/// - **Space Complexity**: O(n) for the output operations plus O(d) for recursion stack
/// - **Cloning**: Each add/replace operation clones the new value tree
/// - **Arrays**: Element-by-element comparison; middle changes trigger O(n) operations
/// - **Objects**: Key set comparison using BTreeSet; O(k log k) for k keys
///
/// # Optimization Tips
/// - For large objects, diff only changed subtrees when possible
/// - Array insertions in the middle are less efficient than appends
/// - Value cloning overhead grows with value size for add/replace operations
///
/// # Example
/// ```
/// use drift::diff;
/// use serde_json::json;
///
/// let old = json!({"name": "Alice"});
/// let new = json!({"name": "Bob"});
/// let ops = diff(&new, &old);
/// assert_eq!(ops.len(), 1);
/// ```
pub fn diff(new: &Value, old: &Value) -> Vec<Delta> {
    let mut output = Vec::new();
    diff_inner(new, old, "", &mut output);
    output
}

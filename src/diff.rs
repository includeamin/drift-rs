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

/// Options for [`diff_with`].
#[derive(Debug, Clone, Default)]
pub struct DiffOptions {
    /// Field names that identify array items, tried in order.
    ///
    /// For an array whose items are all objects with a unique scalar value for
    /// one of these fields, items are matched by that value instead of by
    /// position. Inserting or removing an item then yields one operation
    /// instead of a run of replacements, and reordering yields `move`
    /// operations. Arrays without a usable key are compared by position.
    pub array_keys: Vec<String>,
}

const POSITIONAL: DiffOptions = DiffOptions { array_keys: Vec::new() };

pub(crate) fn diff_inner(new: &Value, old: &Value, path: &str, output: &mut Vec<Delta>) {
    walk(new, old, path, &POSITIONAL, output);
}

/// Identity of an array item under `key`, distinguishing `1` from `"1"`.
fn identity(item: &Value, key: &str) -> Option<String> {
    match item.get(key)? {
        value @ (Value::String(_) | Value::Number(_) | Value::Bool(_)) => Some(value.to_string()),
        _ => None,
    }
}

/// For each item of `new`, the index of the item in `old` with the same key,
/// or `None` if it is new. Returns `None` when no configured key identifies
/// every item of both arrays uniquely, or when either array is empty.
pub fn match_array_items(
    new: &[Value],
    old: &[Value],
    keys: &[String],
) -> Option<Vec<Option<usize>>> {
    if new.is_empty() || old.is_empty() {
        return None;
    }
    keys.iter().find_map(|key| {
        let identities = |items: &[Value]| -> Option<Vec<String>> {
            let ids: Vec<String> = items.iter().map(|i| identity(i, key)).collect::<Option<_>>()?;
            let unique: BTreeSet<&String> = ids.iter().collect();
            (unique.len() == ids.len()).then_some(ids)
        };
        let old_ids = identities(old)?;
        let new_ids = identities(new)?;
        let position: std::collections::HashMap<&String, usize> =
            old_ids.iter().enumerate().map(|(i, id)| (id, i)).collect();
        Some(new_ids.iter().map(|id| position.get(id).copied()).collect())
    })
}

/// Emits operations for an array whose items were matched by key.
///
/// Order matters because paths are indices into the array as it is at that
/// point of the patch: first remove the unmatched old items (highest index
/// first), then walk the new order, adding or moving each item into its final
/// slot and diffing matched pairs once their index is final.
fn keyed_array(
    new: &[Value],
    old: &[Value],
    matches: &[Option<usize>],
    path: &str,
    options: &DiffOptions,
    output: &mut Vec<Delta>,
) {
    let mut kept = vec![false; old.len()];
    matches.iter().flatten().for_each(|&i| kept[i] = true);
    for index in (0..old.len()).rev().filter(|&i| !kept[i]) {
        output.push(Delta::new(Operation::Remove, join_pointer(path, &index.to_string())));
    }

    // Old index of the item currently at each position (`None` for added items).
    let mut current: Vec<Option<usize>> = (0..old.len()).filter(|&i| kept[i]).map(Some).collect();
    for (index, item) in new.iter().enumerate() {
        let target = join_pointer(path, &index.to_string());
        match matches[index] {
            Some(old_index) => {
                let from = current
                    .iter()
                    .position(|&slot| slot == Some(old_index))
                    .expect("matched item is present");
                if from != index {
                    output.push(Delta {
                        op: Operation::Move,
                        path: target.clone(),
                        value: None,
                        from_path: Some(join_pointer(path, &from.to_string())),
                    });
                    let slot = current.remove(from);
                    current.insert(index, slot);
                }
                walk(item, &old[old_index], &target, options, output);
            }
            None => {
                output.push(Delta::with_value(Operation::Add, target, item.clone()));
                current.insert(index, None);
            }
        }
    }
}

fn walk(new: &Value, old: &Value, path: &str, options: &DiffOptions, output: &mut Vec<Delta>) {
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
                walk(&new_obj[*key], &old_obj[*key], &join_pointer(path, key), options, output);
            }
        }
        (Value::Array(new_items), Value::Array(old_items)) => {
            if let Some(matches) = match_array_items(new_items, old_items, &options.array_keys) {
                return keyed_array(new_items, old_items, &matches, path, options, output);
            }
            for (index, (new_value, old_value)) in new_items.iter().zip(old_items).enumerate() {
                walk(
                    new_value,
                    old_value,
                    &join_pointer(path, &index.to_string()),
                    options,
                    output,
                );
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
    diff_with(new, old, &POSITIONAL)
}

/// Like [`diff`], with [`DiffOptions`] such as key-based array matching.
///
/// # Example
/// ```
/// use drift::{diff_with, DiffOptions};
/// use serde_json::json;
///
/// let old = json!([{"id": 1}, {"id": 2}]);
/// let new = json!([{"id": 0}, {"id": 1}, {"id": 2}]);
/// let options = DiffOptions { array_keys: vec!["id".into()] };
/// assert_eq!(diff_with(&new, &old, &options).len(), 1);
/// ```
pub fn diff_with(new: &Value, old: &Value, options: &DiffOptions) -> Vec<Delta> {
    let mut output = Vec::new();
    walk(new, old, "", options, &mut output);
    output
}

#[cfg(test)]
mod keyed_tests {
    use super::*;
    use crate::patch;
    use serde_json::json;

    fn keyed(names: &[&str]) -> DiffOptions {
        DiffOptions { array_keys: names.iter().map(|n| n.to_string()).collect() }
    }

    fn ops(new: &Value, old: &Value, options: &DiffOptions) -> Vec<(String, String)> {
        diff_with(new, old, options)
            .iter()
            .map(|d| (d.op.as_str().into(), d.path.clone()))
            .collect()
    }

    fn roundtrips(new: &Value, old: &Value, options: &DiffOptions) {
        let deltas = diff_with(new, old, options);
        let patched = patch(old.clone(), &deltas).unwrap_or_else(|e| panic!("{e}: {deltas:?}"));
        assert_eq!(&patched, new, "{old} -> {new}: {deltas:?}");
    }

    #[test]
    fn default_options_match_positional_diff() {
        let old = json!([{"id": 1}, {"id": 2}]);
        let new = json!([{"id": 0}, {"id": 1}, {"id": 2}]);
        assert_eq!(diff_with(&new, &old, &DiffOptions::default()), diff(&new, &old));
    }

    #[test]
    fn front_insert_is_a_single_add() {
        let old = json!([{"id": 1, "v": "a"}, {"id": 2, "v": "b"}]);
        let new = json!([{"id": 0, "v": "z"}, {"id": 1, "v": "a"}, {"id": 2, "v": "b"}]);
        assert_eq!(ops(&new, &old, &keyed(&["id"])), vec![("add".into(), "/0".into())]);
        roundtrips(&new, &old, &keyed(&["id"]));
    }

    #[test]
    fn middle_delete_is_a_single_remove() {
        let old = json!([{"id": 1}, {"id": 2}, {"id": 3}]);
        let new = json!([{"id": 1}, {"id": 3}]);
        assert_eq!(ops(&new, &old, &keyed(&["id"])), vec![("remove".into(), "/1".into())]);
    }

    #[test]
    fn reorder_uses_move() {
        let old = json!([{"id": 1}, {"id": 2}, {"id": 3}]);
        let new = json!([{"id": 3}, {"id": 1}, {"id": 2}]);
        let deltas = diff_with(&new, &old, &keyed(&["id"]));
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].op, Operation::Move);
        assert_eq!(deltas[0].from_path.as_deref(), Some("/2"));
        assert_eq!(deltas[0].path, "/0");
        roundtrips(&new, &old, &keyed(&["id"]));
    }

    #[test]
    fn matched_items_are_diffed_in_place() {
        let old = json!({"users": [{"id": 1, "n": "a"}, {"id": 2, "n": "b"}]});
        let new = json!({"users": [{"id": 2, "n": "B"}, {"id": 1, "n": "a"}]});
        roundtrips(&new, &old, &keyed(&["id"]));
        assert!(ops(&new, &old, &keyed(&["id"])).contains(&("replace".into(), "/users/0/n".into())));
    }

    #[test]
    fn falls_back_to_positional_when_key_is_unusable() {
        let options = keyed(&["id"]);
        let duplicates = json!([{"id": 1}, {"id": 1}]);
        let missing = json!([{"id": 1}, {"name": "x"}]);
        let scalars = json!([1, 2]);
        for old in [&duplicates, &missing, &scalars] {
            let new = json!([{"id": 9}]);
            assert_eq!(diff_with(&new, old, &options), diff(&new, old));
        }
    }

    #[test]
    fn first_usable_key_wins() {
        let old = json!([{"id": 1, "name": "a"}, {"id": 1, "name": "b"}]);
        let new = json!([{"id": 1, "name": "b"}, {"id": 1, "name": "a"}]);
        roundtrips(&new, &old, &keyed(&["id", "name"]));
        assert_eq!(diff_with(&new, &old, &keyed(&["id", "name"])).len(), 1);
    }

    #[test]
    fn string_and_number_keys_do_not_collide() {
        let old = json!([{"id": 1}, {"id": "1"}]);
        let new = json!([{"id": "1"}, {"id": 1}]);
        roundtrips(&new, &old, &keyed(&["id"]));
    }

    #[test]
    fn random_keyed_arrays_round_trip() {
        // xorshift: deterministic, no extra dependencies.
        let mut state = 0x2545F4914F6CDD1Du64;
        let mut next = move |n: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % n
        };
        let options = keyed(&["id"]);
        for _ in 0..500 {
            let make = |next: &mut dyn FnMut(u64) -> u64| {
                let mut ids: Vec<u64> = (0..8).filter(|_| next(2) == 0).collect();
                for i in (1..ids.len()).rev() {
                    ids.swap(i, next(i as u64 + 1) as usize);
                }
                Value::Array(
                    ids.into_iter()
                        .map(|id| json!({"id": id, "v": next(3), "tags": [next(2), next(2)]}))
                        .collect(),
                )
            };
            let old = json!({"list": make(&mut next), "x": 1});
            let new = json!({"list": make(&mut next), "x": 1});
            roundtrips(&new, &old, &options);
        }
    }
}

//! Undoing and squashing patches.

use crate::patch::get;
use crate::{
    diff_with, join_pointer, patch, unescape_token, Delta, DiffOptions, DriftError, Operation,
};
use serde_json::Value;

/// The operations that undo `operations` when applied to `document`'s result.
///
/// `document` is the document the patch is applied to. The patch is replayed
/// to see what each operation overwrites or removes, so it must apply cleanly;
/// a `test` that fails or a missing path is an error, as with [`patch`](crate::patch).
///
/// ```
/// use drift::{diff, invert, patch};
/// use serde_json::json;
///
/// let old = json!({"name": "David", "tags": ["a", "b"]});
/// let new = json!({"name": "Alex", "tags": ["a"]});
///
/// let operations = diff(&new, &old);
/// let undo = invert(&old, &operations)?;
/// assert_eq!(patch(new, &undo)?, old);
/// # Ok::<(), drift::DriftError>(())
/// ```
///
/// Undoing a `remove` or a `replace` that overwrote an object member puts the
/// member back as a new key, so the restored document is equal to the original
/// but the member may sit last instead of where it was.
pub fn invert(document: &Value, operations: &[Delta]) -> Result<Vec<Delta>, DriftError> {
    let mut current = document.clone();
    let mut undo = Vec::with_capacity(operations.len());
    for operation in operations {
        undo.push(undo_one(&current, operation)?);
        current = patch(current, std::slice::from_ref(operation))?;
    }
    // Last operation first.
    Ok(undo.into_iter().rev().flatten().collect())
}

/// One patch equivalent to applying `operations` to `document`, without the
/// detours: a value set twice is set once, and an added key that is later
/// removed disappears.
///
/// This needs the document the patch applies to, because the result is the
/// difference between that document and the patched one, computed with
/// [`diff_with`] and `options`. The operations may differ in form from the
/// originals, for example an add and a remove of the same array item can
/// become a replace.
///
/// ```
/// use drift::{compose, Delta, DiffOptions, Operation};
/// use serde_json::json;
///
/// let doc = json!({"count": 1});
/// let steps: Vec<Delta> = vec![
///     Delta::with_value(Operation::Replace, "/count", json!(2)),
///     Delta::with_value(Operation::Replace, "/count", json!(3)),
/// ];
/// let squashed = compose(&doc, &steps, &DiffOptions::default())?;
/// assert_eq!(squashed.len(), 1);
/// # Ok::<(), drift::DriftError>(())
/// ```
pub fn compose(
    document: &Value,
    operations: &[Delta],
    options: &DiffOptions,
) -> Result<Vec<Delta>, DriftError> {
    let result = patch(document.clone(), operations)?;
    Ok(diff_with(&result, document, options))
}

/// What undoes `operation`, given the document just before it runs.
fn undo_one(document: &Value, operation: &Delta) -> Result<Vec<Delta>, DriftError> {
    let path = operation.path.as_str();
    Ok(match operation.op {
        Operation::Test => Vec::new(),
        Operation::Replace => {
            vec![Delta::with_value(Operation::Replace, path, get(document, path)?.clone())]
        }
        Operation::Remove => {
            vec![Delta::with_value(Operation::Add, path, get(document, path)?.clone())]
        }
        Operation::Add | Operation::Copy => vec![undo_insert(document, path)?],
        Operation::Move => {
            let from = operation.from_path.as_deref().ok_or_else(|| DriftError::Operation {
                operation: "move".into(),
                path: path.into(),
                reason: "missing from path".into(),
            })?;
            // The value goes back where it came from; if it replaced an object
            // member on the way, that member comes back too.
            let mut undo = vec![Delta {
                op: Operation::Move,
                path: from.into(),
                value: None,
                from_path: Some(path.into()),
            }];
            if from != path {
                if let Some(overwritten) = existing_member(document, path)? {
                    undo.push(Delta::with_value(Operation::Add, path, overwritten.clone()));
                }
            }
            undo
        }
    })
}

/// What undoes inserting at `path`: removing it, or restoring the member it replaced.
fn undo_insert(document: &Value, path: &str) -> Result<Delta, DriftError> {
    if path.is_empty() {
        return Ok(Delta::with_value(Operation::Replace, "", document.clone()));
    }
    if let Some(overwritten) = existing_member(document, path)? {
        return Ok(Delta::with_value(Operation::Replace, path, overwritten.clone()));
    }
    let (parent, token) = path.rsplit_once('/').unwrap_or(("", path));
    // `-` appends, so the inserted item lands at the current length.
    let target = match (get(document, parent)?, token) {
        (Value::Array(items), "-") => join_pointer(parent, &items.len().to_string()),
        _ => path.to_string(),
    };
    Ok(Delta::new(Operation::Remove, target))
}

/// The value at `path` if it is an object member that already exists.
fn existing_member<'a>(document: &'a Value, path: &str) -> Result<Option<&'a Value>, DriftError> {
    let Some((parent, token)) = path.rsplit_once('/') else { return Ok(None) };
    match get(document, parent)? {
        Value::Object(map) => Ok(map.get(&unescape_token(token)?)),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{diff, diff_with, patch, DiffOptions, Operation};
    use serde_json::{json, Value};

    fn add(path: &str, value: Value) -> Delta {
        Delta::with_value(Operation::Add, path, value)
    }
    fn remove(path: &str) -> Delta {
        Delta::new(Operation::Remove, path)
    }
    fn replace(path: &str, value: Value) -> Delta {
        Delta::with_value(Operation::Replace, path, value)
    }
    fn moved(from: &str, path: &str) -> Delta {
        Delta { op: Operation::Move, path: path.into(), value: None, from_path: Some(from.into()) }
    }
    fn copied(from: &str, path: &str) -> Delta {
        Delta { op: Operation::Copy, path: path.into(), value: None, from_path: Some(from.into()) }
    }

    /// Applying `operations` and then their inverse gives back `old`.
    fn assert_undoes(old: &Value, operations: &[Delta]) {
        let applied = patch(old.clone(), operations).unwrap();
        let undo = invert(old, operations).unwrap();
        let restored = patch(applied.clone(), &undo)
            .unwrap_or_else(|e| panic!("{e}: {old} --{operations:?}--> {applied} --{undo:?}"));
        assert_eq!(&restored, old, "{operations:?} then {undo:?}");
    }

    #[test]
    fn inverts_each_operation() {
        let doc = json!({"a": 1, "list": [10, 20, 30], "o": {"x": 1}});
        for operations in [
            vec![add("/new", json!(5))],
            vec![add("/a", json!("overwrites"))],
            vec![add("/list/1", json!(15))],
            vec![add("/list/-", json!(40))],
            vec![add("/list/3", json!(40))],
            vec![remove("/a")],
            vec![remove("/list/0")],
            vec![replace("/a", json!([1]))],
            vec![replace("", json!({"whole": "doc"}))],
            vec![add("", json!(7))],
            vec![moved("/a", "/b")],
            vec![moved("/list/2", "/list/0")],
            vec![moved("/list/0", "/list/2")],
            vec![moved("/o/x", "/a")],
            vec![copied("/o", "/copy")],
            vec![copied("/a", "/list/1")],
            vec![Delta::with_value(Operation::Test, "/a", json!(1))],
        ] {
            assert_undoes(&doc, &operations);
        }
    }

    #[test]
    fn inverts_a_sequence_in_reverse() {
        let doc = json!({"a": 1, "list": [1, 2, 3]});
        let operations = vec![
            add("/b", json!(2)),
            remove("/list/0"),
            replace("/a", json!(9)),
            moved("/b", "/list/1"),
            copied("/a", "/c"),
            add("/list/-", json!("end")),
        ];
        assert_undoes(&doc, &operations);
        // The inverse of the last operation comes first.
        assert_eq!(invert(&doc, &operations).unwrap()[0], remove("/list/3"));
    }

    #[test]
    fn a_failing_operation_is_reported() {
        let doc = json!({"a": 1});
        assert!(invert(&doc, &[remove("/missing")]).is_err());
        assert!(invert(&doc, &[Delta::with_value(Operation::Test, "/a", json!(2))]).is_err());
    }

    #[test]
    fn the_inverse_of_a_test_is_nothing() {
        let doc = json!({"a": 1});
        let test = Delta::with_value(Operation::Test, "/a", json!(1));
        assert!(invert(&doc, &[test]).unwrap().is_empty());
    }

    #[test]
    fn inverts_what_diff_produces() {
        let old =
            json!({"name": "a", "tags": ["x", "y", "z"], "n": {"deep": [1, {"k": 1}]}, "gone": 1});
        let new =
            json!({"name": "b", "tags": ["x"], "n": {"deep": [2, {"k": 2}, 3]}, "added": [true]});
        assert_undoes(&old, &diff(&new, &old));
    }

    #[test]
    fn inverts_keyed_array_diffs_with_moves() {
        let options = DiffOptions::new().array_key("id");
        let old = json!({"l": [{"id": 1, "v": 1}, {"id": 2, "v": 2}, {"id": 3, "v": 3}]});
        let new = json!({"l": [{"id": 3, "v": 3}, {"id": 0, "v": 0}, {"id": 1, "v": 9}]});
        let operations = diff_with(&new, &old, &options);
        assert!(operations.iter().any(|o| o.op == Operation::Move), "{operations:?}");
        assert_undoes(&old, &operations);
    }

    #[test]
    fn random_patches_are_undone() {
        // xorshift: deterministic, no extra dependencies.
        let mut state = 0x9E3779B97F4A7C15u64;
        let mut next = move |n: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % n
        };
        let options = DiffOptions::new().array_key("id");
        for _ in 0..400 {
            let doc = |next: &mut dyn FnMut(u64) -> u64| {
                let mut ids: Vec<u64> = (0..6).filter(|_| next(2) == 0).collect();
                for i in (1..ids.len()).rev() {
                    ids.swap(i, next(i as u64 + 1) as usize);
                }
                let items: Vec<Value> = ids
                    .into_iter()
                    .map(|id| json!({"id": id, "v": next(3), "t": [next(2), next(2)]}))
                    .collect();
                let mut object = json!({"list": items, "x": next(3)});
                if next(2) == 0 {
                    object["extra"] = json!({"k": next(5)});
                }
                object
            };
            let old = doc(&mut next);
            let new = doc(&mut next);
            assert_undoes(&old, &diff(&new, &old));
            assert_undoes(&old, &diff_with(&new, &old, &options));
        }
    }

    #[test]
    fn compose_gives_one_equivalent_patch() {
        let doc = json!({"a": 1, "list": [1, 2, 3]});
        let operations = vec![
            replace("/a", json!(2)),
            replace("/a", json!(3)),
            add("/tmp", json!("x")),
            remove("/tmp"),
            remove("/list/2"),
            add("/list/-", json!(9)),
        ];
        let squashed = compose(&doc, &operations, &DiffOptions::default()).unwrap();
        assert_eq!(
            patch(doc.clone(), &squashed).unwrap(),
            patch(doc.clone(), &operations).unwrap()
        );
        assert_eq!(squashed, vec![replace("/a", json!(3)), replace("/list/2", json!(9))]);
    }

    #[test]
    fn compose_of_a_patch_and_its_undo_is_empty() {
        let doc = json!({"a": [1, 2], "b": {"c": 1}});
        let operations = vec![remove("/a/0"), add("/b/d", json!(2)), replace("/b/c", json!(5))];
        let undo = invert(&doc, &operations).unwrap();
        let both: Vec<Delta> = operations.into_iter().chain(undo).collect();
        assert!(compose(&doc, &both, &DiffOptions::default()).unwrap().is_empty());
    }

    #[test]
    fn compose_fails_when_the_patch_does_not_apply() {
        assert!(compose(&json!({}), &[remove("/a")], &DiffOptions::default()).is_err());
    }
}

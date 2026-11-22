//! Three-way merge of two documents that both changed from a common base.

use crate::join_pointer;
use serde::Serialize;
use serde_json::{Map, Value};

/// The outcome of a merge.
#[derive(Debug, Clone, PartialEq)]
pub enum Merged {
    /// Both sides combined without disagreement.
    Clean(Value),
    /// The sides disagree at these places; nothing was guessed.
    Conflicted(Vec<Conflict>),
}

/// A place where the two sides changed the same thing differently.
///
/// A `None` value means the key or item is absent on that side.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Conflict {
    /// JSON Pointer to the conflicting node (empty for the document root).
    pub path: String,
    /// The value in the common base.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<Value>,
    /// The value on our side.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ours: Option<Value>,
    /// The value on their side.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theirs: Option<Value>,
}

/// Which side wins when [`MergeOptions::prefer`] resolves a conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Keep our value.
    Ours,
    /// Keep their value.
    Theirs,
}

/// Options for [`merge_with`].
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct MergeOptions {
    /// Fields that identify array items, tried in order.
    pub array_keys: Vec<String>,
    /// Resolve every conflict in favour of one side.
    pub prefer: Option<Side>,
}

impl MergeOptions {
    /// Default options: no array keys, conflicts are reported.
    pub fn new() -> Self {
        Self::default()
    }

    /// Identify array items by this field.
    pub fn array_key(mut self, key: impl Into<String>) -> Self {
        self.array_keys.push(key.into());
        self
    }

    /// Resolve every conflict in favour of `side`.
    pub fn prefer(mut self, side: Side) -> Self {
        self.prefer = Some(side);
        self
    }
}

/// Merge `ours` and `theirs`, which both descend from `base`.
pub fn merge(base: &Value, ours: &Value, theirs: &Value) -> Merged {
    merge_with(base, ours, theirs, &MergeOptions::default())
}

/// Like [`merge`], with options.
pub fn merge_with(base: &Value, ours: &Value, theirs: &Value, options: &MergeOptions) -> Merged {
    let mut ctx = Ctx { options, conflicts: Vec::new() };
    let result = node(Some(base), Some(ours), Some(theirs), "", &mut ctx);
    if ctx.conflicts.is_empty() {
        Merged::Clean(result.unwrap_or(Value::Null))
    } else {
        Merged::Conflicted(ctx.conflicts)
    }
}

struct Ctx<'a> {
    options: &'a MergeOptions,
    conflicts: Vec<Conflict>,
}

/// Merge one node; `None` means absent.
fn node(
    base: Option<&Value>,
    ours: Option<&Value>,
    theirs: Option<&Value>,
    path: &str,
    ctx: &mut Ctx,
) -> Option<Value> {
    if ours == theirs {
        return ours.cloned();
    }
    if ours == base {
        return theirs.cloned();
    }
    if theirs == base {
        return ours.cloned();
    }
    if let (Some(Value::Object(o)), Some(Value::Object(t))) = (ours, theirs) {
        let empty = Map::new();
        let b = match base {
            Some(Value::Object(b)) => b,
            _ => &empty,
        };
        return Some(Value::Object(object(b, o, t, path, ctx)));
    }
    match ctx.options.prefer {
        Some(Side::Ours) => ours.cloned(),
        Some(Side::Theirs) => theirs.cloned(),
        None => {
            ctx.conflicts.push(Conflict {
                path: path.to_string(),
                base: base.cloned(),
                ours: ours.cloned(),
                theirs: theirs.cloned(),
            });
            None
        }
    }
}

/// Merge objects key by key: base order, then keys new in ours, then in theirs.
fn object(
    base: &Map<String, Value>,
    ours: &Map<String, Value>,
    theirs: &Map<String, Value>,
    path: &str,
    ctx: &mut Ctx,
) -> Map<String, Value> {
    let keys = base
        .keys()
        .chain(ours.keys().filter(|k| !base.contains_key(*k)))
        .chain(theirs.keys().filter(|k| !base.contains_key(*k) && !ours.contains_key(*k)));
    let mut out = Map::new();
    for key in keys {
        let child = join_pointer(path, key);
        let merged = node(base.get(key), ours.get(key), theirs.get(key), &child, ctx);
        if let Some(value) = merged {
            out.insert(key.clone(), value);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn conflict(path: &str, b: Option<Value>, o: Option<Value>, t: Option<Value>) -> Conflict {
        Conflict { path: path.into(), base: b, ours: o, theirs: t }
    }

    #[test]
    fn identical_sides_win() {
        let m = merge(&json!({"a": 1}), &json!({"a": 2}), &json!({"a": 2}));
        assert_eq!(m, Merged::Clean(json!({"a": 2})));
    }

    #[test]
    fn unchanged_side_yields_other() {
        let b = json!({"a": 1});
        let x = json!({"a": 5});
        assert_eq!(merge(&b, &b, &x), Merged::Clean(x.clone()));
        assert_eq!(merge(&b, &x, &b), Merged::Clean(x));
    }

    #[test]
    fn disjoint_keys_merge() {
        let m = merge(&json!({"a": 1, "b": 1}), &json!({"a": 2, "b": 1}), &json!({"a": 1, "b": 3}));
        assert_eq!(m, Merged::Clean(json!({"a": 2, "b": 3})));
    }

    #[test]
    fn same_leaf_different_values_conflicts() {
        let m = merge(&json!({"a": 1}), &json!({"a": 2}), &json!({"a": 3}));
        assert_eq!(
            m,
            Merged::Conflicted(vec![conflict(
                "/a",
                Some(json!(1)),
                Some(json!(2)),
                Some(json!(3))
            )])
        );
    }

    #[test]
    fn delete_vs_modify_conflicts() {
        let m = merge(&json!({"a": 1}), &json!({}), &json!({"a": 2}));
        assert_eq!(
            m,
            Merged::Conflicted(vec![conflict("/a", Some(json!(1)), None, Some(json!(2)))])
        );
    }

    #[test]
    fn both_delete_is_clean() {
        let m = merge(&json!({"a": 1, "b": 2}), &json!({"b": 2}), &json!({"b": 2}));
        assert_eq!(m, Merged::Clean(json!({"b": 2})));
        let m = merge(&json!({"a": 1, "b": 2}), &json!({"b": 3}), &json!({"b": 2}));
        assert_eq!(m, Merged::Clean(json!({"b": 3})));
    }

    #[test]
    fn both_add_same_key_recurses() {
        let m = merge(&json!({}), &json!({"k": {"x": 1}}), &json!({"k": {"y": 2}}));
        assert_eq!(m, Merged::Clean(json!({"k": {"x": 1, "y": 2}})));
        let m = merge(&json!({}), &json!({"k": {"x": 1}}), &json!({"k": {"x": 1}}));
        assert_eq!(m, Merged::Clean(json!({"k": {"x": 1}})));
    }

    #[test]
    fn object_key_order() {
        let m = merge(
            &json!({"b": 1, "a": 1}),
            &json!({"b": 1, "a": 1, "o2": 1, "o1": 1}),
            &json!({"b": 1, "a": 1, "t": 1}),
        );
        let Merged::Clean(v) = m else { panic!("conflict") };
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["b", "a", "o2", "o1", "t"]);
    }

    #[test]
    fn type_change_conflicts_at_root() {
        let m = merge(&json!({"a": 1}), &json!([1]), &json!({"a": 2}));
        assert_eq!(
            m,
            Merged::Conflicted(vec![conflict(
                "",
                Some(json!({"a": 1})),
                Some(json!([1])),
                Some(json!({"a": 2}))
            )])
        );
    }

    #[test]
    fn pointer_escaping() {
        let m = merge(&json!({"a/b~": 1}), &json!({"a/b~": 2}), &json!({"a/b~": 3}));
        let Merged::Conflicted(c) = m else { panic!("clean") };
        assert_eq!(c[0].path, "/a~1b~0");
    }

    #[test]
    fn prefer_resolves_every_conflict() {
        let theirs = MergeOptions::new().prefer(Side::Theirs);
        let ours = MergeOptions::new().prefer(Side::Ours);
        let m = merge_with(&json!({"a": 1}), &json!({}), &json!({"a": 2}), &theirs);
        assert_eq!(m, Merged::Clean(json!({"a": 2})));
        let m = merge_with(&json!({"a": 1}), &json!({"a": 2}), &json!({"a": 3}), &theirs);
        assert_eq!(m, Merged::Clean(json!({"a": 3})));
        let m = merge_with(&json!({"a": 1}), &json!({}), &json!({"a": 2}), &ours);
        assert_eq!(m, Merged::Clean(json!({})));
        let m = merge_with(&json!({"a": 1}), &json!([1]), &json!({"a": 2}), &ours);
        assert_eq!(m, Merged::Clean(json!([1])));
    }

    #[test]
    fn unkeyed_arrays_changed_by_both_conflict_whole_array() {
        let m = merge(&json!({"l": [1]}), &json!({"l": [1, 2]}), &json!({"l": [0, 1]}));
        assert_eq!(
            m,
            Merged::Conflicted(vec![conflict(
                "/l",
                Some(json!([1])),
                Some(json!([1, 2])),
                Some(json!([0, 1]))
            )])
        );
    }
}

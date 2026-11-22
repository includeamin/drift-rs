//! Three-way merge of two documents that both changed from a common base.

use crate::diff::identity;
use crate::join_pointer;
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::{BTreeSet, HashMap, HashSet};

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
    ///
    /// Inside an array merged by key, the index is the item's index in the
    /// base (for an item not in the base, its index in ours, else in theirs),
    /// so it may not point at the item in ours or theirs.
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
///
/// ```
/// use drift::{merge, Merged};
/// use serde_json::json;
///
/// let base = json!({"port": 80, "debug": false});
///
/// // Different keys changed: both edits are kept.
/// let clean = merge(&base, &json!({"port": 8080, "debug": false}), &json!({"port": 80, "debug": true}));
/// assert_eq!(clean, Merged::Clean(json!({"port": 8080, "debug": true})));
///
/// // The same key changed differently: a conflict.
/// let conflicted = merge(&base, &json!({"port": 8080, "debug": false}), &json!({"port": 9090, "debug": false}));
/// match conflicted {
///     Merged::Conflicted(conflicts) => {
///         assert_eq!(conflicts.len(), 1);
///         assert_eq!(conflicts[0].path, "/port");
///     }
///     Merged::Clean(_) => unreachable!(),
/// }
/// ```
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
    if let (Some(Value::Array(b)), Some(Value::Array(o)), Some(Value::Array(t))) =
        (base, ours, theirs)
    {
        if let Some(key) = usable_key([b, o, t], &ctx.options.array_keys) {
            let key = key.to_string();
            return keyed_array(b, o, t, &key, path, ctx);
        }
    }
    conflict(base, ours, theirs, path, ctx)
}

/// Resolve a disagreement by `prefer`, or record it.
fn conflict(
    base: Option<&Value>,
    ours: Option<&Value>,
    theirs: Option<&Value>,
    path: &str,
    ctx: &mut Ctx,
) -> Option<Value> {
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

/// The first key identifying every item of all three arrays, uniquely within
/// each array. Empty arrays qualify.
fn usable_key<'a>(arrays: [&[Value]; 3], keys: &'a [String]) -> Option<&'a str> {
    keys.iter().map(String::as_str).find(|key| {
        arrays.iter().all(|items| {
            let ids: Option<Vec<String>> = items.iter().map(|i| identity(i, key)).collect();
            ids.is_some_and(|ids| ids.iter().collect::<BTreeSet<_>>().len() == ids.len())
        })
    })
}

/// Merge arrays item by item, matching items by `key`.
fn keyed_array(
    base: &[Value],
    ours: &[Value],
    theirs: &[Value],
    key: &str,
    path: &str,
    ctx: &mut Ctx,
) -> Option<Value> {
    let ids = |items: &[Value]| -> Vec<String> {
        items.iter().filter_map(|i| identity(i, key)).collect()
    };
    let (b_ids, o_ids, t_ids) = (ids(base), ids(ours), ids(theirs));
    let b_set: HashSet<&String> = b_ids.iter().collect();
    let o_set: HashSet<&String> = o_ids.iter().collect();
    let t_set: HashSet<&String> = t_ids.iter().collect();

    // Order of the items a side shares with another array.
    let shared = |list: &[String], other: &HashSet<&String>| -> Vec<String> {
        list.iter().filter(|i| other.contains(i)).cloned().collect()
    };
    let position = |ids: &[String]| -> HashMap<String, usize> {
        ids.iter().cloned().enumerate().map(|(i, id)| (id, i)).collect()
    };
    let (b_pos, o_pos, t_pos) = (position(&b_ids), position(&o_ids), position(&t_ids));

    // The whole array conflicts when the sides disagree on where items go:
    // - both reordered the base items, and differently; or
    // - an id both sides added (absent from base) sits before some item both
    //   sides have on one side and after it on the other. Only a reorder of
    //   base items by a single side may explain a difference in order, so the
    //   result never depends on which side is ours.
    let o_reordered = shared(&o_ids, &b_set) != shared(&b_ids, &o_set);
    let t_reordered = shared(&t_ids, &b_set) != shared(&b_ids, &t_set);
    let common = shared(&o_ids, &t_set);
    let added_apart = common.iter().filter(|id| !b_set.contains(id)).any(|added| {
        common.iter().any(|other| (o_pos[added] < o_pos[other]) != (t_pos[added] < t_pos[other]))
    });
    if added_apart || (o_reordered && t_reordered && common != shared(&t_ids, &o_set)) {
        return conflict(
            Some(&Value::Array(base.to_vec())),
            Some(&Value::Array(ours.to_vec())),
            Some(&Value::Array(theirs.to_vec())),
            path,
            ctx,
        );
    }

    // Merge every item present anywhere: base order, then new in ours, then theirs.
    let all = b_ids
        .iter()
        .chain(o_ids.iter().filter(|i| !b_set.contains(i)))
        .chain(t_ids.iter().filter(|i| !b_set.contains(i) && !o_set.contains(i)));
    let mut merged: HashMap<&String, Value> = HashMap::new();
    for id in all {
        let (b, o, t) = (
            b_pos.get(id).map(|&i| &base[i]),
            o_pos.get(id).map(|&i| &ours[i]),
            t_pos.get(id).map(|&i| &theirs[i]),
        );
        let index = b_pos.get(id).or_else(|| o_pos.get(id)).or_else(|| t_pos.get(id)).copied();
        let child = join_pointer(path, &index.unwrap_or(0).to_string());
        if let Some(value) = node(b, o, t, &child, ctx) {
            merged.insert(id, value);
        }
    }

    // The reordering side gives the skeleton; ours when neither reordered.
    let (skeleton_ids, other_ids, other_set) = if t_reordered && !o_reordered {
        (&t_ids, &o_ids, &o_set)
    } else {
        (&o_ids, &t_ids, &t_set)
    };
    let skeleton: Vec<&String> = skeleton_ids.iter().filter(|i| merged.contains_key(i)).collect();

    // Items the other side contributes that the skeleton lacks (its additions,
    // or an item the skeleton side deleted but `prefer` kept), grouped under
    // their nearest preceding kept item of that side. With no preceding item
    // they go at the FRONT.
    let skeleton_set: HashSet<&&String> = skeleton.iter().collect();
    let mut runs: HashMap<Option<&String>, Vec<&String>> = HashMap::new();
    let mut anchor: Option<&String> = None;
    for id in other_ids {
        if skeleton_set.contains(&id) {
            anchor = Some(id);
        } else if merged.contains_key(id) {
            runs.entry(anchor).or_default().push(id);
        }
    }

    let mut out: Vec<Value> = Vec::new();
    let mut at = 0;
    let mut anchor: Option<&String> = None;
    loop {
        let own_start = at;
        while at < skeleton.len() && !other_set.contains(skeleton[at]) {
            at += 1;
        }
        let own = &skeleton[own_start..at];
        let theirs_run = runs.remove(&anchor).unwrap_or_default();
        // Both sides added here: the smaller identity goes first.
        let other_first = match (own.first(), theirs_run.first()) {
            (Some(a), Some(b)) => b < a,
            _ => true,
        };
        let (first, second): (&[&String], &[&String]) =
            if other_first { (&theirs_run, own) } else { (own, &theirs_run) };
        out.extend(first.iter().chain(second.iter()).filter_map(|id| merged.remove(*id)));
        if at >= skeleton.len() {
            break;
        }
        anchor = Some(skeleton[at]);
        out.extend(merged.remove(skeleton[at]));
        at += 1;
    }
    Some(Value::Array(out))
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

    fn keyed() -> MergeOptions {
        MergeOptions::new().array_key("id")
    }

    fn items(ids: &[i64]) -> Value {
        Value::Array(ids.iter().map(|i| json!({"id": i, "v": 0})).collect())
    }

    fn ids_of(m: &Merged) -> Vec<i64> {
        let Merged::Clean(v) = m else { panic!("conflict: {m:?}") };
        v["items"].as_array().unwrap().iter().map(|i| i["id"].as_i64().unwrap()).collect()
    }

    fn doc(items: Value) -> Value {
        json!({ "items": items })
    }

    #[test]
    fn keyed_disjoint_item_edits_merge() {
        let b = doc(json!([{"id":1,"v":0},{"id":2,"v":0}]));
        let o = doc(json!([{"id":1,"v":5},{"id":2,"v":0}]));
        let t = doc(json!([{"id":1,"v":0},{"id":2,"v":7}]));
        let m = merge_with(&b, &o, &t, &keyed());
        assert_eq!(m, Merged::Clean(doc(json!([{"id":1,"v":5},{"id":2,"v":7}]))));
    }

    #[test]
    fn keyed_inserts_on_both_sides_keep_both() {
        let b = doc(items(&[1, 2]));
        let o = doc(items(&[1, 2, 3]));
        let t = doc(items(&[0, 1, 2]));
        assert_eq!(ids_of(&merge_with(&b, &o, &t, &keyed())), [0, 1, 2, 3]);
    }

    #[test]
    fn keyed_same_anchor_inserts_commute() {
        let b = doc(items(&[1, 2]));
        let o = doc(items(&[1, 9, 2]));
        let t = doc(items(&[1, 5, 2]));
        let a = merge_with(&b, &o, &t, &keyed());
        let c = merge_with(&b, &t, &o, &keyed());
        assert_eq!(a, c);
        assert_eq!(ids_of(&a), [1, 5, 9, 2]);
    }

    #[test]
    fn keyed_delete_vs_modify_conflicts_at_item() {
        let b = doc(json!([{"id":1,"v":0},{"id":2,"v":0}]));
        let o = doc(json!([{"id":1,"v":0}]));
        let t = doc(json!([{"id":1,"v":0},{"id":2,"v":9}]));
        let m = merge_with(&b, &o, &t, &keyed());
        assert_eq!(
            m,
            Merged::Conflicted(vec![conflict(
                "/items/1",
                Some(json!({"id":2,"v":0})),
                None,
                Some(json!({"id":2,"v":9}))
            )])
        );
    }

    #[test]
    fn keyed_both_delete_is_clean() {
        let b = doc(items(&[1, 2]));
        let o = doc(items(&[2]));
        let m = merge_with(&b, &o, &o, &keyed());
        assert_eq!(ids_of(&m), [2]);
    }

    #[test]
    fn keyed_one_side_deletes_unchanged_item() {
        let b = doc(items(&[1, 2]));
        let o = doc(items(&[2]));
        assert_eq!(ids_of(&merge_with(&b, &o, &b, &keyed())), [2]);
        assert_eq!(ids_of(&merge_with(&b, &b, &o, &keyed())), [2]);
    }

    #[test]
    fn keyed_one_side_reorders() {
        let b = doc(json!([{"id":1,"v":0},{"id":2,"v":0}]));
        let o = doc(json!([{"id":2,"v":0},{"id":1,"v":0}]));
        let t = doc(json!([{"id":1,"v":4},{"id":2,"v":0}]));
        let want = Merged::Clean(doc(json!([{"id":2,"v":0},{"id":1,"v":4}])));
        assert_eq!(merge_with(&b, &o, &t, &keyed()), want);
        assert_eq!(merge_with(&b, &t, &o, &keyed()), want);
    }

    #[test]
    fn keyed_both_reorder_differently_conflicts_on_array() {
        let b = doc(items(&[1, 2, 3]));
        let o = doc(items(&[2, 1, 3]));
        let t = doc(items(&[3, 2, 1]));
        let m = merge_with(&b, &o, &t, &keyed());
        assert_eq!(
            m,
            Merged::Conflicted(vec![conflict(
                "/items",
                Some(items(&[1, 2, 3])),
                Some(items(&[2, 1, 3])),
                Some(items(&[3, 2, 1]))
            )])
        );
        let p = keyed().prefer(Side::Theirs);
        assert_eq!(ids_of(&merge_with(&b, &o, &t, &p)), [3, 2, 1]);
    }

    #[test]
    fn keyed_falls_back_when_key_not_unique() {
        let b = doc(items(&[1, 2]));
        let o = doc(json!([{"id":1,"v":0},{"id":1,"v":1}]));
        let t = doc(items(&[0, 1, 2]));
        let m = merge_with(&b, &o, &t, &keyed());
        assert_eq!(
            m,
            Merged::Conflicted(vec![conflict(
                "/items",
                Some(items(&[1, 2])),
                Some(o.clone()["items"].clone()),
                Some(items(&[0, 1, 2]))
            )])
        );
    }

    fn val_items(vals: &[(i64, i64)]) -> Value {
        doc(Value::Array(vals.iter().map(|(i, v)| json!({"id": i, "v": v})).collect()))
    }

    #[test]
    fn keyed_prefer_theirs_keeps_modified_item_ours_deleted() {
        let b = val_items(&[(1, 0), (2, 0)]);
        let o = val_items(&[(1, 0)]);
        let t = val_items(&[(1, 0), (2, 9)]);
        let p = keyed().prefer(Side::Theirs);
        assert_eq!(merge_with(&b, &o, &t, &p), Merged::Clean(val_items(&[(1, 0), (2, 9)])));
    }

    #[test]
    fn keyed_prefer_ours_keeps_modified_item_theirs_deleted() {
        let b = val_items(&[(1, 0), (2, 0)]);
        let o = val_items(&[(1, 0), (2, 9)]);
        let t = val_items(&[(1, 0)]);
        let p = keyed().prefer(Side::Ours);
        assert_eq!(merge_with(&b, &o, &t, &p), Merged::Clean(val_items(&[(1, 0), (2, 9)])));
    }

    #[test]
    fn keyed_prefer_ours_removes_item_ours_deleted() {
        let b = val_items(&[(1, 0), (2, 0)]);
        let o = val_items(&[(1, 0)]);
        let t = val_items(&[(1, 0), (2, 9)]);
        let p = keyed().prefer(Side::Ours);
        assert_eq!(merge_with(&b, &o, &t, &p), Merged::Clean(val_items(&[(1, 0)])));
    }

    #[test]
    fn keyed_additions_interleave_with_a_reorder() {
        let b = doc(items(&[1, 2, 3]));
        let o = doc(items(&[3, 2, 1]));
        let t = doc(items(&[1, 2, 9, 3]));
        assert_eq!(ids_of(&merge_with(&b, &o, &t, &keyed())), [3, 2, 9, 1]);
        assert_eq!(ids_of(&merge_with(&b, &t, &o, &keyed())), [3, 2, 9, 1]);
    }

    #[test]
    fn keyed_same_id_added_at_different_places_conflicts() {
        let cases = [
            (items(&[1, 2]), items(&[1, 2, 3]), items(&[3, 1, 2])),
            (items(&[]), items(&[1, 2]), items(&[2, 1])),
        ];
        for (b, o, t) in cases {
            let (b, o, t) = (doc(b), doc(o), doc(t));
            for (x, y) in [(&o, &t), (&t, &o)] {
                let m = merge_with(&b, x, y, &keyed());
                let Merged::Conflicted(c) = &m else { panic!("clean: {m:?}") };
                assert_eq!(c.len(), 1);
                assert_eq!(c[0].path, "/items");
            }
            let ours = merge_with(&b, &o, &t, &keyed().prefer(Side::Ours));
            let theirs = merge_with(&b, &t, &o, &keyed().prefer(Side::Theirs));
            assert_eq!(ours, theirs);
            assert_eq!(ours, Merged::Clean(o.clone()));
        }
    }

    #[test]
    fn keyed_same_id_added_at_same_place_is_clean() {
        let b = doc(items(&[1, 2]));
        let o = doc(items(&[1, 3, 2, 4]));
        let t = doc(items(&[1, 3, 2]));
        assert_eq!(ids_of(&merge_with(&b, &o, &t, &keyed())), [1, 3, 2, 4]);
        assert_eq!(ids_of(&merge_with(&b, &t, &o, &keyed())), [1, 3, 2, 4]);
        // One side's reorder of base items is not a disagreement about the
        // added item when it stays on the same side of every shared item.
        let o = doc(items(&[2, 1, 3]));
        let t = doc(items(&[1, 2, 3]));
        assert_eq!(ids_of(&merge_with(&b, &o, &t, &keyed())), [2, 1, 3]);
        assert_eq!(ids_of(&merge_with(&b, &t, &o, &keyed())), [2, 1, 3]);
    }

    #[test]
    fn keyed_anchor_deleted_by_other_side() {
        let b = doc(items(&[1, 2, 3]));
        let o = doc(items(&[1, 3]));
        let t = doc(items(&[1, 2, 5, 3]));
        assert_eq!(ids_of(&merge_with(&b, &o, &t, &keyed())), [1, 5, 3]);
        assert_eq!(ids_of(&merge_with(&b, &t, &o, &keyed())), [1, 5, 3]);
    }
}

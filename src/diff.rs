use crate::search::glob_match;
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
///
/// Build one with [`DiffOptions::new`] and the methods below; the struct is
/// `#[non_exhaustive]` so new options can be added without breaking callers.
///
/// ```
/// use drift::DiffOptions;
///
/// let options = DiffOptions::new().array_key("id").ignore_path("/**/updatedAt");
/// assert_eq!(options.array_keys, ["id"]);
/// ```
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct DiffOptions {
    /// Field names that identify array items, tried in order.
    ///
    /// For an array whose items are all objects with a unique scalar value for
    /// one of these fields, items are matched by that value instead of by
    /// position. Inserting or removing an item then yields one operation
    /// instead of a run of replacements, and reordering yields `move`
    /// operations. Arrays without a usable key are compared by position.
    pub array_keys: Vec<String>,
    /// JSON Pointer patterns for parts of the document to leave out of the diff.
    ///
    /// `*` matches one token and `**` any number, as in
    /// [`path_matches`](crate::path_matches): `/updatedAt` ignores one member,
    /// `/users/*/lastSeen` a member of every user, and `/**/id` every `id` at
    /// any depth. Ignored parts are skipped, not filtered afterwards, so
    /// nothing under them is compared; whether they were added, removed or
    /// changed is not reported. The patched document keeps the old value.
    ///
    /// Adding or removing an array *item* is always reported, even if a
    /// pattern covers it, because skipping it would shift the indices of the
    /// operations after it. Such a pattern ignores changes *inside* the item.
    ///
    /// A pattern that is not a JSON Pointer (it must be empty or start with
    /// `/`) matches nothing. The empty pattern ignores the whole document.
    pub ignore_paths: Vec<String>,
}

impl DiffOptions {
    /// No options: positional arrays, nothing ignored.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a field that identifies array items; see [`array_keys`](Self::array_keys).
    pub fn array_key(mut self, name: impl Into<String>) -> Self {
        self.array_keys.push(name.into());
        self
    }

    /// Adds a pattern to leave out of the diff; see [`ignore_paths`](Self::ignore_paths).
    pub fn ignore_path(mut self, pattern: impl Into<String>) -> Self {
        self.ignore_paths.push(pattern.into());
        self
    }

    /// Whether `path` (a JSON Pointer) is covered by an ignore pattern.
    ///
    /// For callers that walk a document alongside [`diff_with`] and need to
    /// skip the same parts. It re-reads the patterns on every call, so it is
    /// meant for display code, not hot loops.
    pub fn ignores(&self, path: &str) -> bool {
        Context::new(self).ignored(path)
    }
}

const POSITIONAL: DiffOptions = DiffOptions { array_keys: Vec::new(), ignore_paths: Vec::new() };

/// The options plus the ignore patterns split into tokens once, not per node.
struct Context<'a> {
    options: &'a DiffOptions,
    ignore: Vec<Vec<&'a str>>,
}

impl<'a> Context<'a> {
    fn new(options: &'a DiffOptions) -> Self {
        let ignore = options
            .ignore_paths
            .iter()
            .filter_map(|pattern| match pattern.strip_prefix('/') {
                Some(tokens) => Some(tokens.split('/').collect()),
                None if pattern.is_empty() => Some(Vec::new()),
                None => None,
            })
            .collect();
        Self { options, ignore }
    }

    fn ignored(&self, path: &str) -> bool {
        if self.ignore.is_empty() {
            return false;
        }
        let tokens: Vec<&str> =
            path.strip_prefix('/').map_or_else(Vec::new, |p| p.split('/').collect());
        self.ignore.iter().any(|pattern| glob_match(pattern, &tokens))
    }
}

pub(crate) fn diff_inner(new: &Value, old: &Value, path: &str, output: &mut Vec<Delta>) {
    walk(new, old, path, &Context::new(&POSITIONAL), output);
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
    ctx: &Context,
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
                walk(item, &old[old_index], &target, ctx, output);
            }
            None => {
                output.push(Delta::with_value(Operation::Add, target, item.clone()));
                current.insert(index, None);
            }
        }
    }
}

fn walk(new: &Value, old: &Value, path: &str, ctx: &Context, output: &mut Vec<Delta>) {
    if ctx.ignored(path) {
        return;
    }
    match (new, old) {
        (Value::Object(new_obj), Value::Object(old_obj)) => {
            let old_keys: BTreeSet<_> = old_obj.keys().collect();
            let new_keys: BTreeSet<_> = new_obj.keys().collect();
            for key in old_keys.difference(&new_keys) {
                let member = join_pointer(path, key);
                if !ctx.ignored(&member) {
                    output.push(Delta::new(Operation::Remove, member));
                }
            }
            for key in new_keys.difference(&old_keys) {
                let member = join_pointer(path, key);
                if !ctx.ignored(&member) {
                    output.push(Delta::with_value(Operation::Add, member, new_obj[*key].clone()));
                }
            }
            for key in old_keys.intersection(&new_keys) {
                walk(&new_obj[*key], &old_obj[*key], &join_pointer(path, key), ctx, output);
            }
        }
        (Value::Array(new_items), Value::Array(old_items)) => {
            if let Some(matches) = match_array_items(new_items, old_items, &ctx.options.array_keys)
            {
                return keyed_array(new_items, old_items, &matches, path, ctx, output);
            }
            for (index, (new_value, old_value)) in new_items.iter().zip(old_items).enumerate() {
                walk(new_value, old_value, &join_pointer(path, &index.to_string()), ctx, output);
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
/// let options = DiffOptions::new().array_key("id");
/// assert_eq!(diff_with(&new, &old, &options).len(), 1);
/// ```
pub fn diff_with(new: &Value, old: &Value, options: &DiffOptions) -> Vec<Delta> {
    let mut output = Vec::new();
    walk(new, old, "", &Context::new(options), &mut output);
    output
}

#[cfg(test)]
mod keyed_tests {
    use super::*;
    use crate::patch;
    use serde_json::json;

    fn keyed(names: &[&str]) -> DiffOptions {
        names.iter().fold(DiffOptions::new(), |options, name| options.array_key(*name))
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

#[cfg(test)]
mod ignore_tests {
    use super::*;
    use crate::{patch, path_matches};
    use serde_json::json;

    fn ignoring(patterns: &[&str]) -> DiffOptions {
        patterns.iter().fold(DiffOptions::new(), |options, p| options.ignore_path(*p))
    }

    fn paths(new: &Value, old: &Value, options: &DiffOptions) -> Vec<String> {
        diff_with(new, old, options).into_iter().map(|d| d.path).collect()
    }

    #[test]
    fn ignored_members_are_not_reported_whether_changed_added_or_removed() {
        let old = json!({"a": 1, "stamp": 1, "gone": 1});
        let new = json!({"a": 2, "stamp": 9, "fresh": 1});
        assert_eq!(paths(&new, &old, &ignoring(&["/stamp"])), ["/gone", "/fresh", "/a"]);
        assert_eq!(paths(&new, &old, &ignoring(&["/stamp", "/gone", "/fresh"])), ["/a"]);
    }

    #[test]
    fn an_ignored_subtree_is_skipped_whole() {
        let old = json!({"meta": {"a": 1, "deep": {"b": [1, 2]}}, "x": 1});
        let new = json!({"meta": {"a": 2, "deep": {"b": [9]}, "c": 3}, "x": 2});
        assert_eq!(paths(&new, &old, &ignoring(&["/meta"])), ["/x"]);
    }

    #[test]
    fn wildcards_reach_into_arrays_and_any_depth() {
        let old = json!({"items": [{"id": 1, "seen": 1}, {"id": 2, "seen": 1}], "n": {"seen": 1, "k": 1}});
        let new = json!({"items": [{"id": 1, "seen": 5}, {"id": 3, "seen": 6}], "n": {"seen": 2, "k": 2}});
        assert_eq!(
            paths(&new, &old, &ignoring(&["/items/*/seen", "/n/seen"])),
            ["/items/1/id", "/n/k"]
        );
        assert_eq!(paths(&new, &old, &ignoring(&["/**/seen"])), ["/items/1/id", "/n/k"]);
    }

    #[test]
    fn array_items_are_still_added_and_removed_because_indices_would_shift() {
        let old = json!({"xs": [1, 2, 3]});
        let new = json!({"xs": [1, 9]});
        // The pattern covers every item, so changes inside items are skipped...
        assert_eq!(paths(&new, &old, &ignoring(&["/xs/*"])), ["/xs/2"]);
        // ...but dropping the third item must still be reported, and patching works.
        let operations = diff_with(&new, &old, &ignoring(&["/xs/*"]));
        assert_eq!(patch(old, &operations).unwrap(), json!({"xs": [1, 2]}));
    }

    #[test]
    fn ignoring_composes_with_keyed_arrays() {
        let old = json!({"u": [{"id": 1, "seen": 1, "n": "a"}, {"id": 2, "seen": 1, "n": "b"}]});
        let new = json!({"u": [{"id": 2, "seen": 7, "n": "b"}, {"id": 1, "seen": 8, "n": "A"}]});
        let options = DiffOptions::new().array_key("id").ignore_path("/u/*/seen");
        let operations = diff_with(&new, &old, &options);
        assert!(operations.iter().all(|d| !d.path.ends_with("/seen")), "{operations:?}");
        let patched = patch(old, &operations).unwrap();
        assert_eq!(patched["u"][0]["n"], "b");
        assert_eq!(patched["u"][1]["n"], "A");
    }

    #[test]
    fn the_empty_pattern_ignores_the_whole_document() {
        assert!(diff_with(&json!({"a": 2}), &json!({"a": 1}), &ignoring(&[""])).is_empty());
    }

    #[test]
    fn keys_with_slashes_use_pointer_escapes() {
        let old = json!({"a/b": 1, "a": {"b": 1}});
        let new = json!({"a/b": 2, "a": {"b": 2}});
        assert_eq!(paths(&new, &old, &ignoring(&["/a~1b"])), ["/a/b"]);
    }

    #[test]
    fn a_pattern_that_is_not_a_pointer_matches_nothing() {
        let (old, new) = (json!({"stamp": 1}), json!({"stamp": 2}));
        assert_eq!(paths(&new, &old, &ignoring(&["stamp"])), ["/stamp"]);
    }

    #[test]
    fn no_patterns_means_the_plain_diff() {
        let old = json!({"a": [1, {"b": 2}], "c": 1});
        let new = json!({"a": [1, {"b": 3}, 4], "d": 1});
        assert_eq!(diff_with(&new, &old, &DiffOptions::new()), diff(&new, &old));
    }

    #[test]
    fn matches_the_plain_diff_minus_ignored_paths_on_random_objects() {
        // xorshift: deterministic, no extra dependencies.
        let mut state = 0xA24BAED4963EE407u64;
        let mut next = move |n: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % n
        };
        fn object(depth: usize, next: &mut dyn FnMut(u64) -> u64) -> Value {
            let mut map = serde_json::Map::new();
            for key in ["a", "b", "c", "d"] {
                if next(3) == 0 {
                    continue;
                }
                let value = match next(3) {
                    0 if depth > 0 => object(depth - 1, next),
                    1 => json!(next(3)),
                    _ => json!(["x", next(2)]),
                };
                map.insert(key.to_string(), value);
            }
            Value::Object(map)
        }
        let all = ["/a", "/b/c", "/*/d", "/**/a", "/c/**", "/**/b/c"];
        for _ in 0..600 {
            let (old, new) = (object(3, &mut next), object(3, &mut next));
            let chosen: Vec<&str> = all.iter().copied().filter(|_| next(3) == 0).collect();
            let options = ignoring(&chosen);
            // An operation is dropped when its path, or an ancestor of it, is ignored.
            let ignored = |path: &str| {
                let mut prefix = String::new();
                let tokens: Vec<&str> = path.split('/').skip(1).collect();
                std::iter::once(String::new())
                    .chain(tokens.iter().map(|t| {
                        prefix = format!("{prefix}/{t}");
                        prefix.clone()
                    }))
                    .any(|p| chosen.iter().any(|pattern| path_matches(pattern, &p).unwrap()))
            };
            let expected: Vec<Delta> =
                diff(&new, &old).into_iter().filter(|d| !ignored(&d.path)).collect();
            assert_eq!(
                diff_with(&new, &old, &options),
                expected,
                "{old} -> {new} ignoring {chosen:?}"
            );
        }
    }
}

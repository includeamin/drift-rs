//! Merged view of two documents, annotated with what changed.
//!
//! The walk mirrors [`drift::diff`]: objects are matched by key and arrays by
//! index. Each changed node corresponds to exactly one RFC 6902 operation, which
//! the tests check.

use drift::{join_pointer, match_array_items, DiffOptions};
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::HashSet;

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Same,
    Added,
    Removed,
    Changed,
    /// Unchanged itself, but something below it changed.
    Nested,
    /// Covered by an ignore pattern, so left out of the diff whatever differs.
    Ignored,
}

#[derive(Serialize, Debug)]
pub struct Node {
    /// Object key or array index; `None` for the root.
    pub key: Option<String>,
    pub status: Status,
    /// Value in the old document (removed, changed and unchanged leaves).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old: Option<Value>,
    /// Value in the new document (added, changed and unchanged leaves).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new: Option<Value>,
    /// Index in the old array, for key-matched items (the old side shows this).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_index: Option<usize>,
    /// Index in the old array of a key-matched item that changed position.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moved_from: Option<usize>,
    /// "object" or "array" for nested nodes, so the UI can draw brackets.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Node>,
}

impl Node {
    fn leaf(key: Option<String>, status: Status, old: Option<&Value>, new: Option<&Value>) -> Self {
        Self {
            key,
            status,
            old: old.cloned(),
            new: new.cloned(),
            container: None,
            old_index: None,
            moved_from: None,
            children: Vec::new(),
        }
    }
}

/// `options` must be the ones given to [`drift::diff_with`], so the tree pairs
/// array items and skips ignored parts exactly like the operations do.
pub fn build(old: &Value, new: &Value, options: &DiffOptions) -> Node {
    node(None, "", old, new, options)
}

fn node(key: Option<String>, path: &str, old: &Value, new: &Value, options: &DiffOptions) -> Node {
    if old == new {
        // Unchanged subtrees stay collapsed: one value, no children.
        return Node::leaf(key, Status::Same, None, Some(new));
    }
    if options.ignores(path) {
        return Node::leaf(key, Status::Ignored, Some(old), Some(new));
    }
    let keys = &options.array_keys[..];
    let container = if new.is_array() { "array" } else { "object" };
    let children = match (old, new) {
        (Value::Object(o), Value::Object(n)) => merged_keys(o, n)
            .into_iter()
            .map(|k| {
                let member = join_pointer(path, k);
                match (o.get(k), n.get(k)) {
                    (Some(a), Some(b)) => node(Some(k.clone()), &member, a, b, options),
                    // Added or removed members under an ignore pattern are not reported.
                    (Some(a), None) if options.ignores(&member) => {
                        Node::leaf(Some(k.clone()), Status::Ignored, Some(a), None)
                    }
                    (None, Some(b)) if options.ignores(&member) => {
                        Node::leaf(Some(k.clone()), Status::Ignored, None, Some(b))
                    }
                    (Some(a), None) => Node::leaf(Some(k.clone()), Status::Removed, Some(a), None),
                    (None, Some(b)) => Node::leaf(Some(k.clone()), Status::Added, None, Some(b)),
                    (None, None) => unreachable!(),
                }
            })
            .collect(),
        (Value::Array(o), Value::Array(n)) => match match_array_items(n, o, keys) {
            Some(matches) => keyed_children(o, n, &matches, path, options),
            None => (0..o.len().max(n.len()))
                .map(|i| {
                    let key = Some(i.to_string());
                    let item = join_pointer(path, &i.to_string());
                    match (o.get(i), n.get(i)) {
                        (Some(a), Some(b)) => node(key, &item, a, b, options),
                        (Some(a), None) => Node::leaf(key, Status::Removed, Some(a), None),
                        (None, Some(b)) => Node::leaf(key, Status::Added, None, Some(b)),
                        (None, None) => unreachable!(),
                    }
                })
                .collect(),
        },
        _ => return Node::leaf(key, Status::Changed, Some(old), Some(new)),
    };
    Node {
        key,
        status: Status::Nested,
        old: None,
        new: None,
        container: Some(container),
        old_index: None,
        moved_from: None,
        children,
    }
}

/// Keys of both objects in document order: the new document's order, with each
/// removed key placed after the nearest preceding key it had in the old one.
fn merged_keys<'a>(old: &'a Map<String, Value>, new: &'a Map<String, Value>) -> Vec<&'a String> {
    let mut order: Vec<&String> = new.keys().collect();
    let mut previous: Option<&String> = None;
    for key in old.keys() {
        if new.contains_key(key) {
            previous = Some(key);
            continue;
        }
        let at = previous.and_then(|p| order.iter().position(|k| *k == p)).map_or(0, |i| i + 1);
        order.insert(at, key);
        previous = Some(key);
    }
    order
}

/// Children of a key-matched array: new items in new order (paired with their
/// old item, or added), then removed old items.
fn keyed_children(
    old: &[Value],
    new: &[Value],
    matches: &[Option<usize>],
    path: &str,
    options: &DiffOptions,
) -> Vec<Node> {
    let matched: HashSet<usize> = matches.iter().flatten().copied().collect();
    // Replays the placement drift::diff_with performs, to know which items get a `move`.
    let mut current: Vec<Option<usize>> =
        (0..old.len()).filter(|i| matched.contains(i)).map(Some).collect();
    let mut children = Vec::new();
    for (index, item) in new.iter().enumerate() {
        let key = Some(index.to_string());
        children.push(match matches[index] {
            Some(old_index) => {
                let from =
                    current.iter().position(|&slot| slot == Some(old_index)).unwrap_or(index);
                let item_path = join_pointer(path, &index.to_string());
                let mut child = node(key, &item_path, &old[old_index], item, options);
                child.old_index = Some(old_index);
                if from != index {
                    let slot = current.remove(from);
                    current.insert(index, slot);
                    child.moved_from = Some(old_index);
                }
                child
            }
            None => {
                current.insert(index, None);
                Node::leaf(key, Status::Added, None, Some(item))
            }
        });
    }
    for (index, item) in old.iter().enumerate().filter(|(i, _)| !matched.contains(i)) {
        children.push(Node::leaf(Some(index.to_string()), Status::Removed, Some(item), None));
    }
    children
}

/// Number of changed nodes (added, removed or replaced), i.e. expected op count.
pub fn changed_nodes(node: &Node) -> usize {
    match node.status {
        Status::Added | Status::Removed | Status::Changed => 1,
        Status::Same | Status::Ignored => 0,
        Status::Nested => node.children.iter().map(changed_nodes).sum(),
    }
}

/// Number of key-matched items that changed position, i.e. expected `move` ops.
pub fn moved_nodes(node: &Node) -> usize {
    usize::from(node.moved_from.is_some()) + node.children.iter().map(moved_nodes).sum::<usize>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn keys(node: &Node) -> Vec<&str> {
        node.children.iter().map(|c| c.key.as_deref().unwrap()).collect()
    }

    #[test]
    fn children_follow_document_order_with_removed_keys_in_place() {
        let old = json!({"zebra": 1, "gone": 1, "apple": 1});
        let new = json!({"zebra": 2, "apple": 1, "fresh": 1});
        // `gone` stays right after `zebra`, where it was; `fresh` is last, as in the new document.
        assert_eq!(
            keys(&build(&old, &new, &DiffOptions::new())),
            ["zebra", "gone", "apple", "fresh"]
        );
    }
}

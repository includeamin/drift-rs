//! Merged view of two documents, annotated with what changed.
//!
//! The walk mirrors [`drift::diff`]: objects are matched by key and arrays by
//! index. Each changed node corresponds to exactly one RFC 6902 operation, which
//! the tests check.

use drift::match_array_items;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeSet, HashSet};

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Same,
    Added,
    Removed,
    Changed,
    /// Unchanged itself, but something below it changed.
    Nested,
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

/// `array_keys` must be the same ones given to [`drift::diff_with`], so the tree
/// pairs array items exactly like the operations do.
pub fn build(old: &Value, new: &Value, array_keys: &[String]) -> Node {
    node(None, old, new, array_keys)
}

fn node(key: Option<String>, old: &Value, new: &Value, keys: &[String]) -> Node {
    if old == new {
        // Unchanged subtrees stay collapsed: one value, no children.
        return Node::leaf(key, Status::Same, None, Some(new));
    }
    let container = if new.is_array() { "array" } else { "object" };
    let children = match (old, new) {
        (Value::Object(o), Value::Object(n)) => {
            let names: BTreeSet<&String> = o.keys().chain(n.keys()).collect();
            names
                .into_iter()
                .map(|k| match (o.get(k), n.get(k)) {
                    (Some(a), Some(b)) => node(Some(k.clone()), a, b, keys),
                    (Some(a), None) => Node::leaf(Some(k.clone()), Status::Removed, Some(a), None),
                    (None, Some(b)) => Node::leaf(Some(k.clone()), Status::Added, None, Some(b)),
                    (None, None) => unreachable!(),
                })
                .collect()
        }
        (Value::Array(o), Value::Array(n)) => match match_array_items(n, o, keys) {
            Some(matches) => keyed_children(o, n, &matches, keys),
            None => (0..o.len().max(n.len()))
                .map(|i| {
                    let key = Some(i.to_string());
                    match (o.get(i), n.get(i)) {
                        (Some(a), Some(b)) => node(key, a, b, keys),
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

/// Children of a key-matched array: new items in new order (paired with their
/// old item, or added), then removed old items.
fn keyed_children(
    old: &[Value],
    new: &[Value],
    matches: &[Option<usize>],
    keys: &[String],
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
                let mut child = node(key, &old[old_index], item, keys);
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
        Status::Same => 0,
        Status::Nested => node.children.iter().map(changed_nodes).sum(),
    }
}

/// Number of key-matched items that changed position, i.e. expected `move` ops.
pub fn moved_nodes(node: &Node) -> usize {
    usize::from(node.moved_from.is_some()) + node.children.iter().map(moved_nodes).sum::<usize>()
}

//! Merged view of two documents, annotated with what changed.
//!
//! The walk mirrors [`drift::diff`]: objects are matched by key and arrays by
//! index. Each changed node corresponds to exactly one RFC 6902 operation, which
//! the tests check.

use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;

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
            children: Vec::new(),
        }
    }
}

pub fn build(old: &Value, new: &Value) -> Node {
    node(None, old, new)
}

fn node(key: Option<String>, old: &Value, new: &Value) -> Node {
    if old == new {
        // Unchanged subtrees stay collapsed: one value, no children.
        return Node::leaf(key, Status::Same, None, Some(new));
    }
    let container = if new.is_array() { "array" } else { "object" };
    let children = match (old, new) {
        (Value::Object(o), Value::Object(n)) => {
            let keys: BTreeSet<&String> = o.keys().chain(n.keys()).collect();
            keys.into_iter()
                .map(|k| match (o.get(k), n.get(k)) {
                    (Some(a), Some(b)) => node(Some(k.clone()), a, b),
                    (Some(a), None) => Node::leaf(Some(k.clone()), Status::Removed, Some(a), None),
                    (None, Some(b)) => Node::leaf(Some(k.clone()), Status::Added, None, Some(b)),
                    (None, None) => unreachable!(),
                })
                .collect()
        }
        (Value::Array(o), Value::Array(n)) => (0..o.len().max(n.len()))
            .map(|i| {
                let key = Some(i.to_string());
                match (o.get(i), n.get(i)) {
                    (Some(a), Some(b)) => node(key, a, b),
                    (Some(a), None) => Node::leaf(key, Status::Removed, Some(a), None),
                    (None, Some(b)) => Node::leaf(key, Status::Added, None, Some(b)),
                    (None, None) => unreachable!(),
                }
            })
            .collect(),
        _ => return Node::leaf(key, Status::Changed, Some(old), Some(new)),
    };
    Node { key, status: Status::Nested, old: None, new: None, container: Some(container), children }
}

/// Number of changed nodes (added, removed or replaced), i.e. expected op count.
pub fn changed_nodes(node: &Node) -> usize {
    match node.status {
        Status::Added | Status::Removed | Status::Changed => 1,
        Status::Same => 0,
        Status::Nested => node.children.iter().map(changed_nodes).sum(),
    }
}

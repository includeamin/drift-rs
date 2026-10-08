//! Browser bindings for drift: parse two documents, diff them, and report the
//! result together with timings and size metrics.

mod clock;
pub mod tree;

use clock::Clock;
use drift::formats::{self, Format};
use drift::{diff_with, patch, DiffOptions};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use wasm_bindgen::prelude::*;

#[derive(Serialize)]
pub struct Side {
    pub format: &'static str,
    pub bytes: usize,
    pub lines: usize,
    pub nodes: usize,
    pub depth: usize,
    pub parse_ms: f64,
}

#[derive(Serialize)]
pub struct Metrics {
    pub old: Side,
    pub new: Side,
    pub diff_ms: f64,
    pub tree_ms: f64,
    pub patch_ms: f64,
    pub total_ms: f64,
    pub operations: usize,
    pub by_op: BTreeMap<&'static str, usize>,
    /// Applying the operations to the old document reproduces the new one.
    pub roundtrip_ok: bool,
    pub version: &'static str,
}

#[derive(Serialize)]
pub struct Report {
    pub ok: bool,
    pub operations: Vec<Value>,
    pub tree: tree::Node,
    pub metrics: Metrics,
    pub raw_pretty: String,
}

#[derive(Serialize, Debug)]
pub struct Failure {
    pub ok: bool,
    /// "old" or "new": which input failed to parse.
    pub side: &'static str,
    pub error: String,
}

fn measure(value: &Value) -> (usize, usize) {
    fn walk(value: &Value, depth: usize) -> (usize, usize) {
        let children: Vec<&Value> = match value {
            Value::Object(map) => map.values().collect(),
            Value::Array(items) => items.iter().collect(),
            _ => return (1, depth),
        };
        children.into_iter().fold((1, depth), |(nodes, deepest), child| {
            let (n, d) = walk(child, depth + 1);
            (nodes + n, deepest.max(d))
        })
    }
    walk(value, 0)
}

/// `hint` is a file name, a format name, or empty to sniff the text.
fn resolve(hint: &str, text: &str) -> Format {
    Format::from_name(hint)
        .or_else(|| Format::from_extension(hint))
        .unwrap_or_else(|| Format::sniff(text))
}

fn load(text: &str, hint: &str, side: &'static str) -> Result<(Value, Side), Failure> {
    let format = resolve(hint, text);
    let clock = Clock::start();
    let value = formats::parse(text, format).map_err(|e| Failure {
        ok: false,
        side,
        error: e.to_string(),
    })?;
    let parse_ms = clock.elapsed_ms();
    let (nodes, depth) = measure(&value);
    let info = Side {
        format: format.as_str(),
        bytes: text.len(),
        lines: text.lines().count(),
        nodes,
        depth,
        parse_ms,
    };
    Ok((value, info))
}

pub fn run(
    old_text: &str,
    new_text: &str,
    old_hint: &str,
    new_hint: &str,
    array_keys: &str,
) -> Result<Report, Failure> {
    let options = DiffOptions {
        array_keys: array_keys
            .split(',')
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .map(String::from)
            .collect(),
    };
    let total = Clock::start();
    let (old, old_side) = load(old_text, old_hint, "old")?;
    let (new, new_side) = load(new_text, new_hint, "new")?;

    let clock = Clock::start();
    let deltas = diff_with(&new, &old, &options);
    let diff_ms = clock.elapsed_ms();

    let clock = Clock::start();
    let tree = tree::build(&old, &new, &options.array_keys);
    let tree_ms = clock.elapsed_ms();

    let clock = Clock::start();
    let roundtrip_ok = patch(old, &deltas).is_ok_and(|patched| patched == new);
    let patch_ms = clock.elapsed_ms();

    let mut by_op = BTreeMap::new();
    for delta in &deltas {
        *by_op.entry(delta.op.as_str()).or_insert(0) += 1;
    }
    let operations: Vec<Value> =
        deltas.iter().filter_map(|d| serde_json::to_value(d).ok()).collect();
    let raw_pretty = serde_json::to_string_pretty(&operations).unwrap_or_default();

    Ok(Report {
        ok: true,
        metrics: Metrics {
            old: old_side,
            new: new_side,
            diff_ms,
            tree_ms,
            patch_ms,
            total_ms: total.elapsed_ms(),
            operations: operations.len(),
            by_op,
            roundtrip_ok,
            version: drift::VERSION,
        },
        operations,
        tree,
        raw_pretty,
    })
}

/// Diffs two documents and returns a JSON string: a [`Report`], or a
/// [`Failure`] (`ok: false`) if either input does not parse.
#[wasm_bindgen]
pub fn run_diff(old: &str, new: &str, old_hint: &str, new_hint: &str, array_keys: &str) -> String {
    let result = match run(old, new, old_hint, new_hint, array_keys) {
        Ok(report) => serde_json::to_string(&report),
        Err(failure) => serde_json::to_string(&failure),
    };
    result.unwrap_or_else(|e| format!(r#"{{"ok":false,"side":"old","error":"{e}"}}"#))
}

/// Name of the format that would be used for `text`, given a hint.
#[wasm_bindgen]
pub fn detect_format(text: &str, hint: &str) -> String {
    resolve(hint, text).as_str().to_string()
}

#[wasm_bindgen]
pub fn version() -> String {
    drift::VERSION.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reports_operations_and_metrics() {
        let report =
            run(r#"{"a":1,"b":[1,2]}"#, r#"{"a":2,"b":[1],"c":true}"#, "", "", "").unwrap();
        assert_eq!(report.metrics.operations, 3);
        assert_eq!(report.metrics.by_op["replace"], 1);
        assert!(report.metrics.roundtrip_ok);
        assert_eq!(report.metrics.old.format, "json");
        assert_eq!(report.metrics.old.nodes, 5);
        assert_eq!(report.metrics.old.depth, 2);
    }

    #[test]
    fn mixed_formats_can_be_compared() {
        let report = run("a: 1\n", r#"{"a": 2}"#, "x.yaml", "", "").unwrap();
        assert_eq!(report.metrics.old.format, "yaml");
        assert_eq!(report.metrics.operations, 1);
    }

    #[test]
    fn parse_failure_names_the_side() {
        let failure = run("{}", "{", "", "json", "").err().unwrap();
        assert_eq!(failure.side, "new");
        assert!(failure.error.contains("json"));
    }

    #[test]
    fn keyed_tree_matches_operations() {
        use drift::Operation;
        let keys = vec!["id".to_string()];
        let options = DiffOptions { array_keys: keys.clone() };
        let cases = [
            (
                json!([{"id": 1, "v": 1}, {"id": 2, "v": 2}]),
                json!([{"id": 0}, {"id": 1, "v": 1}, {"id": 2, "v": 3}]),
            ),
            (json!([{"id": 1}, {"id": 2}, {"id": 3}]), json!([{"id": 3}, {"id": 1}, {"id": 2}])),
            (
                json!({"l": [{"id": "a", "t": [1]}, {"id": "b"}]}),
                json!({"l": [{"id": "b"}, {"id": "a", "t": [1, 2]}]}),
            ),
        ];
        for (old, new) in cases {
            let ops = diff_with(&new, &old, &options);
            let changes = ops.iter().filter(|d| d.op != Operation::Move).count();
            let moves = ops.len() - changes;
            let tree = tree::build(&old, &new, &keys);
            assert_eq!(tree::changed_nodes(&tree), changes, "{old} -> {new}");
            assert_eq!(tree::moved_nodes(&tree), moves, "{old} -> {new}");
        }
    }

    #[test]
    fn tree_changed_nodes_match_operation_count() {
        let cases = [
            (
                json!({"a": {"b": 1, "c": [1, 2, 3]}, "d": "x"}),
                json!({"a": {"b": 2, "c": [1]}, "e": 1}),
            ),
            (json!([1, [2, 3], {"k": null}]), json!([1, [2, 3, 4], {"k": 0}, 9])),
            (json!(1), json!("one")),
            (json!({"same": [1, 2]}), json!({"same": [1, 2]})),
        ];
        for (old, new) in cases {
            let ops = drift::diff(&new, &old);
            let tree = tree::build(&old, &new, &[]);
            assert_eq!(tree::changed_nodes(&tree), ops.len(), "{old} -> {new}");
        }
    }

    #[test]
    fn unchanged_subtrees_are_collapsed() {
        let tree =
            tree::build(&json!({"a": {"x": 1}, "b": 1}), &json!({"a": {"x": 1}, "b": 2}), &[]);
        let a = &tree.children[0];
        assert_eq!(a.status, tree::Status::Same);
        assert!(a.children.is_empty());
    }
}

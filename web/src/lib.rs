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

/// The non-empty, trimmed items of a comma-separated list.
fn list(text: &str) -> impl Iterator<Item = &str> {
    text.split(',').map(str::trim).filter(|item| !item.is_empty())
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
    ignore_paths: &str,
) -> Result<Report, Failure> {
    let options = list(array_keys).fold(DiffOptions::new(), |o, key| o.array_key(key));
    let options = list(ignore_paths).fold(options, |o, pattern| o.ignore_path(pattern));
    let total = Clock::start();
    let (old, old_side) = load(old_text, old_hint, "old")?;
    let (new, new_side) = load(new_text, new_hint, "new")?;

    let clock = Clock::start();
    let deltas = diff_with(&new, &old, &options);
    let diff_ms = clock.elapsed_ms();

    let clock = Clock::start();
    let tree = tree::build(&old, &new, &options);
    let tree_ms = clock.elapsed_ms();

    let clock = Clock::start();
    // Ignored parts keep their old value, so with ignore patterns the check is
    // that nothing differs outside them.
    let roundtrip_ok = patch(old, &deltas).is_ok_and(|patched| {
        if options.ignore_paths.is_empty() {
            patched == new
        } else {
            diff_with(&new, &patched, &options).is_empty()
        }
    });
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
pub fn run_diff(
    old: &str,
    new: &str,
    old_hint: &str,
    new_hint: &str,
    array_keys: &str,
    ignore_paths: &str,
) -> String {
    let result = match run(old, new, old_hint, new_hint, array_keys, ignore_paths) {
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
            run(r#"{"a":1,"b":[1,2]}"#, r#"{"a":2,"b":[1],"c":true}"#, "", "", "", "").unwrap();
        assert_eq!(report.metrics.operations, 3);
        assert_eq!(report.metrics.by_op["replace"], 1);
        assert!(report.metrics.roundtrip_ok);
        assert_eq!(report.metrics.old.format, "json");
        assert_eq!(report.metrics.old.nodes, 5);
        assert_eq!(report.metrics.old.depth, 2);
    }

    #[test]
    fn ignored_paths_are_left_out_of_the_operations_and_marked_in_the_tree() {
        let (old, new) = (
            r#"{"a": 1, "stamp": 1, "n": {"seen": 1, "k": 1}, "gone": 1}"#,
            r#"{"a": 2, "stamp": 9, "n": {"seen": 2, "k": 2}, "fresh": 1}"#,
        );
        let report = run(old, new, "", "", "", " /stamp , /**/seen, /gone ,/fresh").unwrap();
        let paths: Vec<_> =
            report.operations.iter().map(|o| o["path"].as_str().unwrap().to_string()).collect();
        assert_eq!(paths, ["/a", "/n/k"]);
        assert_eq!(report.metrics.operations, 2);
        assert!(report.metrics.roundtrip_ok);
        let ignored = |key: &str| {
            report.tree.children.iter().find(|c| c.key.as_deref() == Some(key)).unwrap().status
        };
        for key in ["stamp", "gone", "fresh"] {
            assert_eq!(ignored(key), tree::Status::Ignored, "{key}");
        }
        assert_eq!(tree::changed_nodes(&report.tree), 2);
    }

    #[test]
    fn mixed_formats_can_be_compared() {
        let report = run("a: 1\n", r#"{"a": 2}"#, "x.yaml", "", "", "").unwrap();
        assert_eq!(report.metrics.old.format, "yaml");
        assert_eq!(report.metrics.operations, 1);
    }

    #[test]
    fn parse_failure_names_the_side() {
        let failure = run("{}", "{", "", "json", "", "").err().unwrap();
        assert_eq!(failure.side, "new");
        assert!(failure.error.contains("json"));
    }

    #[test]
    fn keyed_tree_matches_operations() {
        use drift::Operation;
        let options = DiffOptions::new().array_key("id");
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
            let tree = tree::build(&old, &new, &options);
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
            let tree = tree::build(&old, &new, &DiffOptions::new());
            assert_eq!(tree::changed_nodes(&tree), ops.len(), "{old} -> {new}");
        }
    }

    #[test]
    fn unchanged_subtrees_are_collapsed() {
        let tree = tree::build(
            &json!({"a": {"x": 1}, "b": 1}),
            &json!({"a": {"x": 1}, "b": 2}),
            &DiffOptions::new(),
        );
        let a = &tree.children[0];
        assert_eq!(a.status, tree::Status::Same);
        assert!(a.children.is_empty());
    }
}

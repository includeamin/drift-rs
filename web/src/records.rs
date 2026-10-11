//! Comparing two files of records from pasted text, with the same results as the
//! `drift diff` command gives for NDJSON, CSV and TSV files.
//!
//! The command works on files and never holds the rows. Here the text is already
//! in memory, so each row is wrapped as `{"k": key, "r": row}` and the two lists
//! are diffed as arrays matched by `k`, which is what the command's planner does;
//! the wrapper is taken off the operations afterwards.

use crate::clock::Clock;
use crate::tree::{self, Node, Status};
use crate::{list, measure, Metrics, Side};
use drift::{diff_with, patch, split_pointer, DiffOptions, Operation};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};

#[derive(Clone, Copy, PartialEq)]
enum Policy {
    Error,
    First,
    Last,
    Occurrence,
}

struct Rows {
    /// The wrapped rows that take part.
    kept: Vec<Value>,
    /// How many rows the file has, and how many were left out as repeats.
    total: usize,
}

fn parse_rows(text: &str, format: &str) -> Result<Vec<(usize, Value)>, String> {
    match format {
        "ndjson" => text
            .lines()
            .enumerate()
            .filter(|(_, line)| !line.trim().is_empty())
            .map(|(at, line)| {
                serde_json::from_str(line.trim())
                    .map(|row| (at + 1, row))
                    .map_err(|e| format!("line {}: {e}", at + 1))
            })
            .collect(),
        "csv" | "tsv" => {
            let delimiter = if format == "tsv" { b'\t' } else { b',' };
            let mut reader = csv::ReaderBuilder::new()
                .delimiter(delimiter)
                .flexible(true)
                .from_reader(text.as_bytes());
            let header: Vec<String> = reader
                .headers()
                .map_err(|e| format!("header: {e}"))?
                .iter()
                .map(String::from)
                .collect();
            if let Some(empty) = header.iter().position(String::is_empty) {
                return Err(format!("header: column {} has no name", empty + 1));
            }
            let mut rows = Vec::new();
            for record in reader.records() {
                let record = record.map_err(|e| e.to_string())?;
                let line = record.position().map_or(0, |p| p.line() as usize);
                if record.len() != header.len() {
                    return Err(format!(
                        "line {line}: expected {} fields, found {}",
                        header.len(),
                        record.len()
                    ));
                }
                let row: Map<String, Value> = header
                    .iter()
                    .zip(record.iter())
                    .map(|(name, cell)| (name.clone(), Value::String(cell.to_string())))
                    .collect();
                rows.push((line, Value::Object(row)));
            }
            Ok(rows)
        }
        other => Err(format!("unknown record format `{other}`")),
    }
}

/// The value with object members in a fixed order, as text: equal values give
/// equal text whatever the order of members.
fn canonical(value: &Value) -> String {
    match value {
        Value::Object(members) => {
            let sorted: BTreeMap<&String, String> =
                members.iter().map(|(name, member)| (name, canonical(member))).collect();
            let parts: Vec<String> =
                sorted.iter().map(|(name, text)| format!("{}:{text}", json!(name))).collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(items) => {
            format!("[{}]", items.iter().map(canonical).collect::<Vec<_>>().join(","))
        }
        other => other.to_string(),
    }
}

fn key_of(row: &Value, fields: &[&str], line: usize) -> Result<String, String> {
    let mut parts = Vec::with_capacity(fields.len());
    for field in fields {
        let found = if field.starts_with('/') { row.pointer(field) } else { row.get(field) };
        match found {
            None => return Err(format!("line {line}: the row has no `{field}`")),
            Some(v @ (Value::String(_) | Value::Number(_) | Value::Bool(_))) => {
                parts.push(v.clone())
            }
            Some(_) => {
                return Err(format!("line {line}: `{field}` must be a string, number or boolean"))
            }
        }
    }
    Ok(Value::Array(parts).to_string())
}

fn prepare(
    rows: Vec<(usize, Value)>,
    fields: &[&str],
    keyless: bool,
    policy: Policy,
) -> Result<Rows, String> {
    let total = rows.len();
    let mut keyed = Vec::with_capacity(total);
    for (line, row) in rows {
        let key = if keyless { canonical(&row) } else { key_of(&row, fields, line)? };
        keyed.push((line, key, row));
    }
    let kept: Vec<(String, Value)> = match policy {
        Policy::Error => {
            let mut first: HashMap<&str, usize> = HashMap::new();
            for (line, key, _) in &keyed {
                if let Some(earlier) = first.insert(key, *line) {
                    return Err(format!(
                        "line {line}: duplicate key {key} (first seen at line {earlier})"
                    ));
                }
            }
            keyed.into_iter().map(|(_, key, row)| (key, row)).collect()
        }
        Policy::First | Policy::Last => {
            // The row that survives keeps its own place among the rows.
            let mut chosen: HashMap<&str, usize> = HashMap::new();
            for (at, (_, key, _)) in keyed.iter().enumerate() {
                if policy == Policy::Last {
                    chosen.insert(key, at);
                } else {
                    chosen.entry(key).or_insert(at);
                }
            }
            let survivors: Vec<bool> = keyed
                .iter()
                .enumerate()
                .map(|(at, (_, key, _))| chosen[key.as_str()] == at)
                .collect();
            keyed
                .into_iter()
                .zip(survivors)
                .filter_map(|((_, key, row), keep)| keep.then_some((key, row)))
                .collect()
        }
        Policy::Occurrence => {
            // The nth row with a key is matched to the nth with it in the other file.
            let mut seen: HashMap<String, usize> = HashMap::new();
            keyed
                .into_iter()
                .map(|(_, key, row)| {
                    let nth = seen.entry(key.clone()).or_insert(0);
                    let unique = if *nth == 0 { key } else { format!("{key}#{nth}") };
                    *nth += 1;
                    (unique, row)
                })
                .collect()
        }
    };
    let kept = kept.into_iter().map(|(key, row)| json!({"k": key, "r": row})).collect();
    Ok(Rows { kept, total })
}

/// Takes the wrapper off a path: `/3/r/name` is `/3/name`.
fn unwrap_path(path: &str) -> String {
    match split_pointer(path) {
        Ok(tokens) if tokens.len() >= 2 && tokens[1] == "r" => {
            let mut rest = vec![tokens[0].clone()];
            rest.extend(tokens[2..].iter().cloned());
            rest.iter().fold(String::new(), |path, token| drift::join_pointer(&path, token))
        }
        _ => path.to_string(),
    }
}

/// A wrapped row's own value.
fn unwrap_value(value: &mut Value) {
    if let Some(row) = value.get_mut("r").map(Value::take) {
        *value = row;
    }
}

/// The tree of two lists of wrapped rows, as the tree of the rows themselves: a
/// row's node is its `r` node, keeping the row's position.
fn unwrap_tree(mut root: Node) -> Node {
    if root.status == Status::Nested {
        root.children = root.children.into_iter().map(unwrap_row).collect();
    } else {
        for list in [&mut root.old, &mut root.new].into_iter().flatten() {
            if let Value::Array(rows) = list {
                rows.iter_mut().for_each(unwrap_value);
            }
        }
    }
    root
}

fn unwrap_row(mut row: Node) -> Node {
    if row.status == Status::Nested {
        if let Some(at) = row.children.iter().position(|c| c.key.as_deref() == Some("r")) {
            let mut inner = row.children.swap_remove(at);
            inner.key = row.key;
            inner.old_index = row.old_index;
            inner.moved_from = row.moved_from;
            return inner;
        }
    }
    for value in [&mut row.old, &mut row.new].into_iter().flatten() {
        unwrap_value(value);
    }
    row
}

/// The wrapped rows as the rows they hold.
fn rows_of(wrapped: &[Value]) -> Value {
    Value::Array(wrapped.iter().map(|row| row["r"].clone()).collect())
}

pub fn run(
    old_text: &str,
    new_text: &str,
    format: &str,
    key: &str,
    duplicates: &str,
    keyless: bool,
    ignore_paths: &str,
) -> Result<Value, String> {
    let fields: Vec<&str> = list(key).collect();
    if !keyless && fields.is_empty() {
        return Err("give the key: the field that identifies a row, or tick Keyless".into());
    }
    let policy = match (duplicates, keyless) {
        ("error", _) => Policy::Error,
        ("first", _) => Policy::First,
        ("last", _) => Policy::Last,
        ("occurrence", _) => Policy::Occurrence,
        (_, true) => Policy::Occurrence,
        (_, false) => Policy::Error,
    };
    let clock = Clock::start();
    let old_rows = parse_rows(old_text, format).map_err(|e| format!("old: {e}"))?;
    let old_parse_ms = clock.elapsed_ms();
    let clock = Clock::start();
    let new_rows = parse_rows(new_text, format).map_err(|e| format!("new: {e}"))?;
    let new_parse_ms = clock.elapsed_ms();
    let total = Clock::start();
    let old = prepare(old_rows, &fields, keyless, policy).map_err(|e| format!("old: {e}"))?;
    let new = prepare(new_rows, &fields, keyless, policy).map_err(|e| format!("new: {e}"))?;

    // The command's patterns name rows as `/*/field`; inside the wrapper that is
    // `/*/r/field`.
    let options = list(ignore_paths).fold(DiffOptions::new().array_key("k"), |o, pattern| {
        match pattern.strip_prefix("/*") {
            Some(rest) => o.ignore_path(format!("/*/r{rest}")),
            None => o.ignore_path(pattern),
        }
    });
    let (old_list, new_list) = (Value::Array(old.kept.clone()), Value::Array(new.kept.clone()));
    let clock = Clock::start();
    let deltas = diff_with(&new_list, &old_list, &options);
    let diff_ms = clock.elapsed_ms();

    let clock = Clock::start();
    let tree = unwrap_tree(tree::build(&old_list, &new_list, &options));
    let tree_ms = clock.elapsed_ms();

    let clock = Clock::start();
    // As for documents: with ignored parts the check is that nothing differs
    // outside them.
    let roundtrip_ok = patch(old_list.clone(), &deltas).is_ok_and(|patched| {
        if options.ignore_paths.is_empty() {
            patched == new_list
        } else {
            diff_with(&new_list, &patched, &options).is_empty()
        }
    });
    let patch_ms = clock.elapsed_ms();

    let mut by_op: BTreeMap<&'static str, usize> = BTreeMap::new();
    let operations: Vec<Value> = deltas
        .into_iter()
        .map(|mut delta| {
            delta.path = unwrap_path(&delta.path);
            delta.from_path = delta.from_path.as_deref().map(unwrap_path);
            if delta.op == Operation::Add && split_pointer(&delta.path).is_ok_and(|t| t.len() == 1)
            {
                delta.value = delta.value.and_then(|wrapped| wrapped.get("r").cloned());
            }
            *by_op.entry(delta.op.as_str()).or_insert(0) += 1;
            serde_json::to_value(&delta).unwrap_or(Value::Null)
        })
        .collect();
    let raw_pretty = serde_json::to_string_pretty(&operations).unwrap_or_default();

    let side = |text: &str, rows: &[Value], parse_ms: f64| {
        let (nodes, depth) = measure(&rows_of(rows));
        Side {
            format: match format {
                "csv" => "csv",
                "tsv" => "tsv",
                _ => "ndjson",
            },
            bytes: text.len(),
            lines: text.lines().count(),
            nodes,
            depth,
            parse_ms,
        }
    };
    let metrics = Metrics {
        old: side(old_text, &old.kept, old_parse_ms),
        new: side(new_text, &new.kept, new_parse_ms),
        diff_ms,
        tree_ms,
        patch_ms,
        total_ms: total.elapsed_ms() + old_parse_ms + new_parse_ms,
        operations: operations.len(),
        by_op,
        roundtrip_ok,
        version: drift::VERSION,
    };
    let mut metrics = serde_json::to_value(&metrics).map_err(|e| e.to_string())?;
    metrics["rows"] = json!({
        "old": old.total, "new": new.total, "old_used": old.kept.len(), "new_used": new.kept.len(),
    });
    Ok(json!({
        "operations": operations,
        "tree": serde_json::to_value(&tree).map_err(|e| e.to_string())?,
        "metrics": metrics,
        "raw_pretty": raw_pretty,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use drift::{diff_records_with, CsvOptions, Duplicates, RecordFormat, RecordOptions};

    fn write(name: &str, text: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("drift-web-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    /// What the command's library call gives for the same text, as JSON values.
    fn command(
        old: &str,
        new: &str,
        format: &str,
        key: &str,
        duplicates: &str,
        keyless: bool,
        ignore: &str,
    ) -> Result<Vec<Value>, String> {
        let (o, n) = (write(&format!("o.{format}"), old), write(&format!("n.{format}"), new));
        let mut diff = DiffOptions::new();
        for pattern in list(ignore) {
            diff = diff.ignore_path(pattern);
        }
        let mut options = RecordOptions::new().diff(diff);
        if format != "ndjson" {
            let delimiter = if format == "tsv" { b'\t' } else { b',' };
            options = options.format(RecordFormat::Csv(CsvOptions::new().delimiter(delimiter)));
        }
        if keyless {
            options = options.whole_row();
        }
        for field in list(key) {
            options = options.key(field);
        }
        match duplicates {
            "first" => options = options.duplicates(Duplicates::First),
            "last" => options = options.duplicates(Duplicates::Last),
            "occurrence" => options = options.duplicates(Duplicates::Occurrence),
            "error" => options = options.duplicates(Duplicates::Error),
            _ => {}
        }
        diff_records_with(&o, &n, &options)
            .map(|ops| ops.iter().map(|d| serde_json::to_value(d).unwrap()).collect())
            .map_err(|e| e.to_string())
    }

    /// Added, removed and changed nodes, and moved ones, in a tree as JSON.
    fn count(node: &Value) -> (usize, usize) {
        let own =
            usize::from(matches!(node["status"].as_str(), Some("added" | "removed" | "changed")));
        let moved = usize::from(node.get("moved_from").is_some());
        node["children"]
            .as_array()
            .into_iter()
            .flatten()
            .map(count)
            .fold((own, moved), |(a, b), (c, d)| (a + c, b + d))
    }

    #[test]
    fn random_files_give_what_the_command_gives() {
        let mut state = 0x9E3779B97F4A7C15u64;
        let mut next = move |n: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % n
        };
        let cases: [(&str, &str, &str, bool, &str); 12] = [
            ("ndjson", "id", "", false, ""),
            ("ndjson", "id,region", "", false, ""),
            ("ndjson", "id", "first", false, ""),
            ("ndjson", "id", "last", false, ""),
            ("ndjson", "id", "occurrence", false, ""),
            ("ndjson", "", "", true, ""),
            ("ndjson", "", "first", true, ""),
            ("ndjson", "id", "", false, "/*/v"),
            ("csv", "id", "last", false, ""),
            ("csv", "", "", true, ""),
            ("tsv", "id,region", "occurrence", false, "/*/v"),
            ("csv", "id", "first", false, ""),
        ];
        let mut compared = 0;
        for round in 0..300 {
            let (format, key, duplicates, keyless, ignore) = cases[round % cases.len()];
            let unique = duplicates.is_empty() && !keyless;
            let make = |next: &mut dyn FnMut(u64) -> u64| -> Vec<(u64, &'static str, u64)> {
                let mut rows: Vec<_> = (0..next(10))
                    .map(|_| (next(5), ["eu", "us"][next(2) as usize], next(3)))
                    .collect();
                if unique {
                    let mut seen = std::collections::HashSet::new();
                    rows.retain(|r| seen.insert((r.0, r.1)));
                }
                rows
            };
            let text = |rows: &[(u64, &str, u64)]| -> String {
                match format {
                    "ndjson" => rows
                        .iter()
                        .map(|(id, region, v)| {
                            format!("{{\"id\":{id},\"region\":\"{region}\",\"v\":{v}}}\n")
                        })
                        .collect(),
                    _ => {
                        let sep = if format == "tsv" { '\t' } else { ',' };
                        let mut out = format!("id{sep}region{sep}v\n");
                        for (id, region, v) in rows {
                            out += &format!("{id}{sep}{region}{sep}{v}\n");
                        }
                        out
                    }
                }
            };
            let (old, new) = (text(&make(&mut next)), text(&make(&mut next)));
            let expected = command(&old, &new, format, key, duplicates, keyless, ignore);
            let report = run(&old, &new, format, key, duplicates, keyless, ignore);
            if let Ok(report) = &report {
                // The tree is of the rows, not of how they were wrapped, and each of
                // its changes is one operation.
                let text = report["tree"].to_string();
                assert!(!text.contains("\"k\":") && !text.contains("\"r\":"), "{text}");
                let (changed, moved) = count(&report["tree"]);
                assert_eq!(
                    changed + moved,
                    report["operations"].as_array().unwrap().len(),
                    "round {round} {format} {key}: {old:?} -> {new:?}\n{text}"
                );
                assert_eq!(report["metrics"]["roundtrip_ok"], true, "round {round}");
            }
            let got = report.map(|report| report["operations"].as_array().unwrap().clone());
            match (&got, &expected) {
                (Ok(got), Ok(expected)) => {
                    assert_eq!(got, expected, "round {round} {format} {key}: {old:?} -> {new:?}")
                }
                (Err(_), Err(_)) => {}
                _ => panic!("round {round}: {got:?} vs {expected:?}"),
            }
            compared += 1;
        }
        assert_eq!(compared, 300);
    }

    #[test]
    fn problems_are_named_by_side_and_line() {
        let ok = "{\"id\":1}\n";
        let e = run(ok, "{\"id\":1}\n{\"id\":1}\n", "ndjson", "id", "", false, "").unwrap_err();
        assert!(e.starts_with("new: line 2: duplicate key"), "{e}");
        let e = run("{\"x\":1}\n", ok, "ndjson", "id", "", false, "").unwrap_err();
        assert_eq!(e, "old: line 1: the row has no `id`");
        let e = run(ok, "{\"id\":\n", "ndjson", "id", "", false, "").unwrap_err();
        assert!(e.starts_with("new: line 1:"), "{e}");
        assert!(run(ok, ok, "ndjson", "", "", false, "").unwrap_err().contains("give the key"));
        let e = run("id,v\n1\n", "id,v\n", "csv", "id", "", false, "").unwrap_err();
        assert!(e.contains("old: line 2: expected 2 fields, found 1"), "{e}");
    }

    #[test]
    fn a_row_added_at_the_front_is_one_operation_with_the_row_unwrapped() {
        let report = run(
            "{\"id\":1,\"v\":1}\n",
            "{\"id\":0,\"v\":0}\n{\"id\":1,\"v\":1}\n",
            "ndjson",
            "id",
            "",
            false,
            "",
        )
        .unwrap();
        assert_eq!(
            report["operations"],
            json!([{"op": "add", "path": "/0", "value": {"id": 0, "v": 0}}])
        );
        let rows = &report["metrics"]["rows"];
        assert_eq!((rows["old"].as_u64(), rows["new"].as_u64()), (Some(1), Some(2)));
        assert_eq!(report["metrics"]["roundtrip_ok"], true);
    }
}

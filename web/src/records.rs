//! Comparing two files of records from pasted text, with the same results as the
//! `drift diff` command gives for NDJSON, CSV and TSV files.
//!
//! The command works on files and never holds the rows. Here the text is already
//! in memory, so each row is wrapped as `{"k": key, "r": row}` and the two lists
//! are diffed as arrays matched by `k`, which is what the command's planner does;
//! the wrapper is taken off the operations afterwards.

use crate::clock::Clock;
use crate::list;
use drift::{diff_with, split_pointer, DiffOptions, Operation};
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
    let old_rows = parse_rows(old_text, format).map_err(|e| format!("old: {e}"))?;
    let new_rows = parse_rows(new_text, format).map_err(|e| format!("new: {e}"))?;
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
    let clock = Clock::start();
    let deltas =
        diff_with(&Value::Array(new.kept.clone()), &Value::Array(old.kept.clone()), &options);
    let diff_ms = clock.elapsed_ms();

    let mut by_op: BTreeMap<&str, usize> = BTreeMap::new();
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
    Ok(json!({
        "operations": operations,
        "raw_pretty": raw_pretty,
        "by_op": by_op,
        "old_rows": old.total,
        "new_rows": new.total,
        "old_used": old.kept.len(),
        "new_used": new.kept.len(),
        "diff_ms": diff_ms,
        "total_ms": total.elapsed_ms(),
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
            let got = run(&old, &new, format, key, duplicates, keyless, ignore)
                .map(|report| report["operations"].as_array().unwrap().clone());
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
        assert_eq!((report["old_rows"].as_u64(), report["new_rows"].as_u64()), (Some(1), Some(2)));
    }
}

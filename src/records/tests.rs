use super::*;
use crate::{diff_with, patch};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};

/// A scratch directory that is removed afterwards.
struct Dir(std::path::PathBuf);

impl Dir {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "drift-records-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    /// Writes rows as one JSON object per line and returns the path.
    fn rows(&self, name: &str, rows: &[Value]) -> std::path::PathBuf {
        let text: String = rows.iter().map(|r| format!("{r}\n")).collect();
        self.text(name, &text)
    }
    fn text(&self, name: &str, text: &str) -> std::path::PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// What the in-memory keyed diff says about the same rows.
fn reference(old: &[Value], new: &[Value], key: &str, options: &DiffOptions) -> Vec<Delta> {
    let mut options = options.clone();
    options.array_keys.insert(0, key.to_string());
    diff_with(&Value::Array(new.to_vec()), &Value::Array(old.to_vec()), &options)
}

/// Streams the two row sets from files and checks the result is exactly the
/// in-memory one, and that applying it to the old rows gives the new ones.
fn assert_same_as_in_memory(
    old: &[Value],
    new: &[Value],
    key: &str,
    options: &DiffOptions,
) -> Vec<Delta> {
    let dir = Dir::new();
    let (old_file, new_file) = (dir.rows("old.ndjson", old), dir.rows("new.ndjson", new));
    let streamed = diff_records(&old_file, &new_file, key, options)
        .unwrap_or_else(|e| panic!("{e}: {old:?} -> {new:?}"));
    assert_eq!(streamed, reference(old, new, key, options), "{old:?} -> {new:?}");
    if options.ignore_paths.is_empty() {
        let patched = patch(Value::Array(old.to_vec()), &streamed).unwrap();
        assert_eq!(patched, Value::Array(new.to_vec()), "{streamed:?}");
    }
    streamed
}

fn row(id: u64, v: u64) -> Value {
    json!({"id": id, "v": v, "tags": ["a", "b"], "o": {"k": id}})
}

#[test]
fn identical_files_have_no_differences() {
    let rows: Vec<Value> = (0..20).map(|i| row(i, i)).collect();
    assert!(assert_same_as_in_memory(&rows, &rows, "id", &DiffOptions::new()).is_empty());
    assert!(assert_same_as_in_memory(&[], &[], "id", &DiffOptions::new()).is_empty());
}

#[test]
fn edits_inserts_deletes_and_moves_match_the_in_memory_diff() {
    let old: Vec<Value> = (0..6).map(|i| row(i, i)).collect();
    let cases: Vec<Vec<Value>> = vec![
        vec![row(0, 9), row(1, 1), row(2, 2), row(3, 3), row(4, 4), row(5, 5)], // an edit
        [vec![row(99, 0)], old.clone()].concat(), // insert at the front
        [old.clone(), vec![row(99, 0)]].concat(), // append
        old[1..].to_vec(),                        // delete the first
        old[..5].to_vec(),                        // delete the last
        vec![
            old[3].clone(),
            old[0].clone(),
            old[1].clone(),
            old[2].clone(),
            old[4].clone(),
            old[5].clone(),
        ], // move
        old.iter().rev().cloned().collect(),      // reverse
        vec![row(7, 7), row(2, 8), row(0, 0)],    // everything at once
        vec![],                                   // delete all
    ];
    for new in cases {
        assert_same_as_in_memory(&old, &new, "id", &DiffOptions::new());
    }
    assert_same_as_in_memory(&[], &old, "id", &DiffOptions::new());
}

#[test]
fn a_front_insert_is_one_operation_not_a_run_of_replacements() {
    let old: Vec<Value> = (1..=1000).map(|i| row(i, i)).collect();
    let new = [vec![row(0, 0)], old.clone()].concat();
    let ops = assert_same_as_in_memory(&old, &new, "id", &DiffOptions::new());
    assert_eq!(ops.len(), 1);
}

#[test]
fn rows_equal_up_to_field_order_and_whitespace_are_not_differences() {
    let dir = Dir::new();
    let old = dir.text("old.ndjson", "{\"id\":1,\"a\":1,\"b\":[1,2]}\n{\"id\":2,\"a\":2}\n");
    let new = dir
        .text("new.ndjson", "  {\"b\": [1, 2], \"a\": 1, \"id\": 1}  \n{ \"a\":2 , \"id\":2 }\n");
    assert!(diff_records(&old, &new, "id", &DiffOptions::new()).unwrap().is_empty());
    // 1 and 1.0 are different values, as everywhere else in drift.
    let float = dir.text("float.ndjson", "{\"id\":1,\"a\":1.0,\"b\":[1,2]}\n{\"id\":2,\"a\":2}\n");
    let ops = diff_records(&old, &float, "id", &DiffOptions::new()).unwrap();
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].path, "/0/a");
}

#[test]
fn blank_lines_and_windows_line_endings_are_ignored() {
    let dir = Dir::new();
    let old = dir.text("old.ndjson", "{\"id\":1,\"v\":1}\r\n\r\n   \n{\"id\":2,\"v\":2}\r\n");
    let new = dir.text("new.ndjson", "\n{\"id\":1,\"v\":1}\n{\"id\":2,\"v\":3}");
    let ops = diff_records(&old, &new, "id", &DiffOptions::new()).unwrap();
    // Row indices count rows, not lines.
    assert_eq!(ops.iter().map(|o| o.path.as_str()).collect::<Vec<_>>(), ["/1/v"]);
}

#[test]
fn numeric_and_string_keys_are_different_keys() {
    let old = [json!({"id": 1}), json!({"id": "1"})];
    let new = [json!({"id": "1"}), json!({"id": 1})];
    assert_same_as_in_memory(&old, &new, "id", &DiffOptions::new());
}

#[test]
fn the_key_can_be_any_field_and_unicode() {
    let old = [json!({"name": "é", "n": 1}), json!({"name": "日本", "n": 2})];
    let new = [json!({"n": 2, "name": "日本"}), json!({"name": "é", "n": 5})];
    assert_same_as_in_memory(&old, &new, "name", &DiffOptions::new());
}

#[test]
fn ignored_paths_apply_inside_rows() {
    let old = [json!({"id": 1, "stamp": 1, "v": 1}), json!({"id": 2, "stamp": 1, "v": 1})];
    let new = [json!({"id": 1, "stamp": 9, "v": 1}), json!({"id": 2, "stamp": 9, "v": 2})];
    let options = DiffOptions::new().ignore_path("/*/stamp");
    let ops = assert_same_as_in_memory(&old, &new, "id", &options);
    assert_eq!(ops.iter().map(|o| o.path.as_str()).collect::<Vec<_>>(), ["/1/v"]);
}

#[test]
fn arrays_inside_rows_can_be_matched_by_key_too() {
    let old = [json!({"id": 1, "items": [{"sku": "a", "q": 1}, {"sku": "b", "q": 2}]})];
    let new = [
        json!({"id": 1, "items": [{"sku": "c", "q": 0}, {"sku": "a", "q": 1}, {"sku": "b", "q": 3}]}),
    ];
    let ops = assert_same_as_in_memory(&old, &new, "id", &DiffOptions::new().array_key("sku"));
    assert_eq!(ops.len(), 2);
}

#[test]
fn random_record_sets_give_exactly_the_in_memory_diff() {
    // xorshift: deterministic, no extra dependencies.
    let mut state = 0x6A09E667F3BCC909u64;
    let mut next = move |n: u64| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state % n
    };
    let options = DiffOptions::new().array_key("sku").ignore_path("/*/stamp");
    for round in 0..300 {
        let make = |next: &mut dyn FnMut(u64) -> u64| -> Vec<Value> {
            let mut ids: Vec<u64> = (0..10).filter(|_| next(3) != 0).collect();
            for i in (1..ids.len()).rev() {
                ids.swap(i, next(i as u64 + 1) as usize);
            }
            ids.into_iter()
                .map(|id| {
                    let mut skus: Vec<u64> = (0..4).filter(|_| next(2) == 0).collect();
                    if next(2) == 0 {
                        skus.reverse();
                    }
                    let items: Vec<Value> =
                        skus.iter().map(|s| json!({"sku": s, "q": next(3)})).collect();
                    json!({"id": id, "v": next(3), "stamp": next(5), "items": items,
                           "o": if next(2) == 0 { json!({"a": next(2)}) } else { Value::Null }})
                })
                .collect()
        };
        let (old, new) = (make(&mut next), make(&mut next));
        let dir = Dir::new();
        let streamed =
            diff_records(&dir.rows("o.ndjson", &old), &dir.rows("n.ndjson", &new), "id", &options)
                .unwrap();
        assert_eq!(
            streamed,
            reference(&old, &new, "id", &options),
            "round {round}\n{old:?}\n{new:?}"
        );
    }
}

#[test]
fn a_large_mostly_unchanged_file_is_diffed_quickly() {
    let old: Vec<Value> = (0..30_000).map(|i| row(i, i % 97)).collect();
    let new: Vec<Value> =
        (0..30_000).map(|i| row(i, if i % 50 == 0 { 1000 } else { i % 97 })).collect();
    let dir = Dir::new();
    let started = std::time::Instant::now();
    let ops = diff_records(
        &dir.rows("o.ndjson", &old),
        &dir.rows("n.ndjson", &new),
        "id",
        &DiffOptions::new(),
    )
    .unwrap();
    assert_eq!(ops.len(), 600);
    assert!(started.elapsed().as_secs_f64() < 20.0, "took {:?}", started.elapsed());
}

#[test]
fn a_row_without_the_key_is_reported_with_its_line() {
    let dir = Dir::new();
    let good = dir.text("good.ndjson", "{\"id\":1}\n");
    let bad = dir.text("bad.ndjson", "{\"id\":1}\n\n{\"name\":\"x\"}\n");
    let error = diff_records(&good, &bad, "id", &DiffOptions::new()).unwrap_err().to_string();
    assert!(
        error.contains("line 3") && error.contains("`id`") && error.contains("bad.ndjson"),
        "{error}"
    );
}

#[test]
fn a_key_that_is_not_a_scalar_is_an_error() {
    let dir = Dir::new();
    let ok = dir.text("ok.ndjson", "{\"id\":1}\n");
    for text in ["{\"id\":{\"a\":1}}\n", "{\"id\":[1]}\n", "{\"id\":null}\n"] {
        let bad = dir.text("bad.ndjson", text);
        let error = diff_records(&ok, &bad, "id", &DiffOptions::new()).unwrap_err().to_string();
        assert!(error.contains("`id`") && error.contains("line 1"), "{text}: {error}");
    }
}

#[test]
fn a_duplicate_key_is_an_error_naming_both_lines() {
    let dir = Dir::new();
    let ok = dir.text("ok.ndjson", "{\"id\":1}\n");
    let dup = dir.text("dup.ndjson", "{\"id\":1}\n{\"id\":2}\n\n{\"id\":1}\n");
    let error = diff_records(&ok, &dup, "id", &DiffOptions::new()).unwrap_err().to_string();
    assert!(
        error.contains("duplicate") && error.contains("line 1") && error.contains("line 4"),
        "{error}"
    );
    // Duplicates in the old file are caught too.
    assert!(diff_records(&dup, &ok, "id", &DiffOptions::new()).is_err());
}

#[test]
fn invalid_json_and_non_object_rows_are_errors_with_the_line() {
    let dir = Dir::new();
    let ok = dir.text("ok.ndjson", "{\"id\":1}\n");
    let broken = dir.text("broken.ndjson", "{\"id\":1}\n{\"id\":2\n");
    let error = diff_records(&ok, &broken, "id", &DiffOptions::new()).unwrap_err().to_string();
    assert!(error.contains("line 2"), "{error}");
    let scalar = dir.text("scalar.ndjson", "{\"id\":1}\n5\n");
    let error = diff_records(&ok, &scalar, "id", &DiffOptions::new()).unwrap_err().to_string();
    assert!(error.contains("line 2") && error.contains("object"), "{error}");
}

#[test]
fn a_missing_file_is_an_io_error() {
    let dir = Dir::new();
    let ok = dir.text("ok.ndjson", "{\"id\":1}\n");
    let missing = dir.0.join("nope.ndjson");
    assert!(matches!(
        diff_records(&ok, &missing, "id", &DiffOptions::new()),
        Err(DriftError::Io(_))
    ));
}

fn key(field: &str) -> KeySpec {
    KeySpec::parse(&RecordOptions::new().key(field)).unwrap()
}

#[test]
fn the_index_is_the_same_however_the_file_is_cut_up() {
    // xorshift: deterministic, no extra dependencies.
    let mut state = 0x243F6A8885A308D3u64;
    let mut next = move |n: u64| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state % n
    };
    for round in 0..100 {
        // Rows of very different lengths, blank lines, CRLF and no final newline.
        let mut text = String::new();
        let rows = next(60) + 1;
        for id in 0..rows {
            let pad = "x".repeat(next(200) as usize);
            text.push_str(&format!("{{\"id\":{id},\"pad\":\"{pad}\"}}"));
            text.push_str(["\n", "\r\n", "\n\n", "\n  \n\r\n"][next(4) as usize]);
        }
        if next(2) == 0 {
            text = text.trim_end().to_string();
        }
        let dir = Dir::new();
        let path = dir.text("rows.ndjson", &text);
        let single = Rows::index_with(&path, &key("id"), 1).unwrap();
        assert_eq!(single.len() as u64, rows, "round {round}");
        for threads in 2..=8 {
            let split = Rows::index_with(&path, &key("id"), threads).unwrap();
            assert_eq!(split.keys, single.keys, "round {round}, {threads} threads");
            assert_eq!(split.hashes, single.hashes, "round {round}, {threads} threads");
            assert_eq!(split.offsets, single.offsets, "round {round}, {threads} threads");
        }
    }
}

#[test]
fn a_bad_row_in_any_part_of_a_split_file_is_reported_with_its_own_line() {
    let dir = Dir::new();
    let mut text = String::new();
    for id in 0..200 {
        text.push_str(&format!("{{\"id\":{id},\"pad\":\"{}\"}}\n", "x".repeat(50)));
    }
    let lines: Vec<&str> = text.lines().collect();
    for bad in [0usize, 1, 57, 100, 143, 198, 199] {
        let mut broken = lines.clone();
        broken[bad] = "{\"id\": nope}";
        let path = dir.text("broken.ndjson", &broken.join("\n"));
        for threads in [1, 4, 8] {
            let error = Rows::index_with(&path, &key("id"), threads).err().unwrap().to_string();
            assert!(
                error.contains(&format!("line {}:", bad + 1)),
                "{threads} threads, row {bad}: {error}"
            );
        }
    }
}

#[test]
fn reading_only_the_key_agrees_with_parsing_the_whole_row() {
    // xorshift: deterministic, no extra dependencies.
    let mut state = 0xB7E151628AED2A6Au64;
    let mut next = move |n: u64| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state % n
    };
    fn value(depth: usize, next: &mut dyn FnMut(u64) -> u64) -> Value {
        match if depth == 0 { next(4) } else { next(7) } {
            0 => json!(next(5)),
            1 => json!(["k", "id", "a b"][next(3) as usize]),
            2 => json!(next(2) == 0),
            3 => Value::Null,
            4 => Value::Array((0..next(3)).map(|_| value(depth - 1, next)).collect()),
            _ => {
                let mut map = serde_json::Map::new();
                for name in ["id", "k", "a", "user", "b"] {
                    if next(3) != 0 {
                        map.insert(name.to_string(), value(depth - 1, next));
                    }
                }
                Value::Object(map)
            }
        }
    }
    for field in ["id", "/user/id", "/a/b/k"] {
        let spec = key(field);
        let pointer = if field.starts_with('/') { field.to_string() } else { format!("/{field}") };
        for round in 0..400 {
            let row = value(3, &mut next);
            let line = row.to_string();
            let expected = match &row {
                Value::Object(_) => match row.pointer(&pointer) {
                    None => "no key".to_string(),
                    Some(v @ (Value::String(_) | Value::Number(_) | Value::Bool(_))) => {
                        format!("key {v}")
                    }
                    Some(_) => "bad key".to_string(),
                },
                _ => "not an object".to_string(),
            };
            let got = match read_key(line.as_bytes(), &spec) {
                Ok(parts) => format!("key {}", parts[0]),
                Err(Fault::NoKey(_)) => "no key".to_string(),
                Err(Fault::BadKey(_)) => "bad key".to_string(),
                Err(Fault::NotObject) => "not an object".to_string(),
                Err(Fault::Json(e)) => panic!("{field} round {round}: {e}: {line}"),
            };
            assert_eq!(got, expected, "{field} round {round}: {line}");
        }
    }
}

#[test]
fn a_key_given_as_a_json_pointer_reaches_into_the_row() {
    let old = [json!({"user": {"id": 1}, "v": 1}), json!({"user": {"id": 2}, "v": 2})];
    let new = [json!({"user": {"id": 2}, "v": 5}), json!({"user": {"id": 1}, "v": 1})];
    let dir = Dir::new();
    let ops = diff_records(
        &dir.rows("o.ndjson", &old),
        &dir.rows("n.ndjson", &new),
        "/user/id",
        &DiffOptions::new(),
    )
    .unwrap();
    // Row 2 moves to the front and changes; row 1 stays as it was.
    assert_eq!(
        ops.iter().map(|o| (o.op.as_str(), o.path.as_str())).collect::<Vec<_>>(),
        [("move", "/0"), ("replace", "/0/v")]
    );
}

// ----- composite keys, duplicates and whole-row keys ---------------------------

use crate::diff::{plan_keyed, walk, Context, KeyedStep};
use std::collections::HashMap;

/// An independent, deliberately naive version of the matching rules, to check
/// the real one against. It shares only the planner and the row walk.
fn canon(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted: std::collections::BTreeMap<_, _> =
                map.iter().map(|(k, v)| (k.clone(), canon(v))).collect();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(items) => Value::Array(items.iter().map(canon).collect()),
        other => other.clone(),
    }
}

/// The rows that take part, each with the key it is matched by.
fn select(rows: &[Value], options: &RecordOptions) -> Vec<(String, Value)> {
    let keyed: Vec<(String, Value)> = rows
        .iter()
        .map(|row| {
            let key = if options.whole_row {
                canon(row).to_string()
            } else {
                let parts: Vec<Value> = options
                    .key
                    .iter()
                    .map(|spec| {
                        let pointer =
                            if spec.starts_with('/') { spec.clone() } else { format!("/{spec}") };
                        row.pointer(&pointer).cloned().expect("test rows have their keys")
                    })
                    .collect();
                Value::Array(parts).to_string()
            };
            (key, row.clone())
        })
        .collect();
    let policy = options.duplicates.unwrap_or(if options.whole_row {
        Duplicates::Occurrence
    } else {
        Duplicates::Error
    });
    let mut seen: HashMap<String, usize> = HashMap::new();
    for (key, _) in &keyed {
        *seen.entry(key.clone()).or_default() += 1;
    }
    let mut count: HashMap<String, usize> = HashMap::new();
    keyed
        .into_iter()
        .filter_map(|(key, row)| {
            let nth = *count.entry(key.clone()).and_modify(|n| *n += 1).or_insert(0);
            let total = seen[&key];
            match policy {
                Duplicates::Error => {
                    assert_eq!(total, 1, "the oracle is only used on unique keys");
                    Some((key, row))
                }
                Duplicates::First => (nth == 0).then_some((key, row)),
                Duplicates::Last => (nth + 1 == total).then_some((key, row)),
                Duplicates::Occurrence => Some((format!("{key}#{nth}"), row)),
            }
        })
        .collect()
}

fn oracle(old: &[Value], new: &[Value], options: &RecordOptions) -> Vec<Delta> {
    let (old, new) = (select(old, options), select(new, options));
    let index: HashMap<&str, usize> =
        old.iter().enumerate().map(|(i, (k, _))| (k.as_str(), i)).collect();
    let matches: Vec<Option<usize>> =
        new.iter().map(|(k, _)| index.get(k.as_str()).copied()).collect();
    let ctx = Context::new(&options.diff);
    let at = |i: usize| join_pointer("", &i.to_string());
    let mut ops = Vec::new();
    plan_keyed(old.len(), &matches, |step| match step {
        KeyedStep::Remove(i) => ops.push(Delta::new(Operation::Remove, at(i))),
        KeyedStep::Move { from, to } => ops.push(Delta {
            op: Operation::Move,
            path: at(to),
            value: None,
            from_path: Some(at(from)),
        }),
        KeyedStep::Add(n) => ops.push(Delta::with_value(Operation::Add, at(n), new[n].1.clone())),
        KeyedStep::Pair { old: o, new: n } => walk(&new[n].1, &old[o].1, &at(n), &ctx, &mut ops),
    });
    ops
}

/// Streams the rows from files and checks the result against the oracle, and
/// that it turns the selected old rows into the selected new ones.
fn assert_matches_oracle(old: &[Value], new: &[Value], options: &RecordOptions) -> Vec<Delta> {
    let dir = Dir::new();
    let (old_file, new_file) = (dir.rows("old.ndjson", old), dir.rows("new.ndjson", new));
    let streamed = diff_records_with(&old_file, &new_file, options)
        .unwrap_or_else(|e| panic!("{e}: {old:?} -> {new:?}"));
    assert_eq!(streamed, oracle(old, new, options), "{old:?} -> {new:?}");
    if options.diff.ignore_paths.is_empty() {
        let kept = |rows: &[Value]| -> Vec<Value> {
            select(rows, options).into_iter().map(|(_, row)| row).collect()
        };
        let patched = patch(Value::Array(kept(old)), &streamed).unwrap();
        assert_eq!(patched, Value::Array(kept(new)), "{streamed:?}");
    }
    streamed
}

fn region_row(id: u64, region: &str, v: u64) -> Value {
    json!({"id": id, "region": region, "v": v, "meta": {"zone": region}})
}

#[test]
fn a_key_can_be_several_fields_and_a_row_is_a_different_row_in_another_region() {
    let options = RecordOptions::new().key("id").key("region");
    let old = [region_row(1, "eu", 1), region_row(1, "us", 2), region_row(2, "eu", 3)];
    let new = [
        region_row(2, "eu", 3),
        region_row(1, "us", 9),
        region_row(1, "eu", 1),
        region_row(3, "eu", 0),
    ];
    let ops = assert_matches_oracle(&old, &new, &options);
    // (1, us) changed, (2, eu) moved, (3, eu) is new: nothing is confused with (1, eu).
    assert!(ops.iter().any(|o| o.path.ends_with("/v")), "{ops:?}");
    assert!(ops.iter().any(|o| o.op == Operation::Add), "{ops:?}");
    // With only `id` as the key the same files are a duplicate-key error.
    let dir = Dir::new();
    let error = diff_records_with(
        &dir.rows("o.ndjson", &old),
        &dir.rows("n.ndjson", &new),
        &RecordOptions::new().key("id"),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("duplicate"), "{error}");
}

#[test]
fn composite_keys_can_mix_fields_and_pointers() {
    let options = RecordOptions::new().key("id").key("/meta/zone");
    let old = [region_row(1, "eu", 1), region_row(1, "us", 2)];
    let new = [region_row(1, "us", 3), region_row(1, "eu", 1)];
    assert_matches_oracle(&old, &new, &options);
}

#[test]
fn composite_key_problems_name_the_part_that_is_wrong() {
    let dir = Dir::new();
    let ok = dir.text("ok.ndjson", "{\"id\":1,\"region\":\"eu\"}\n");
    let options = RecordOptions::new().key("id").key("region");
    let no_region = dir.text("a.ndjson", "{\"id\":1}\n");
    let error = diff_records_with(&ok, &no_region, &options).unwrap_err().to_string();
    assert!(error.contains("`region`") && error.contains("line 1"), "{error}");
    let bad_region = dir.text("b.ndjson", "{\"id\":1,\"region\":[1]}\n");
    let error = diff_records_with(&ok, &bad_region, &options).unwrap_err().to_string();
    assert!(error.contains("`region`") && error.contains("string, number or boolean"), "{error}");
    // Parts that contain one another, or repeat, cannot be told apart.
    for keys in [["/a", "/a/b"], ["id", "id"], ["/a/b", "/a"]] {
        let options = RecordOptions::new().key(keys[0]).key(keys[1]);
        let error = diff_records_with(&ok, &ok, &options).unwrap_err().to_string();
        assert!(error.contains("overlap"), "{keys:?}: {error}");
    }
    // And there has to be some key.
    assert!(diff_records_with(&ok, &ok, &RecordOptions::new()).is_err());
}

#[test]
fn duplicate_keys_can_keep_the_first_the_last_or_pair_them_up() {
    let old = [json!({"id": 1, "v": "a"}), json!({"id": 2, "v": "b"}), json!({"id": 1, "v": "c"})];
    let new = [
        json!({"id": 1, "v": "d"}),
        json!({"id": 1, "v": "c"}),
        json!({"id": 2, "v": "b"}),
        json!({"id": 1, "v": "e"}),
    ];
    for policy in [Duplicates::First, Duplicates::Last, Duplicates::Occurrence] {
        assert_matches_oracle(&old, &new, &RecordOptions::new().key("id").duplicates(policy));
    }
    // First keeps the first row of each key: old is [1:a, 2:b], new is [1:d, 2:b].
    let ops = assert_matches_oracle(
        &old,
        &new,
        &RecordOptions::new().key("id").duplicates(Duplicates::First),
    );
    assert_eq!(ops.iter().map(|o| o.path.as_str()).collect::<Vec<_>>(), ["/0/v"]);
    // Last keeps the last: old is [2:b, 1:c], new is [2:b, 1:e].
    let ops = assert_matches_oracle(
        &old,
        &new,
        &RecordOptions::new().key("id").duplicates(Duplicates::Last),
    );
    assert_eq!(ops.iter().map(|o| o.path.as_str()).collect::<Vec<_>>(), ["/1/v"]);
    // Occurrence pairs the nth row of a key with the nth row of that key.
    let ops = assert_matches_oracle(
        &old,
        &new,
        &RecordOptions::new().key("id").duplicates(Duplicates::Occurrence),
    );
    assert!(!ops.is_empty());
}

#[test]
fn rows_without_a_key_are_matched_by_their_whole_content() {
    let options = RecordOptions::new().whole_row();
    let old = [json!({"a": 1, "b": [1, 2]}), json!({"a": 2}), json!({"a": 3})];
    let new = [json!({"b": [1, 2], "a": 1}), json!({"a": 3}), json!({"a": 9}), json!({"a": 3})];
    let ops = assert_matches_oracle(&old, &new, &options);
    // A changed row is a removal and an addition, never an in-place edit.
    assert!(ops.iter().all(|o| o.op != Operation::Replace), "{ops:?}");
    assert!(ops.iter().any(|o| o.op == Operation::Remove), "{ops:?}");
    // Identical files have no differences, however many identical rows they hold.
    let same = [json!({"a": 1}), json!({"a": 1}), json!({"a": 1})];
    assert!(assert_matches_oracle(&same, &same, &options).is_empty());
    // The count of identical rows matters: one fewer is one removal.
    let ops = assert_matches_oracle(&same, &same[..2], &options);
    assert_eq!(ops.len(), 1);
}

#[test]
fn whole_row_mode_takes_rows_that_are_not_objects() {
    let options = RecordOptions::new().whole_row();
    let old = [json!(1), json!("two"), json!([3]), json!(null)];
    let new = [json!(null), json!(1), json!([3, 4])];
    assert_matches_oracle(&old, &new, &options);
}

#[test]
fn a_key_and_whole_row_mode_together_are_refused() {
    let dir = Dir::new();
    let ok = dir.text("ok.ndjson", "{\"id\":1}\n");
    assert!(diff_records_with(&ok, &ok, &RecordOptions::new().key("id").whole_row()).is_err());
}

#[test]
fn random_files_with_composite_keys_duplicates_and_no_key_match_the_oracle() {
    // xorshift: deterministic, no extra dependencies.
    let mut state = 0x510E527FADE682D1u64;
    let mut next = move |n: u64| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state % n
    };
    let configs = [
        RecordOptions::new().key("id").key("region"),
        RecordOptions::new().key("/meta/n").duplicates(Duplicates::First),
        RecordOptions::new().key("id").duplicates(Duplicates::Last),
        RecordOptions::new().key("id").key("region").duplicates(Duplicates::Occurrence),
        RecordOptions::new().whole_row(),
        RecordOptions::new().whole_row().diff(DiffOptions::new().ignore_path("/*/v")),
    ];
    for round in 0..200 {
        let options = &configs[round % configs.len()];
        let unique = options.duplicates.is_none() && !options.whole_row;
        let make = |next: &mut dyn FnMut(u64) -> u64| -> Vec<Value> {
            let mut rows: Vec<Value> = (0..next(12))
                .map(|_| {
                    let (id, region) = (next(5), ["eu", "us"][next(2) as usize]);
                    let (v, n, t) = (next(3), next(4), [next(2), next(2)]);
                    json!({"id": id, "region": region, "v": v, "meta": {"n": n}, "t": t})
                })
                .collect();
            if unique {
                // Composite keys must still be unique when no policy says otherwise.
                let mut seen = std::collections::HashSet::new();
                rows.retain(|r| seen.insert((r["id"].clone(), r["region"].clone())));
            }
            rows
        };
        let (old, new) = (make(&mut next), make(&mut next));
        assert_matches_oracle(&old, &new, options);
    }
}

#[test]
fn the_whole_row_fingerprint_ignores_member_order_but_nothing_else() {
    let same = [
        (
            json!({"a": 1, "b": [1, 2], "c": {"x": 1, "y": 2}}),
            json!({"c": {"y": 2, "x": 1}, "b": [1, 2], "a": 1}),
        ),
        (json!({}), json!({})),
        (json!("é"), json!("\u{00e9}")),
    ];
    for (a, b) in same {
        assert_eq!(fingerprint(&a), fingerprint(&b), "{a} vs {b}");
    }
    let different = [
        (json!({"a": 1}), json!({"a": 2})),
        (json!({"a": 1}), json!({"b": 1})),
        (json!(1), json!(1.0)),
        (json!(1), json!("1")),
        (json!(true), json!(1)),
        (json!(null), json!(false)),
        (json!([1, 2]), json!([2, 1])),
        (json!([]), json!({})),
        (json!({"a": {"b": 1}}), json!({"a": {"b": 2}})),
        (json!({"a": 1, "b": 2}), json!({"a": 2, "b": 1})),
        (json!(""), json!(null)),
    ];
    for (a, b) in different {
        assert_ne!(fingerprint(&a), fingerprint(&b), "{a} vs {b}");
    }
}

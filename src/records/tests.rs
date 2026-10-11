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
    #[cfg(feature = "csv")]
    fn path(&self, name: &str) -> std::path::PathBuf {
        self.0.join(name)
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
                Err(Fault::Io(e)) => panic!("{field} round {round}: {e}: {line}"),
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

// ----- CSV and TSV ---------------------------------------------------------------

#[cfg(feature = "csv")]
mod csv_format {
    use super::*;
    use crate::{ColumnType, CsvOptions, RecordFormat};

    /// CSV text written by the `csv` crate, so quoting is not our own invention.
    fn text(header: &[&str], rows: &[Vec<&str>], delimiter: u8) -> String {
        let mut writer = ::csv::WriterBuilder::new().delimiter(delimiter).from_writer(Vec::new());
        writer.write_record(header).unwrap();
        for row in rows {
            writer.write_record(row).unwrap();
        }
        String::from_utf8(writer.into_inner().unwrap()).unwrap()
    }

    /// What a row should read as: an object of its cells, converted by column type.
    fn object(header: &[&str], cells: &[&str], types: &[(&str, ColumnType)]) -> Value {
        let mut map = serde_json::Map::new();
        for (name, cell) in header.iter().zip(cells) {
            let kind = types.iter().find(|(n, _)| n == name).map(|(_, k)| *k);
            let value = match kind {
                None | Some(ColumnType::String) => json!(cell),
                Some(_) if cell.is_empty() => Value::Null,
                Some(ColumnType::Int) => match cell.parse::<i64>() {
                    Ok(i) => json!(i),
                    Err(_) => json!(cell.parse::<u64>().unwrap()),
                },
                Some(ColumnType::Float) => json!(cell.parse::<f64>().unwrap()),
                Some(ColumnType::Bool) => json!(cell.eq_ignore_ascii_case("true")),
                Some(ColumnType::Json) => serde_json::from_str(cell).unwrap(),
            };
            map.insert(name.to_string(), value);
        }
        Value::Object(map)
    }

    fn csv_options(types: &[(&str, ColumnType)]) -> CsvOptions {
        types.iter().fold(CsvOptions::new(), |o, (name, kind)| o.column(*name, *kind))
    }

    fn with_csv(options: RecordOptions, csv: CsvOptions) -> RecordOptions {
        options.format(RecordFormat::Csv(csv))
    }

    /// Diffs two CSV texts and checks the result against the oracle run on the same
    /// rows as objects.
    fn assert_csv_matches_oracle(
        header: &[&str],
        old: &[Vec<&str>],
        new: &[Vec<&str>],
        options: &RecordOptions,
        types: &[(&str, ColumnType)],
    ) -> Vec<Delta> {
        let dir = Dir::new();
        let (old_file, new_file) = (
            dir.text("old.csv", &text(header, old, b',')),
            dir.text("new.csv", &text(header, new, b',')),
        );
        let streamed = diff_records_with(&old_file, &new_file, options)
            .unwrap_or_else(|e| panic!("{e}: {old:?} -> {new:?}"));
        let rows = |data: &[Vec<&str>]| -> Vec<Value> {
            data.iter().map(|cells| object(header, cells, types)).collect()
        };
        assert_eq!(streamed, oracle(&rows(old), &rows(new), options), "{old:?} -> {new:?}");
        streamed
    }

    const HEADER: [&str; 4] = ["id", "name", "note", "score"];

    #[test]
    fn csv_rows_are_diffed_like_the_same_rows_as_objects() {
        let options = with_csv(RecordOptions::new().key("id"), CsvOptions::new());
        let old = vec![
            vec!["1", "Ada", "plain", "10"],
            vec!["2", "Grace", "has, comma", "20"],
            vec!["3", "Linus", "say \"hi\"", "30"],
            vec!["4", "Café ☃", "two\nlines", ""],
        ];
        let cases: Vec<Vec<Vec<&str>>> = vec![
            old.clone(),
            vec![
                vec!["0", "New", "", "0"],
                old[0].clone(),
                old[1].clone(),
                old[2].clone(),
                old[3].clone(),
            ],
            vec![
                old[0].clone(),
                vec!["2", "Grace", "has, comma", "21"],
                old[2].clone(),
                old[3].clone(),
            ],
            vec![old[3].clone(), old[2].clone(), old[1].clone(), old[0].clone()],
            vec![old[1].clone(), old[3].clone()],
            vec![
                old[0].clone(),
                old[1].clone(),
                old[2].clone(),
                vec!["4", "Café ☃", "one line", ""],
            ],
            vec![],
        ];
        for new in cases {
            assert_csv_matches_oracle(&HEADER, &old, &new, &options, &[]);
        }
        assert_csv_matches_oracle(&HEADER, &[], &old, &options, &[]);
    }

    #[test]
    fn cells_are_strings_unless_a_column_is_given_a_type() {
        let types = [
            ("id", ColumnType::Int),
            ("score", ColumnType::Float),
            ("active", ColumnType::Bool),
            ("meta", ColumnType::Json),
        ];
        let header = ["id", "score", "active", "meta", "name"];
        let old =
            vec![vec!["1", "1.5", "true", "{\"a\":[1]}", "x"], vec!["2", "", "FALSE", "", "007"]];
        let new = vec![
            vec!["1", "2.5", "true", "{\"a\":[1,2]}", "x"],
            vec!["2", "", "false", "null", "007"],
        ];
        let options = with_csv(RecordOptions::new().key("id"), csv_options(&types));
        let ops = assert_csv_matches_oracle(&header, &old, &new, &options, &types);
        // The new score is a number, not the text "2.5"; "007" stays text.
        let score = ops.iter().find(|o| o.path == "/0/score").expect("score changed");
        assert_eq!(score.value, Some(json!(2.5)));
        assert!(ops.iter().all(|o| o.path != "/1/name"));
        // Without the types the same files compare as strings.
        let plain = with_csv(RecordOptions::new().key("id"), CsvOptions::new());
        let ops = assert_csv_matches_oracle(&header, &old, &new, &plain, &[]);
        let score = ops.iter().find(|o| o.path == "/0/score").expect("score changed");
        assert_eq!(score.value, Some(json!("2.5")));
    }

    #[test]
    fn the_key_can_be_a_typed_column_and_one_and_one_point_zero_differ() {
        let types = [("id", ColumnType::Int)];
        let old = vec![vec!["1", "a", "", ""], vec!["2", "b", "", ""]];
        let new = vec![vec!["2", "b", "", ""], vec!["1", "A", "", ""]];
        let options = with_csv(RecordOptions::new().key("id"), csv_options(&types));
        assert_csv_matches_oracle(&HEADER, &old, &new, &options, &types);
    }

    #[test]
    fn a_byte_order_mark_windows_line_endings_and_blank_lines_are_handled() {
        let dir = Dir::new();
        let old = dir.text("old.csv", "\u{feff}id,v\r\n1,a\r\n\r\n2,b\r\n");
        let new = dir.text("new.csv", "id,v\n1,a\n\n2,B\n\n");
        let options = with_csv(RecordOptions::new().key("id"), CsvOptions::new());
        let ops = diff_records_with(&old, &new, &options).unwrap();
        // Rows are numbered without the header and the blank lines.
        assert_eq!(ops.iter().map(|o| o.path.as_str()).collect::<Vec<_>>(), ["/1/v"]);
    }

    #[test]
    fn a_file_with_only_a_header_or_nothing_has_no_rows() {
        let dir = Dir::new();
        let rows = dir.text("rows.csv", "id,v\n1,a\n2,b\n");
        let header_only = dir.text("header.csv", "id,v\n");
        let empty = dir.text("empty.csv", "");
        let options = with_csv(RecordOptions::new().key("id"), CsvOptions::new());
        assert!(diff_records_with(&header_only, &header_only, &options).unwrap().is_empty());
        assert!(diff_records_with(&empty, &empty, &options).unwrap().is_empty());
        let ops = diff_records_with(&header_only, &rows, &options).unwrap();
        assert_eq!(ops.iter().map(|o| o.op.as_str()).collect::<Vec<_>>(), ["add", "add"]);
        let ops = diff_records_with(&rows, &empty, &options).unwrap();
        assert_eq!(ops.iter().map(|o| o.op.as_str()).collect::<Vec<_>>(), ["remove", "remove"]);
    }

    #[test]
    fn the_two_files_may_have_different_columns() {
        let dir = Dir::new();
        let old = dir.text("old.csv", "id,price\n1,10\n2,20\n");
        let new = dir.text("new.csv", "id,cost,currency\n1,10,EUR\n2,25,EUR\n");
        let options = with_csv(RecordOptions::new().key("id"), CsvOptions::new());
        let ops = diff_records_with(&old, &new, &options).unwrap();
        // price became cost (a removal and an addition), and a column was added.
        let paths: Vec<_> = ops.iter().map(|o| (o.op.as_str(), o.path.as_str())).collect();
        assert!(paths.contains(&("remove", "/0/price")), "{paths:?}");
        assert!(paths.contains(&("add", "/0/cost")), "{paths:?}");
        assert!(paths.contains(&("add", "/1/currency")), "{paths:?}");
        // Rows with the same cells but different columns are not mistaken for equal ones.
        assert!(!ops.is_empty());
    }

    #[test]
    fn tabs_and_other_delimiters_work() {
        let dir = Dir::new();
        for (delimiter, name) in [(b'\t', "t.tsv"), (b';', "s.csv"), (b'|', "p.csv")] {
            let old = dir.text(
                &format!("old-{name}"),
                &text(&["id", "v"], &[vec!["1", "a;b"], vec!["2", "c"]], delimiter),
            );
            let new = dir.text(
                &format!("new-{name}"),
                &text(&["id", "v"], &[vec!["1", "a;b"], vec!["2", "d"]], delimiter),
            );
            let options =
                with_csv(RecordOptions::new().key("id"), CsvOptions::new().delimiter(delimiter));
            let ops = diff_records_with(&old, &new, &options).unwrap();
            assert_eq!(ops.iter().map(|o| o.path.as_str()).collect::<Vec<_>>(), ["/1/v"], "{name}");
        }
    }

    #[test]
    fn a_row_with_a_quoted_newline_is_one_row_and_lines_are_counted_physically() {
        let dir = Dir::new();
        let good = dir.text("good.csv", "id,v\n1,\"a\nb\"\n2,c\n");
        let changed = dir.text("changed.csv", "id,v\n1,\"a\nB\"\n2,c\n");
        let options = with_csv(RecordOptions::new().key("id"), CsvOptions::new());
        let ops = diff_records_with(&good, &changed, &options).unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].path, "/0/v");
        assert_eq!(ops[0].value, Some(json!("a\nB")));
        // Row 3 starts on physical line 5, after a two-line row.
        let ragged = dir.text("ragged.csv", "id,v\n1,\"a\nb\"\n2\n");
        let error = diff_records_with(&good, &ragged, &options).unwrap_err().to_string();
        assert!(
            error.contains("line 4") && error.contains("expected 2 fields, found 1"),
            "{error}"
        );
    }

    #[test]
    fn csv_problems_are_reported_with_the_file_the_line_and_the_column() {
        let dir = Dir::new();
        let ok = dir.text("ok.csv", "id,v\n1,a\n");
        let options = with_csv(RecordOptions::new().key("id"), CsvOptions::new());
        let message = |content: &str, options: &RecordOptions| {
            let bad = dir.text("bad.csv", content);
            diff_records_with(&ok, &bad, options).unwrap_err().to_string()
        };
        let error = message("id,v\n1,a\n2\n", &options);
        assert!(
            error.contains("bad.csv")
                && error.contains("line 3")
                && error.contains("expected 2 fields, found 1"),
            "{error}"
        );
        let error = message("id,v\n1,a,extra\n", &options);
        assert!(error.contains("line 2") && error.contains("found 3"), "{error}");
        let error = message("id,id\n1,a\n", &options);
        assert!(
            error.contains("line 1") && error.contains("`id`") && error.contains("twice"),
            "{error}"
        );
        let error = message("id,\n1,a\n", &options);
        assert!(error.contains("line 1") && error.contains("empty"), "{error}");
        // The key column has to exist, and be named as the user wrote it.
        let error = message("code,v\n1,a\n", &options);
        assert!(error.contains("`id`") && error.contains("bad.csv"), "{error}");
        // CSV rows are flat: a nested pointer cannot name anything.
        let nested = with_csv(RecordOptions::new().key("/a/b"), CsvOptions::new());
        let error = diff_records_with(&ok, &ok, &nested).unwrap_err().to_string();
        assert!(error.contains("flat") || error.contains("nested"), "{error}");
        // Not valid UTF-8.
        let invalid = dir.path("invalid.csv");
        std::fs::write(&invalid, b"id,v\n1,\xff\xfe\n").unwrap();
        let error = diff_records_with(&ok, &invalid, &options).unwrap_err().to_string();
        assert!(error.contains("line 2") && error.contains("UTF-8"), "{error}");
    }

    #[test]
    fn a_cell_that_does_not_fit_its_column_type_is_an_error_naming_the_cell() {
        let dir = Dir::new();
        let ok = dir.text("ok.csv", "id,n\n1,5\n");
        for (kind, bad_cell, word) in [
            (ColumnType::Int, "five", "integer"),
            (ColumnType::Int, "1.5", "integer"),
            (ColumnType::Float, "x", "number"),
            (ColumnType::Float, "NaN", "number"),
            (ColumnType::Bool, "yes", "boolean"),
            (ColumnType::Json, "{oops", "JSON"),
        ] {
            let bad = dir.text("bad.csv", &format!("id,n\n1,{bad_cell}\n"));
            let options =
                with_csv(RecordOptions::new().key("id"), CsvOptions::new().column("n", kind));
            // A cell of the right type for the file that is fine.
            let fine = match kind {
                ColumnType::Bool => "true",
                ColumnType::Json => "5",
                _ => "1",
            };
            let ok_typed = dir.text("ok2.csv", &format!("id,n\n1,{fine}\n"));
            let error = diff_records_with(&ok_typed, &bad, &options).unwrap_err().to_string();
            assert!(
                error.contains("line 2")
                    && error.contains("`n`")
                    && error.contains(bad_cell)
                    && error.contains(word),
                "{kind:?} {bad_cell}: {error}"
            );
        }
        // A typed column that is not in the file is a mistake worth saying.
        let options = with_csv(
            RecordOptions::new().key("id"),
            CsvOptions::new().column("nope", ColumnType::Int),
        );
        let error = diff_records_with(&ok, &ok, &options).unwrap_err().to_string();
        assert!(error.contains("`nope`") && error.contains("ok.csv"), "{error}");
    }

    #[test]
    fn composite_keys_duplicate_policies_and_whole_rows_work_for_csv_too() {
        let old = vec![
            vec!["1", "eu", "x", "1"],
            vec!["1", "us", "y", "2"],
            vec!["2", "eu", "z", "3"],
            vec!["1", "eu", "dup", "4"],
        ];
        let new = vec![
            vec!["2", "eu", "z", "3"],
            vec!["1", "us", "y", "9"],
            vec!["1", "eu", "x", "1"],
            vec!["1", "eu", "dup2", "4"],
        ];
        let header = ["id", "region", "note", "score"];
        for options in [
            RecordOptions::new().key("id").key("region").duplicates(Duplicates::First),
            RecordOptions::new().key("id").key("region").duplicates(Duplicates::Last),
            RecordOptions::new().key("id").key("region").duplicates(Duplicates::Occurrence),
            RecordOptions::new().whole_row(),
            RecordOptions::new()
                .key("id")
                .key("region")
                .duplicates(Duplicates::Last)
                .diff(DiffOptions::new().ignore_path("/*/note")),
        ] {
            assert_csv_matches_oracle(
                &header,
                &old,
                &new,
                &with_csv(options, CsvOptions::new()),
                &[],
            );
        }
    }

    #[test]
    fn random_csv_files_match_the_oracle() {
        // xorshift: deterministic, no extra dependencies.
        let mut state = 0xA4093822299F31D0u64;
        let mut next = move |n: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % n
        };
        const CELLS: [&str; 9] =
            ["", "a", "b c", "x,y", "q\"r", "two\nlines", "ünï", "007", " pad "];
        let header = ["id", "a", "b", "c"];
        for round in 0..150 {
            let make = |next: &mut dyn FnMut(u64) -> u64| -> Vec<Vec<String>> {
                let mut ids: Vec<u64> = (0..9).filter(|_| next(3) != 0).collect();
                for i in (1..ids.len()).rev() {
                    ids.swap(i, next(i as u64 + 1) as usize);
                }
                ids.into_iter()
                    .map(|id| {
                        let mut row = vec![id.to_string()];
                        row.extend((0..3).map(|_| CELLS[next(9) as usize].to_string()));
                        row
                    })
                    .collect()
            };
            let (old, new) = (make(&mut next), make(&mut next));
            fn as_refs(rows: &[Vec<String>]) -> Vec<Vec<&str>> {
                rows.iter().map(|r| r.iter().map(String::as_str).collect()).collect()
            }
            let options = with_csv(RecordOptions::new().key("id"), CsvOptions::new());
            assert_csv_matches_oracle(&header, &as_refs(&old), &as_refs(&new), &options, &[]);
            let _ = round;
        }
    }

    #[test]
    fn many_columns_and_rows_are_fine() {
        let columns: Vec<String> = (0..60).map(|i| format!("c{i}")).collect();
        let header: Vec<&str> = columns.iter().map(String::as_str).collect();
        let cells: Vec<Vec<String>> =
            (0..500).map(|r| (0..60).map(|c| format!("{r}-{c}")).collect()).collect();
        let old: Vec<Vec<&str>> =
            cells.iter().map(|r| r.iter().map(String::as_str).collect()).collect();
        let mut new = old.clone();
        new[250][59] = "changed";
        let options = with_csv(RecordOptions::new().key("c0"), CsvOptions::new());
        let ops = assert_csv_matches_oracle(&header, &old, &new, &options, &[]);
        assert_eq!(ops.len(), 1);
    }

    #[test]
    fn identical_cells_under_a_renamed_column_are_not_equal_rows() {
        // The bytes of the rows are the same; only the header differs, so a row
        // hash that ignored the header would call them equal and skip them.
        let dir = Dir::new();
        let old = dir.text("old.csv", "id,price\n1,10\n2,20\n");
        let new = dir.text("new.csv", "id,cost\n1,10\n2,20\n");
        let options = with_csv(RecordOptions::new().key("id"), CsvOptions::new());
        let ops = diff_records_with(&old, &new, &options).unwrap();
        let paths: Vec<_> = ops.iter().map(|o| (o.op.as_str(), o.path.as_str())).collect();
        assert_eq!(
            paths,
            [
                ("remove", "/0/price"),
                ("add", "/0/cost"),
                ("remove", "/1/price"),
                ("add", "/1/cost")
            ]
        );
        // The same goes for a column that only changes type.
        let typed = with_csv(
            RecordOptions::new().key("id"),
            CsvOptions::new().column("price", ColumnType::Int),
        );
        let same = dir.text("same.csv", "id,price\n1,10\n2,20\n");
        assert!(diff_records_with(&old, &same, &options).unwrap().is_empty());
        assert!(diff_records_with(&old, &same, &typed).unwrap().is_empty());
    }

    #[test]
    fn integers_beyond_i64_are_kept_exactly() {
        let types = [("id", ColumnType::Int)];
        let big = "18446744073709551615";
        let old = vec![vec![big, "a", "", ""], vec!["-9223372036854775808", "b", "", ""]];
        let new = vec![vec![big, "A", "", ""], vec!["-9223372036854775808", "b", "", ""]];
        let options = with_csv(RecordOptions::new().key("id"), csv_options(&types));
        let ops = assert_csv_matches_oracle(&HEADER, &old, &new, &options, &types);
        assert_eq!(ops.len(), 1);
    }

    #[test]
    fn a_quoted_empty_cell_in_a_one_column_file_is_a_row_but_a_blank_line_is_not() {
        let dir = Dir::new();
        let with_empty = dir.text("a.csv", "id\nx\n\"\"\n\ny\n");
        let without = dir.text("b.csv", "id\nx\n\ny\n");
        let options = with_csv(RecordOptions::new().key("id"), CsvOptions::new());
        let ops = diff_records_with(&with_empty, &without, &options).unwrap();
        // Rows are x, "" and y; dropping the empty one is a single removal at /1.
        assert_eq!(
            ops.iter().map(|o| (o.op.as_str(), o.path.as_str())).collect::<Vec<_>>(),
            [("remove", "/1")]
        );
    }

    #[test]
    fn a_ragged_row_is_an_error_even_when_it_is_the_same_in_both_files() {
        // Equal rows are never read back, so only checking every row while indexing
        // catches this.
        let dir = Dir::new();
        let ragged = dir.text("ragged.csv", "id,v\n1,a\n2\n3,c\n");
        let options = with_csv(RecordOptions::new().key("id"), CsvOptions::new());
        let error = diff_records_with(&ragged, &ragged, &options).unwrap_err().to_string();
        assert!(
            error.contains("line 3") && error.contains("expected 2 fields, found 1"),
            "{error}"
        );
    }
}

// ----- streaming the operations out ------------------------------------------------

#[test]
fn each_operation_is_handed_over_in_the_order_diff_records_with_returns_them() {
    let old: Vec<Value> = (0..40).map(|i| row(i, i)).collect();
    let mut new: Vec<Value> = old.iter().filter(|r| r["id"] != 7).cloned().collect();
    new[3] = row(3, 99);
    new.insert(10, row(500, 1));
    new.swap(20, 21);
    let dir = Dir::new();
    let (old_file, new_file) = (dir.rows("o.ndjson", &old), dir.rows("n.ndjson", &new));
    let options = RecordOptions::new().key("id");
    let collected = diff_records_with(&old_file, &new_file, &options).unwrap();
    assert!(collected.len() > 3, "the case should have several operations");
    let mut streamed = Vec::new();
    diff_records_each(&old_file, &new_file, &options, |op| {
        streamed.push(op);
        Ok(())
    })
    .unwrap();
    assert_eq!(streamed, collected);
}

#[test]
fn an_error_from_the_callback_stops_the_diff_and_is_returned() {
    let old: Vec<Value> = (0..50).map(|i| row(i, i)).collect();
    let new: Vec<Value> = (0..50).map(|i| row(i, i + 1)).collect();
    let dir = Dir::new();
    let (old_file, new_file) = (dir.rows("o.ndjson", &old), dir.rows("n.ndjson", &new));
    let mut calls = 0;
    let result = diff_records_each(&old_file, &new_file, &RecordOptions::new().key("id"), |_| {
        calls += 1;
        if calls == 3 {
            Err(DriftError::Io("the disk is full".into()))
        } else {
            Ok(())
        }
    });
    assert!(matches!(&result, Err(DriftError::Io(m)) if m == "the disk is full"), "{result:?}");
    assert_eq!(calls, 3, "no operation is handed over after the callback failed");
}

/// Diffs with a budget for sorting instead of an index, through temporary files.
mod bounded {
    use super::*;
    use std::path::PathBuf;

    /// A directory for temporary files inside the scratch one, so leftovers show.
    fn scratch(dir: &Dir) -> PathBuf {
        let path = dir.0.join("tmp");
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn leftovers(path: &Path) -> Vec<String> {
        std::fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    fn bounded(options: &RecordOptions, limit: usize, dir: &Dir) -> RecordOptions {
        options.clone().memory_limit(limit).temp_dir(scratch(dir))
    }

    fn has_move(operations: &[Delta]) -> bool {
        operations.iter().any(|operation| operation.op == Operation::Move)
    }

    #[test]
    fn random_files_give_the_in_memory_diff_and_only_moves_need_the_memory() {
        // xorshift: deterministic, no extra dependencies.
        let mut state = 0x2545F4914F6CDD1Du64;
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
            RecordOptions::new().key("id").duplicates(Duplicates::Occurrence),
            RecordOptions::new().whole_row(),
            RecordOptions::new().whole_row().diff(DiffOptions::new().ignore_path("/*/v")),
        ];
        let (mut sorted_out, mut needed_moves) = (0, 0);
        for round in 0..400 {
            let options = &configs[round % configs.len()];
            let unique = options.duplicates.is_none() && !options.whole_row;
            let in_order = round % 2 == 0;
            let make = |next: &mut dyn FnMut(u64) -> u64| -> Vec<Value> {
                let mut rows: Vec<Value> = (0..next(14))
                    .map(|_| {
                        let id = next(6);
                        let region = ["eu", "us"][next(2) as usize];
                        json!({"id": id, "region": region, "v": next(3), "meta": {"n": id}})
                    })
                    .collect();
                if unique {
                    let mut seen = std::collections::HashSet::new();
                    rows.retain(|r| seen.insert((r["id"].clone(), r["region"].clone())));
                }
                if in_order {
                    rows.sort_by_key(|r| r["id"].as_u64());
                }
                rows
            };
            let (old, new) = (make(&mut next), make(&mut next));
            let dir = Dir::new();
            let (old_file, new_file) = (dir.rows("o.ndjson", &old), dir.rows("n.ndjson", &new));
            let expected = diff_records_with(&old_file, &new_file, options).unwrap();

            // Room for the rows in memory: always the in-memory answer.
            let roomy = diff_records_with(&old_file, &new_file, &bounded(options, 1600, &dir));
            assert_eq!(roomy.unwrap(), expected, "roomy, round {round}: {old:?} -> {new:?}");

            // No room: the same answer unless it needs moves, which is then refused.
            let strict = diff_records_with(&old_file, &new_file, &bounded(options, 400, &dir));
            let fits = (old.len() + new.len()) * 64 <= 400;
            match strict {
                Ok(operations) => {
                    assert_eq!(operations, expected, "round {round}: {old:?} -> {new:?}");
                    assert!(fits || !has_move(&expected), "round {round}: moves went unnoticed");
                    sorted_out += 1;
                }
                Err(error) => {
                    assert!(has_move(&expected) && !fits, "round {round}: {error}");
                    assert!(error.to_string().contains("different order"), "{error}");
                    needed_moves += 1;
                }
            }
            assert_eq!(leftovers(&dir.0.join("tmp")), Vec::<String>::new(), "round {round}");
        }
        assert!(sorted_out > 100 && needed_moves > 20, "{sorted_out} {needed_moves}");
    }

    /// A file of `count` rows with payloads that make it several runs long.
    fn big(count: u64, changed: impl Fn(u64) -> bool, gone: impl Fn(u64) -> bool) -> Vec<Value> {
        (0..count)
            .filter(|&i| !gone(i))
            .map(|i| {
                let v = if changed(i) { i + 1 } else { i };
                json!({"id": i * 3 + 1, "v": v, "pad": "x".repeat(40 + (i % 17) as usize)})
            })
            .collect()
    }

    #[test]
    fn a_file_many_times_the_budget_gives_the_in_memory_diff() {
        let old = big(20_000, |_| false, |_| false);
        let new = {
            let mut rows = big(20_000, |i| i % 7 == 0, |i| i % 11 == 0);
            rows.insert(5, json!({"id": 2, "v": 0, "pad": ""}));
            rows.push(json!({"id": 1_000_000, "v": 0, "pad": ""}));
            rows
        };
        let dir = Dir::new();
        let (old_file, new_file) = (dir.rows("o.ndjson", &old), dir.rows("n.ndjson", &new));
        let options = RecordOptions::new().key("id");
        let expected = diff_records_with(&old_file, &new_file, &options).unwrap();
        assert!(expected.len() > 4000);
        // 64 KiB: a few hundred rows per run, dozens of runs a file.
        let got = diff_records_with(&old_file, &new_file, &bounded(&options, 64 * 1024, &dir));
        assert_eq!(got.unwrap(), expected);
        assert!(leftovers(&dir.0.join("tmp")).is_empty());
    }

    #[test]
    fn repeats_that_are_left_out_do_not_count_in_the_row_numbers_of_either_file() {
        let row = |id: u64, v: u64| json!({"id": id, "v": v});
        let old = [row(1, 1), row(1, 2), row(2, 1), row(3, 1), row(3, 2), row(3, 3), row(4, 1)];
        let new = [row(0, 0), row(2, 9), row(2, 8), row(3, 1), row(5, 1), row(5, 2)];
        let dir = Dir::new();
        let (old_file, new_file) = (dir.rows("o.ndjson", &old), dir.rows("n.ndjson", &new));
        for policy in [Duplicates::First, Duplicates::Last] {
            let options = RecordOptions::new().key("id").duplicates(policy);
            let expected = diff_records_with(&old_file, &new_file, &options).unwrap();
            for limit in [100, 4000] {
                let got = diff_records_with(&old_file, &new_file, &bounded(&options, limit, &dir));
                assert_eq!(got.unwrap(), expected, "{policy:?} {limit}");
            }
        }
    }

    #[test]
    fn rows_are_read_back_across_the_edges_of_the_read_window() {
        // Rows of every length from a few bytes to far more than a window chunk.
        let rows = |changed: bool| -> Vec<Value> {
            (0..300u64)
                .map(|i| {
                    let pad = "ab\u{e9}\n\"".repeat((i * i % 5000) as usize);
                    json!({"id": i, "v": u64::from(changed && i % 3 == 0), "pad": pad})
                })
                .collect()
        };
        let (old, new) = (rows(false), rows(true));
        let dir = Dir::new();
        let (old_file, new_file) = (dir.rows("o.ndjson", &old), dir.rows("n.ndjson", &new));
        let options = RecordOptions::new().key("id");
        let expected = diff_records_with(&old_file, &new_file, &options).unwrap();
        assert_eq!(expected.len(), 100);
        let got = diff_records_with(&old_file, &new_file, &bounded(&options, 8000, &dir));
        assert_eq!(got.unwrap(), expected);
    }

    #[test]
    fn temporary_files_are_removed_when_the_diff_fails_or_is_stopped() {
        let old: Vec<Value> = (0..200).map(|i| row(i, i)).collect();
        let new: Vec<Value> = (0..200).map(|i| row(i, i + 1)).collect();
        let dir = Dir::new();
        let (old_file, new_file) = (dir.rows("o.ndjson", &old), dir.rows("n.ndjson", &new));
        let options = bounded(&RecordOptions::new().key("id"), 800, &dir);

        let mut calls = 0;
        let stopped = diff_records_each(&old_file, &new_file, &options, |_| {
            calls += 1;
            if calls == 5 {
                Err(DriftError::Io("stop".into()))
            } else {
                Ok(())
            }
        });
        assert!(matches!(stopped, Err(DriftError::Io(m)) if m == "stop"));
        assert_eq!(calls, 5);
        assert!(leftovers(&dir.0.join("tmp")).is_empty());

        let repeated = dir.rows("r.ndjson", &[row(1, 1), row(2, 2), row(1, 3)]);
        assert!(diff_records_with(&old_file, &repeated, &options).is_err());
        assert!(leftovers(&dir.0.join("tmp")).is_empty());

        // And that a diff which sorts on disk really does use the directory.
        let used = std::cell::Cell::new(false);
        diff_records_each(&old_file, &new_file, &options, |_| {
            used.set(used.get() || !leftovers(&dir.0.join("tmp")).is_empty());
            Ok(())
        })
        .unwrap();
        assert!(used.get(), "the sort should have gone to disk");
    }

    #[test]
    fn a_missing_temporary_directory_is_an_error() {
        let dir = Dir::new();
        let file = dir.rows("o.ndjson", &[row(1, 1)]);
        let options =
            RecordOptions::new().key("id").memory_limit(1000).temp_dir(dir.0.join("nowhere"));
        let error = diff_records_with(&file, &file, &options).unwrap_err();
        assert!(matches!(&error, DriftError::Io(m) if m.contains("nowhere")), "{error}");
    }

    #[test]
    fn problems_with_the_data_read_the_same_as_without_a_limit() {
        let dir = Dir::new();
        let good = dir.text("good.ndjson", "{\"id\":1}\n{\"id\":2}\n");
        let bad = [
            ("dup.ndjson", "{\"id\":1}\n{\"id\":2}\n{\"id\":1}\n"),
            ("nokey.ndjson", "{\"id\":1}\n{\"x\":2}\n"),
            ("badkey.ndjson", "{\"id\":1}\n{\"id\":[2]}\n"),
            ("json.ndjson", "{\"id\":1}\n{\"id\":\n"),
            ("scalar.ndjson", "{\"id\":1}\n5\n"),
        ];
        for (name, text) in bad {
            let file = dir.text(name, text);
            let options = RecordOptions::new().key("id");
            for (old, new) in [(&file, &good), (&good, &file)] {
                let plain = diff_records_with(old, new, &options).unwrap_err().to_string();
                let limited =
                    diff_records_with(old, new, &bounded(&options, 500, &dir)).unwrap_err();
                assert_eq!(limited.to_string(), plain, "{name}");
            }
        }
        let missing = dir.0.join("missing.ndjson");
        let options = bounded(&RecordOptions::new().key("id"), 500, &dir);
        assert!(matches!(diff_records_with(&missing, &good, &options), Err(DriftError::Io(_))));
    }

    #[cfg(feature = "csv")]
    mod csv_files {
        use super::*;

        fn cells(i: u64, changed: bool) -> Vec<String> {
            let note =
                ["plain", "has,comma", "has \"quote\"", "two\nlines", "ünï"][(i % 5) as usize];
            vec![
                (i * 2).to_string(),
                if changed { "changed".into() } else { "same".into() },
                format!("{note} {}", "x".repeat((i % 40) as usize)),
            ]
        }

        fn write(dir: &Dir, name: &str, rows: impl Iterator<Item = Vec<String>>) -> PathBuf {
            let mut writer = ::csv::Writer::from_writer(Vec::new());
            writer.write_record(["id", "state", "note"]).unwrap();
            for row in rows {
                writer.write_record(&row).unwrap();
            }
            dir.text(name, &String::from_utf8(writer.into_inner().unwrap()).unwrap())
        }

        #[test]
        fn csv_files_give_the_in_memory_diff_through_the_sort() {
            let dir = Dir::new();
            let old = write(&dir, "o.csv", (0..6000).map(|i| cells(i, false)));
            let new = write(
                &dir,
                "n.csv",
                (0..6000).filter(|i| i % 13 != 0).map(|i| cells(i, i % 5 == 0)),
            );
            let base = RecordOptions::new()
                .key("id")
                .format(RecordFormat::Csv(CsvOptions::new().column("id", ColumnType::Int)));
            let expected = diff_records_with(&old, &new, &base).unwrap();
            assert!(expected.len() > 1000);
            let got = diff_records_with(&old, &new, &bounded(&base, 40_000, &dir));
            assert_eq!(got.unwrap(), expected);
            assert!(leftovers(&dir.0.join("tmp")).is_empty());
        }

        #[test]
        fn csv_whole_rows_and_repeats_work_through_the_sort() {
            let dir = Dir::new();
            let rows = |extra: bool| {
                let mut all: Vec<Vec<String>> = (0..50).map(|i| cells(i % 20, false)).collect();
                if extra {
                    all.push(cells(3, true));
                }
                all.into_iter()
            };
            let (old, new) = (write(&dir, "o.csv", rows(false)), write(&dir, "n.csv", rows(true)));
            for base in [RecordOptions::new().whole_row(), RecordOptions::new().key("id")] {
                let base = base
                    .duplicates(Duplicates::Occurrence)
                    .format(RecordFormat::Csv(CsvOptions::new()));
                let expected = diff_records_with(&old, &new, &base).unwrap();
                let got = diff_records_with(&old, &new, &bounded(&base, 600, &dir));
                assert_eq!(got.unwrap(), expected);
            }
        }

        #[test]
        fn csv_problems_read_the_same_as_without_a_limit() {
            let dir = Dir::new();
            let good = dir.text("good.csv", "id,v\n1,a\n2,b\n");
            let bad = [
                ("ragged.csv", "id,v\n1,a\n2\n"),
                ("dup.csv", "id,v\n1,a\n2,b\n1,c\n"),
                ("header.csv", "id,id\n1,a\n"),
            ];
            for (name, text) in bad {
                let file = dir.text(name, text);
                let base =
                    RecordOptions::new().key("id").format(RecordFormat::Csv(CsvOptions::new()));
                let plain = diff_records_with(&file, &good, &base).unwrap_err().to_string();
                let limited = diff_records_with(&file, &good, &bounded(&base, 500, &dir));
                assert_eq!(limited.unwrap_err().to_string(), plain, "{name}");
            }
        }
    }
}

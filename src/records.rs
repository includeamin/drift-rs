//! Diffing two files of records, one JSON object per line (NDJSON), without
//! loading them.
//!
//! The in-memory diff builds a value tree for the whole document, which takes
//! 26-35 times the size of the file. Here each file is read once into a compact
//! index: per row, its key, a 128-bit fingerprint of its contents and where it
//! sits in the file, whatever the row holds (about 100 MB for two files of
//! 400,000 rows, against about 1.3 GB in memory). Rows are matched
//! by key, rows with equal fingerprints are skipped without being parsed again,
//! and only the rows that changed, or have to be added, are read back from disk.
//!
//! The result is exactly what [`diff_with`](crate::diff_with) gives for the same
//! rows as arrays matched by that key: the order of operations comes from the same
//! planner, so the two can be used interchangeably.

use crate::diff::{identity, plan_keyed, walk, Context, KeyedStep};
use crate::{join_pointer, Delta, DiffOptions, DriftError, Operation};
use serde_json::Value;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

type Result<T> = std::result::Result<T, DriftError>;

/// Differences between two files of records, one JSON object per line, matched
/// by the value of the field `key`.
///
/// Blank lines are ignored and rows are numbered without them, so the first row
/// is `/0`. The operations are those of `diff_with` over the two files read as
/// arrays and matched by `key` (see [`DiffOptions::array_keys`]), with
/// `options.ignore_paths` applied inside each row, as `/*/field`:
///
/// ```
/// use drift::{diff_records, DiffOptions};
///
/// let dir = std::env::temp_dir().join(format!("drift-doc-{}", std::process::id()));
/// std::fs::create_dir_all(&dir)?;
/// let (old, new) = (dir.join("old.ndjson"), dir.join("new.ndjson"));
/// std::fs::write(&old, "{\"id\":1,\"n\":\"a\"}\n{\"id\":2,\"n\":\"b\"}\n")?;
/// std::fs::write(&new, "{\"id\":0,\"n\":\"z\"}\n{\"id\":1,\"n\":\"a\"}\n{\"id\":2,\"n\":\"B\"}\n")?;
///
/// let operations = diff_records(&old, &new, "id", &DiffOptions::new())?;
/// assert_eq!(operations.len(), 2); // add the new first row, change the last
/// # std::fs::remove_dir_all(&dir).ok();
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// Memory is the index of both files, about 250 bytes a row across the two, plus
/// the operations themselves. Every row must be a JSON object whose `key` is a string, number
/// or boolean, and keys must be unique in each file; anything else is an error
/// naming the file and line.
pub fn diff_records(
    old: &Path,
    new: &Path,
    key: &str,
    options: &DiffOptions,
) -> Result<Vec<Delta>> {
    let old_rows = Rows::index(old, key)?;
    let new_rows = Rows::index(new, key)?;
    let old_by_key = old_rows.by_key()?;
    new_rows.by_key()?;
    let matches: Vec<Option<usize>> =
        new_rows.keys.iter().map(|k| old_by_key.get(k.as_str()).copied()).collect();
    drop(old_by_key);

    let mut files = Files { old: open(old)?, new: open(new)? };
    let ctx = Context::new(options);
    let mut operations = Vec::new();
    let mut failure = None;
    plan_keyed(old_rows.len(), &matches, |step| {
        if failure.is_none() {
            if let Err(error) = apply(step, &old_rows, &new_rows, &mut files, &ctx, &mut operations)
            {
                failure = Some(error);
            }
        }
    });
    failure.map_or(Ok(operations), Err)
}

struct Files {
    old: File,
    new: File,
}

/// Turns one planned step into operations, reading rows back only when needed.
fn apply(
    step: KeyedStep,
    old: &Rows,
    new: &Rows,
    files: &mut Files,
    ctx: &Context,
    out: &mut Vec<Delta>,
) -> Result<()> {
    let at = |index: usize| join_pointer("", &index.to_string());
    match step {
        KeyedStep::Remove(index) => out.push(Delta::new(Operation::Remove, at(index))),
        KeyedStep::Move { from, to } => out.push(Delta {
            op: Operation::Move,
            path: at(to),
            value: None,
            from_path: Some(at(from)),
        }),
        KeyedStep::Add(index) => {
            let row = new.read(&mut files.new, index)?;
            out.push(Delta::with_value(Operation::Add, at(index), row));
        }
        KeyedStep::Pair { old: o, new: n } => {
            // Equal fingerprints: the rows are the same, and nothing is read.
            if old.hashes[o] != new.hashes[n] {
                let (old_row, new_row) =
                    (old.read(&mut files.old, o)?, new.read(&mut files.new, n)?);
                walk(&new_row, &old_row, &at(n), ctx, out);
            }
        }
    }
    Ok(())
}

/// A file of records, indexed: one entry per row, in file order.
struct Rows {
    path: PathBuf,
    /// The value of each row's key field, as `to_string` of the JSON value, so
    /// the number 1 and the string "1" are different keys.
    keys: Vec<String>,
    hashes: Vec<u128>,
    /// 1-based line number of each row, for error messages.
    lines: Vec<u32>,
    /// Where each row's line starts in the file, and how long it is.
    spans: Vec<(u64, u32)>,
}

fn open(path: &Path) -> Result<File> {
    File::open(path).map_err(|e| DriftError::Io(format!("{}: {e}", path.display())))
}

fn trim(line: &[u8]) -> &[u8] {
    let start = line.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(line.len());
    let end = line.iter().rposition(|b| !b.is_ascii_whitespace()).map_or(start, |i| i + 1);
    &line[start..end]
}

impl Rows {
    fn len(&self) -> usize {
        self.keys.len()
    }

    fn error(&self, line: u32, message: impl std::fmt::Display) -> DriftError {
        DriftError::Parse(format!("{}: line {line}: {message}", self.path.display()))
    }

    fn index(path: &Path, key: &str) -> Result<Rows> {
        let mut rows = Rows {
            path: path.to_path_buf(),
            keys: Vec::new(),
            hashes: Vec::new(),
            lines: Vec::new(),
            spans: Vec::new(),
        };
        let mut reader = BufReader::with_capacity(1 << 20, open(path)?);
        let (mut buffer, mut offset, mut line) = (Vec::new(), 0u64, 0u32);
        loop {
            buffer.clear();
            let read = reader
                .read_until(b'\n', &mut buffer)
                .map_err(|e| DriftError::Io(format!("{}: {e}", path.display())))?;
            if read == 0 {
                return Ok(rows);
            }
            line += 1;
            let start = offset;
            offset += read as u64;
            let text = trim(&buffer);
            if text.is_empty() {
                continue;
            }
            let row: Value = serde_json::from_slice(text).map_err(|e| rows.error(line, e))?;
            let Value::Object(members) = &row else {
                return Err(rows.error(line, "a row must be a JSON object"));
            };
            let identity = identity(&row, key).ok_or_else(|| {
                if members.contains_key(key) {
                    rows.error(line, format!("`{key}` must be a string, number or boolean"))
                } else {
                    rows.error(line, format!("the row has no `{key}` field"))
                }
            })?;
            rows.keys.push(identity);
            rows.hashes.push(fingerprint(&row));
            rows.lines.push(line);
            rows.spans.push((start, read as u32));
        }
    }

    /// Row index by key, or an error naming the two lines of a duplicate.
    fn by_key(&self) -> Result<HashMap<&str, usize>> {
        let mut by_key = HashMap::with_capacity(self.len());
        for (index, key) in self.keys.iter().enumerate() {
            if let Some(first) = by_key.insert(key.as_str(), index) {
                return Err(self.error(
                    self.lines[index],
                    format!("duplicate key {key} (first seen at line {})", self.lines[first]),
                ));
            }
        }
        Ok(by_key)
    }

    /// Reads row `index` back from the file.
    fn read(&self, file: &mut File, index: usize) -> Result<Value> {
        let (start, length) = self.spans[index];
        let mut buffer = vec![0u8; length as usize];
        file.seek(SeekFrom::Start(start))
            .and_then(|_| file.read_exact(&mut buffer))
            .map_err(|e| DriftError::Io(format!("{}: {e}", self.path.display())))?;
        serde_json::from_slice(trim(&buffer)).map_err(|e| self.error(self.lines[index], e))
    }
}

/// A 128-bit fingerprint of a value such that equal values (as `==` on
/// `serde_json::Value`: object members in any order, `1` different from `1.0`)
/// have equal fingerprints. Rows with equal fingerprints are treated as equal,
/// which is wrong only for a 128-bit collision.
fn fingerprint(value: &Value) -> u128 {
    (u128::from(lane(value, 0x9E37_79B9_7F4A_7C15)) << 64)
        | u128::from(lane(value, 0xC2B2_AE3D_27D4_EB4F))
}

/// One 64-bit half of the fingerprint; the two halves use different salts.
fn lane(value: &Value, salt: u64) -> u64 {
    let mut hasher = DefaultHasher::new();
    salt.hash(&mut hasher);
    match value {
        Value::Null => 0u8.hash(&mut hasher),
        Value::Bool(b) => (1u8, *b).hash(&mut hasher),
        Value::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
            (Some(i), _, _) => (2u8, i).hash(&mut hasher),
            (None, Some(u), _) => (3u8, u).hash(&mut hasher),
            (None, None, Some(f)) => (4u8, f.to_bits()).hash(&mut hasher),
            _ => 8u8.hash(&mut hasher),
        },
        Value::String(text) => (5u8, text).hash(&mut hasher),
        Value::Array(items) => {
            (6u8, items.len()).hash(&mut hasher);
            for item in items {
                lane(item, salt).hash(&mut hasher);
            }
        }
        Value::Object(members) => {
            (7u8, members.len()).hash(&mut hasher);
            // Added together so the order of the members does not matter.
            let combined = members.iter().fold(0u64, |sum, (name, member)| {
                let mut entry = DefaultHasher::new();
                (salt, name, lane(member, salt)).hash(&mut entry);
                sum.wrapping_add(entry.finish())
            });
            combined.hash(&mut hasher);
        }
    }
    hasher.finish()
}

#[cfg(test)]
mod tests {
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
        let new = dir.text(
            "new.ndjson",
            "  {\"b\": [1, 2], \"a\": 1, \"id\": 1}  \n{ \"a\":2 , \"id\":2 }\n",
        );
        assert!(diff_records(&old, &new, "id", &DiffOptions::new()).unwrap().is_empty());
        // 1 and 1.0 are different values, as everywhere else in drift.
        let float =
            dir.text("float.ndjson", "{\"id\":1,\"a\":1.0,\"b\":[1,2]}\n{\"id\":2,\"a\":2}\n");
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
            let streamed = diff_records(
                &dir.rows("o.ndjson", &old),
                &dir.rows("n.ndjson", &new),
                "id",
                &options,
            )
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

    #[test]
    fn the_fingerprint_ignores_member_order_but_nothing_else() {
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
        // Different values must not collide in these ordinary cases.
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
}

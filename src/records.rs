//! Diffing two files of records, one JSON object per line (NDJSON), without
//! loading them.
//!
//! The in-memory diff builds a value tree for the whole document, which takes
//! 26-35 times the size of the file. Here each file is read once into a compact
//! index of 40 bytes a row, whatever the row holds: a hash of the row's key, a
//! hash of its bytes and where it starts in the file. The reading is done in
//! parallel, parsing only the key out of each line. Rows are matched by key, rows
//! whose bytes are equal are skipped without being parsed, and only the rows that
//! differ, or have to be added, are read back from disk.
//!
//! The result is exactly what [`diff_with`](crate::diff_with) gives for the same
//! rows as arrays matched by that key: the order of operations comes from the same
//! planner, so the two can be used interchangeably.

use crate::diff::{plan_keyed, walk, Context, KeyedStep};
use crate::{join_pointer, split_pointer, Delta, DiffOptions, DriftError, Operation};
use serde::de::{DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::Value;
use std::cell::{Cell, RefCell};
use std::collections::hash_map::DefaultHasher;
use std::fmt;
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
/// Memory is the index of both files, 40 bytes a row each, a few more bytes a
/// row for matching and planning, and the operations themselves. Every row must be
/// a JSON object whose `key` is a string, number or boolean, and keys must be
/// unique in each file; anything else is an error naming the file and line.
pub fn diff_records(
    old: &Path,
    new: &Path,
    key: &str,
    options: &DiffOptions,
) -> Result<Vec<Delta>> {
    let spec = KeySpec::parse(&[key.to_string()])?;
    let old_rows = Rows::index(old, &spec)?;
    let new_rows = Rows::index(new, &spec)?;
    let old_order = old_rows.sorted_unique(&spec)?;
    new_rows.sorted_unique(&spec)?;
    let matches: Vec<Option<usize>> = new_rows
        .keys
        .iter()
        .map(|key| {
            old_order
                .binary_search_by(|&row| old_rows.keys[row as usize].cmp(key))
                .ok()
                .map(|at| old_order[at] as usize)
        })
        .collect();
    drop(old_order);

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
            // Equal bytes: the rows are the same, and nothing is read. Rows that
            // differ in bytes only (field order, spacing) are read and found equal
            // by `walk`, which reports nothing for them.
            if old.hashes[o] != new.hashes[n] {
                let (old_row, new_row) =
                    (old.read(&mut files.old, o)?, new.read(&mut files.new, n)?);
                walk(&new_row, &old_row, &at(n), ctx, out);
            }
        }
    }
    Ok(())
}

/// Where the key of a row is: one field, or a JSON Pointer into the row.
#[derive(Debug, Clone)]
struct KeySpec {
    /// What the user wrote, for messages.
    label: String,
    tokens: Vec<String>,
}

impl KeySpec {
    fn parse(specs: &[String]) -> Result<KeySpec> {
        let [spec] = specs else {
            return Err(DriftError::Parse("exactly one key is supported".into()));
        };
        let tokens = if spec.starts_with('/') { split_pointer(spec)? } else { vec![spec.clone()] };
        if tokens.is_empty() {
            return Err(DriftError::Parse("a key must name a field".into()));
        }
        Ok(KeySpec { label: spec.clone(), tokens })
    }
}

/// What can be wrong with a row, found while reading its key.
enum Fault {
    NotObject,
    NoKey,
    BadKey,
    Json(serde_json::Error),
}

/// Reads the key out of one line without building the rest of the row: the other
/// fields are skipped as they are read, which allocates nothing.
fn read_key(line: &[u8], spec: &KeySpec) -> std::result::Result<Value, Fault> {
    let found = RefCell::new(None);
    let object = Cell::new(false);
    let mut deserializer = serde_json::Deserializer::from_slice(line);
    let seed = KeySeed { tokens: &spec.tokens, found: &found, object: &object };
    seed.deserialize(&mut deserializer).map_err(Fault::Json)?;
    deserializer.end().map_err(Fault::Json)?;
    if !object.get() {
        return Err(Fault::NotObject);
    }
    match found.into_inner() {
        None => Err(Fault::NoKey),
        Some(key @ (Value::String(_) | Value::Number(_) | Value::Bool(_))) => Ok(key),
        Some(_) => Err(Fault::BadKey),
    }
}

/// Walks one value, keeping the part at `tokens` and skipping everything else.
struct KeySeed<'a> {
    tokens: &'a [String],
    found: &'a RefCell<Option<Value>>,
    /// Set when the value being walked at the top is an object.
    object: &'a Cell<bool>,
}

impl<'de> DeserializeSeed<'de> for KeySeed<'_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for KeySeed<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<(), A::Error> {
        self.object.set(true);
        let (head, rest) = self.tokens.split_first().expect("a key has a token");
        while let Some(name) = map.next_key::<std::borrow::Cow<str>>()? {
            if name == head.as_str() {
                if rest.is_empty() {
                    *self.found.borrow_mut() = Some(map.next_value::<Value>()?);
                } else {
                    // A nested object: its own `object` flag is irrelevant.
                    let inner = Cell::new(false);
                    map.next_value_seed(KeySeed {
                        tokens: rest,
                        found: self.found,
                        object: &inner,
                    })?;
                }
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(())
    }

    // Anything that is not an object holds no key; arrays still have to be read
    // to their end.
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<(), A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(())
    }
    fn visit_bool<E>(self, _: bool) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_i64<E>(self, _: i64) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_u64<E>(self, _: u64) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_f64<E>(self, _: f64) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_str<E>(self, _: &str) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_unit<E>(self) -> std::result::Result<(), E> {
        Ok(())
    }
}

/// A file of records, indexed: one entry per row, in file order.
struct Rows {
    path: PathBuf,
    /// A 128-bit hash of each row's key. The number 1 and the string "1" are
    /// different keys.
    keys: Vec<u128>,
    /// A 128-bit hash of each row's bytes, so equal lines are known to be equal
    /// rows without being parsed.
    hashes: Vec<u128>,
    /// Where each row's line starts. A row runs to the next row's start (or
    /// `end`), which also takes in any blank lines after it: they are trimmed.
    offsets: Vec<u64>,
    end: u64,
}

fn open(path: &Path) -> Result<File> {
    File::open(path).map_err(|e| DriftError::Io(format!("{}: {e}", path.display())))
}

fn trim(line: &[u8]) -> &[u8] {
    let start = line.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(line.len());
    let end = line.iter().rposition(|b| !b.is_ascii_whitespace()).map_or(start, |i| i + 1);
    &line[start..end]
}

/// The index of one stretch of a file.
struct Part {
    keys: Vec<u128>,
    hashes: Vec<u128>,
    offsets: Vec<u64>,
}

/// A row that could not be indexed: where its line starts, and what is wrong.
struct Failure {
    offset: u64,
    fault: Fault,
}

/// Files smaller than this are indexed on one thread.
const PARALLEL_FROM: u64 = 8 * 1024 * 1024;
const MAX_THREADS: usize = 8;

impl Rows {
    fn len(&self) -> usize {
        self.keys.len()
    }

    /// 1-based line number of `offset`, found by counting the lines before it.
    /// Only errors need it, so it is not kept for every row.
    fn line_of(&self, offset: u64) -> u32 {
        let Ok(file) = File::open(&self.path) else { return 0 };
        let mut reader = BufReader::with_capacity(1 << 20, file.take(offset));
        let mut newlines = 0u32;
        while let Ok(buffer) = reader.fill_buf() {
            if buffer.is_empty() {
                break;
            }
            newlines += buffer.iter().filter(|&&b| b == b'\n').count() as u32;
            let consumed = buffer.len();
            reader.consume(consumed);
        }
        newlines + 1
    }

    fn error_at(&self, offset: u64, message: impl fmt::Display) -> DriftError {
        DriftError::Parse(format!(
            "{}: line {}: {message}",
            self.path.display(),
            self.line_of(offset)
        ))
    }

    fn index(path: &Path, spec: &KeySpec) -> Result<Rows> {
        let end = open(path)?
            .metadata()
            .map_err(|e| DriftError::Io(format!("{}: {e}", path.display())))?
            .len();
        let threads = if end < PARALLEL_FROM {
            1
        } else {
            std::thread::available_parallelism().map_or(1, |n| n.get()).min(MAX_THREADS)
        };
        Rows::index_with(path, spec, threads)
    }

    fn index_with(path: &Path, spec: &KeySpec, threads: usize) -> Result<Rows> {
        let end = open(path)?
            .metadata()
            .map_err(|e| DriftError::Io(format!("{}: {e}", path.display())))?
            .len();
        let bounds = chunk_bounds(path, end, threads)?;
        let parts: Vec<std::result::Result<Part, Failure>> = std::thread::scope(|scope| {
            let handles: Vec<_> = bounds
                .iter()
                .map(|&(start, stop)| scope.spawn(move || index_part(path, start, stop, spec)))
                .collect();
            handles.into_iter().map(|h| h.join().expect("indexing does not panic")).collect()
        });

        let mut rows = Rows {
            path: path.to_path_buf(),
            keys: Vec::new(),
            hashes: Vec::new(),
            offsets: Vec::new(),
            end,
        };
        for part in parts {
            match part {
                Ok(part) => {
                    rows.keys.extend(part.keys);
                    rows.hashes.extend(part.hashes);
                    rows.offsets.extend(part.offsets);
                }
                Err(Failure { offset, fault }) => {
                    let message = match fault {
                        Fault::NotObject => "a row must be a JSON object".to_string(),
                        Fault::NoKey => format!("the row has no `{}` field", spec.label),
                        Fault::BadKey => {
                            format!("`{}` must be a string, number or boolean", spec.label)
                        }
                        Fault::Json(error) => error.to_string(),
                    };
                    return Err(rows.error_at(offset, message));
                }
            }
        }
        Ok(rows)
    }

    /// Row numbers sorted by key, or an error naming the lines of a duplicate.
    fn sorted_unique(&self, spec: &KeySpec) -> Result<Vec<u32>> {
        let mut order: Vec<u32> = (0..self.len() as u32).collect();
        order.sort_unstable_by_key(|&row| (self.keys[row as usize], row));
        for pair in order.windows(2) {
            let (first, second) = (pair[0] as usize, pair[1] as usize);
            if self.keys[first] == self.keys[second] {
                let key = self.key_text(first, spec);
                return Err(self.error_at(
                    self.offsets[second],
                    format!(
                        "duplicate key {key} (first seen at line {})",
                        self.line_of(self.offsets[first])
                    ),
                ));
            }
        }
        Ok(order)
    }

    /// The key of row `index`, as JSON, read back for a message.
    fn key_text(&self, index: usize, spec: &KeySpec) -> String {
        File::open(&self.path)
            .ok()
            .and_then(|mut file| self.read(&mut file, index).ok())
            .and_then(|row| {
                row.pointer(&format!("/{}", spec.tokens.join("/"))).map(Value::to_string)
            })
            .unwrap_or_else(|| "?".to_string())
    }

    /// Reads row `index` back from the file.
    fn read(&self, file: &mut File, index: usize) -> Result<Value> {
        let start = self.offsets[index];
        let stop = self.offsets.get(index + 1).copied().unwrap_or(self.end);
        let mut buffer = vec![0u8; (stop - start) as usize];
        file.seek(SeekFrom::Start(start))
            .and_then(|_| file.read_exact(&mut buffer))
            .map_err(|e| DriftError::Io(format!("{}: {e}", self.path.display())))?;
        serde_json::from_slice(trim(&buffer)).map_err(|e| self.error_at(start, e))
    }
}

/// Cuts a file of `len` bytes into up to `parts` stretches that each start at the
/// beginning of a line.
fn chunk_bounds(path: &Path, len: u64, parts: usize) -> Result<Vec<(u64, u64)>> {
    let mut starts = vec![0u64];
    if parts > 1 && len > 0 {
        let mut reader = BufReader::new(open(path)?);
        for k in 1..parts as u64 {
            let target = (len * k / parts as u64).max(*starts.last().expect("starts is not empty"));
            let mut skipped = Vec::new();
            let moved = reader
                .seek(SeekFrom::Start(target))
                .and_then(|_| reader.read_until(b'\n', &mut skipped))
                .map_err(|e| DriftError::Io(format!("{}: {e}", path.display())))?;
            let start = target + moved as u64;
            if start < len && start > *starts.last().expect("starts is not empty") {
                starts.push(start);
            }
        }
    }
    let mut bounds: Vec<(u64, u64)> = starts.windows(2).map(|w| (w[0], w[1])).collect();
    bounds.push((*starts.last().expect("starts is not empty"), len));
    Ok(bounds)
}

/// Indexes the lines that start in `start..stop`.
fn index_part(
    path: &Path,
    start: u64,
    stop: u64,
    spec: &KeySpec,
) -> std::result::Result<Part, Failure> {
    let io = |e: std::io::Error| Failure {
        offset: start,
        fault: Fault::Json(serde::de::Error::custom(format!("{}: {e}", path.display()))),
    };
    let mut file = File::open(path).map_err(io)?;
    file.seek(SeekFrom::Start(start)).map_err(io)?;
    let mut reader = BufReader::with_capacity(1 << 20, file.take(stop - start));
    let mut part = Part { keys: Vec::new(), hashes: Vec::new(), offsets: Vec::new() };
    let (mut buffer, mut offset) = (Vec::new(), start);
    loop {
        buffer.clear();
        let read = reader.read_until(b'\n', &mut buffer).map_err(io)?;
        if read == 0 {
            return Ok(part);
        }
        let line_start = offset;
        offset += read as u64;
        let text = trim(&buffer);
        if text.is_empty() {
            continue;
        }
        let key = read_key(text, spec).map_err(|fault| Failure { offset: line_start, fault })?;
        part.keys.push(hash_key(&key));
        part.hashes.push(hash_bytes(text));
        part.offsets.push(line_start);
    }
}

/// Two 64-bit hashes of the same data with different salts, as one 128-bit hash.
fn hash128(feed: impl Fn(&mut DefaultHasher)) -> u128 {
    let lane = |salt: u64| {
        let mut hasher = DefaultHasher::new();
        salt.hash(&mut hasher);
        feed(&mut hasher);
        hasher.finish()
    };
    (u128::from(lane(0x9E37_79B9_7F4A_7C15)) << 64) | u128::from(lane(0xC2B2_AE3D_27D4_EB4F))
}

fn hash_bytes(bytes: &[u8]) -> u128 {
    hash128(|hasher| bytes.hash(hasher))
}

/// A hash of a scalar key that keeps the number 1, the string "1" and the
/// number 1.0 apart, as the in-memory diff does.
fn hash_key(key: &Value) -> u128 {
    hash128(|hasher| match key {
        Value::String(text) => (0u8, text).hash(hasher),
        Value::Bool(b) => (1u8, *b).hash(hasher),
        Value::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
            (Some(i), _, _) => (2u8, i).hash(hasher),
            (None, Some(u), _) => (3u8, u).hash(hasher),
            (None, None, Some(f)) => (4u8, f.to_bits()).hash(hasher),
            _ => 5u8.hash(hasher),
        },
        _ => 6u8.hash(hasher),
    })
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

    fn key(field: &str) -> KeySpec {
        KeySpec::parse(&[field.to_string()]).unwrap()
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
            let pointer =
                if field.starts_with('/') { field.to_string() } else { format!("/{field}") };
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
                    Ok(v) => format!("key {v}"),
                    Err(Fault::NoKey) => "no key".to_string(),
                    Err(Fault::BadKey) => "bad key".to_string(),
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
}

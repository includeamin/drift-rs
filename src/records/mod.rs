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

mod ndjson;

use crate::diff::{plan_keyed, walk, Context, KeyedStep};
use crate::{join_pointer, split_pointer, Delta, DiffOptions, DriftError, Operation};
#[cfg(test)]
use ndjson::read_key;
use ndjson::{chunk_bounds, index_part};
use serde_json::Value;
use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

type Result<T> = std::result::Result<T, DriftError>;

/// What to do when several rows of a file have the same key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Duplicates {
    /// Refuse: the two lines are named in the error. The default for keyed rows.
    #[default]
    Error,
    /// Keep the first row with a key and ignore the others, as if they were not in
    /// the file: rows are numbered without them.
    First,
    /// Keep the last row with a key and ignore the others.
    Last,
    /// Match the nth row with a key in one file to the nth row with that key in
    /// the other. The default for whole-row keys, where identical rows are normal.
    Occurrence,
}

/// How [`diff_records_with`] matches and compares rows.
///
/// Build one with [`RecordOptions::new`] and the methods below; the struct is
/// `#[non_exhaustive]` so options can be added without breaking callers.
///
/// ```
/// use drift::{DiffOptions, Duplicates, RecordOptions};
///
/// let options = RecordOptions::new()
///     .key("id")
///     .key("/address/region")
///     .duplicates(Duplicates::Last)
///     .diff(DiffOptions::new().ignore_path("/*/updatedAt"));
/// assert_eq!(options.key, ["id", "/address/region"]);
/// ```
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct RecordOptions {
    /// The fields that together identify a row, each a field name or a JSON
    /// Pointer such as `/address/region`. The value of every one must be a string,
    /// number or boolean, and none may lie inside another.
    pub key: Vec<String>,
    /// Use the whole row as its own key instead: rows match when they are equal
    /// (member order and spacing do not matter), so a changed row is a removal and
    /// an addition. Rows may then be any JSON value. Incompatible with `key`.
    pub whole_row: bool,
    /// What to do about repeated keys. `None` means [`Duplicates::Error`] for keyed
    /// rows and [`Duplicates::Occurrence`] for whole rows.
    pub duplicates: Option<Duplicates>,
    /// How rows are compared once matched, and what is left out.
    pub diff: DiffOptions,
}

impl RecordOptions {
    /// No key yet: add one with [`key`](Self::key) or [`whole_row`](Self::whole_row).
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a field (or JSON Pointer) to the key; several make a composite key.
    pub fn key(mut self, field: impl Into<String>) -> Self {
        self.key.push(field.into());
        self
    }

    /// Matches rows by their whole content; see [`whole_row`](Self::whole_row).
    pub fn whole_row(mut self) -> Self {
        self.whole_row = true;
        self
    }

    /// Sets the policy for repeated keys.
    pub fn duplicates(mut self, policy: Duplicates) -> Self {
        self.duplicates = Some(policy);
        self
    }

    /// Sets how matched rows are compared (ignored paths, keys of inner arrays).
    pub fn diff(mut self, options: DiffOptions) -> Self {
        self.diff = options;
        self
    }
}

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
/// This is [`diff_records_with`] with one key and the default policy for repeated
/// keys, which is to refuse them.
pub fn diff_records(
    old: &Path,
    new: &Path,
    key: &str,
    options: &DiffOptions,
) -> Result<Vec<Delta>> {
    diff_records_with(old, new, &RecordOptions::new().key(key).diff(options.clone()))
}

/// Differences between two files of records, with a composite or nested key, a
/// policy for repeated keys, or the whole row as the key. See [`RecordOptions`].
///
/// The operations are those of `diff_with` over the two files read as arrays and
/// matched by that key, so applying them to the old rows (after the policy for
/// repeated keys has dropped some) gives the new rows. With whole-row keys a
/// changed row is never edited in place: it is a removal and an addition.
///
/// Memory is the index of both files, 40 bytes a row each, a few more bytes a
/// row for matching and planning, and the operations themselves. Problems with
/// the data (a missing or non-scalar key, a repeated key under the default
/// policy, a line that is not JSON) are errors naming the file and line.
pub fn diff_records_with(old: &Path, new: &Path, options: &RecordOptions) -> Result<Vec<Delta>> {
    let spec = KeySpec::parse(options)?;
    let policy = options.duplicates.unwrap_or(if options.whole_row {
        Duplicates::Occurrence
    } else {
        Duplicates::Error
    });
    let old_rows = Rows::index(old, &spec)?.resolve(policy, &spec)?;
    let new_rows = Rows::index(new, &spec)?.resolve(policy, &spec)?;
    let old_order = old_rows.sorted();
    let matches: Vec<Option<usize>> = (0..new_rows.len())
        .map(|row| {
            let key = new_rows.key(row);
            old_order
                .binary_search_by(|&at| old_rows.key(at as usize).cmp(&key))
                .ok()
                .map(|at| old_order[at] as usize)
        })
        .collect();
    drop(old_order);

    let mut files = Files { old: open(old)?, new: open(new)? };
    let ctx = Context::new(&options.diff);
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
            // With whole rows as keys, matched rows are equal rows by definition.
            // Otherwise equal bytes mean equal rows and nothing is read; rows that
            // differ in bytes only (field order, spacing) are read and found equal
            // by `walk`, which reports nothing for them.
            if !old.whole_row && old.hash(o) != new.hash(n) {
                let (old_row, new_row) =
                    (old.read(&mut files.old, o)?, new.read(&mut files.new, n)?);
                walk(&new_row, &old_row, &at(n), ctx, out);
            }
        }
    }
    Ok(())
}

/// Where the key of a row is: one or more fields or JSON Pointers, or the whole row.
#[derive(Debug, Clone)]
struct KeySpec {
    whole_row: bool,
    parts: Vec<KeyPart>,
}

#[derive(Debug, Clone)]
struct KeyPart {
    /// What the user wrote, for messages.
    label: String,
    tokens: Vec<String>,
}

impl KeySpec {
    fn parse(options: &RecordOptions) -> Result<KeySpec> {
        let fail = |message: String| Err(DriftError::Parse(message));
        if options.whole_row {
            if !options.key.is_empty() {
                return fail("use either a key or the whole row as the key, not both".into());
            }
            return Ok(KeySpec { whole_row: true, parts: Vec::new() });
        }
        if options.key.is_empty() {
            return fail(
                "rows need a key: name the field that identifies a row, or match whole rows".into(),
            );
        }
        let mut parts: Vec<KeyPart> = Vec::new();
        for label in &options.key {
            let tokens =
                if label.starts_with('/') { split_pointer(label)? } else { vec![label.clone()] };
            if tokens.is_empty() {
                return fail("a key must name a field".into());
            }
            if let Some(other) = parts
                .iter()
                .find(|p| p.tokens.starts_with(&tokens) || tokens.starts_with(&p.tokens))
            {
                return fail(format!(
                    "the key parts `{}` and `{label}` overlap: one is inside the other",
                    other.label
                ));
            }
            parts.push(KeyPart { label: label.clone(), tokens });
        }
        Ok(KeySpec { whole_row: false, parts })
    }

    /// The key parts of a row that is already parsed, as JSON, for messages.
    fn text(&self, row: &Value) -> String {
        let value = |part: &KeyPart| {
            let pointer: String =
                part.tokens.iter().map(|t| format!("/{}", crate::escape_token(t))).collect();
            row.pointer(&pointer).cloned().unwrap_or(Value::Null)
        };
        match self.parts.as_slice() {
            [single] => value(single).to_string(),
            parts => Value::Array(parts.iter().map(value).collect()).to_string(),
        }
    }
}

/// What can be wrong with a row, found while reading its key.
enum Fault {
    NotObject,
    /// The key part at this position is missing from the row.
    NoKey(usize),
    /// The key part at this position is not a string, number or boolean.
    BadKey(usize),
    Json(serde_json::Error),
}

/// A file of records, indexed: one entry per row, in file order.
struct Rows {
    path: PathBuf,
    whole_row: bool,
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
    /// When repeated keys have been dropped, the rows that are left, as positions
    /// in the vectors above; `None` means all of them. Row numbers in the diff
    /// count these rows only.
    view: Option<Vec<u32>>,
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
    /// The number of rows that take part.
    fn len(&self) -> usize {
        self.view.as_ref().map_or(self.keys.len(), Vec::len)
    }

    /// Position in the index vectors of row `row`.
    fn at(&self, row: usize) -> usize {
        self.view.as_ref().map_or(row, |view| view[row] as usize)
    }

    fn key(&self, row: usize) -> u128 {
        self.keys[self.at(row)]
    }

    fn hash(&self, row: usize) -> u128 {
        self.hashes[self.at(row)]
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
            whole_row: spec.whole_row,
            keys: Vec::new(),
            hashes: Vec::new(),
            offsets: Vec::new(),
            end,
            view: None,
        };
        for part in parts {
            match part {
                Ok(part) => {
                    rows.keys.extend(part.keys);
                    rows.hashes.extend(part.hashes);
                    rows.offsets.extend(part.offsets);
                }
                Err(Failure { offset, fault }) => {
                    let label = |at: usize| spec.parts[at].label.as_str();
                    let message = match fault {
                        Fault::NotObject => "a row must be a JSON object".to_string(),
                        Fault::NoKey(at) => format!("the row has no `{}` field", label(at)),
                        Fault::BadKey(at) => {
                            format!("`{}` must be a string, number or boolean", label(at))
                        }
                        Fault::Json(error) => error.to_string(),
                    };
                    return Err(rows.error_at(offset, message));
                }
            }
        }
        Ok(rows)
    }

    /// Applies the policy for repeated keys.
    fn resolve(mut self, policy: Duplicates, spec: &KeySpec) -> Result<Rows> {
        let mut order: Vec<u32> = (0..self.keys.len() as u32).collect();
        order.sort_unstable_by_key(|&row| (self.keys[row as usize], row));
        match policy {
            Duplicates::Error => {
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
            }
            Duplicates::Occurrence => {
                // The nth row with a key gets the key of "that key, nth time".
                let mut start = 0;
                while start < order.len() {
                    let key = self.keys[order[start] as usize];
                    let mut end = start;
                    while end < order.len() && self.keys[order[end] as usize] == key {
                        end += 1;
                    }
                    for (nth, &row) in order[start + 1..end].iter().enumerate() {
                        self.keys[row as usize] = hash128(|h| (key, nth + 1).hash(h));
                    }
                    start = end;
                }
            }
            Duplicates::First | Duplicates::Last => {
                let mut keep = vec![false; self.keys.len()];
                let mut start = 0;
                while start < order.len() {
                    let key = self.keys[order[start] as usize];
                    let mut end = start;
                    while end < order.len() && self.keys[order[end] as usize] == key {
                        end += 1;
                    }
                    // `order` is sorted by row within a key.
                    let chosen =
                        if policy == Duplicates::First { order[start] } else { order[end - 1] };
                    keep[chosen as usize] = true;
                    start = end;
                }
                self.view =
                    Some((0..self.keys.len() as u32).filter(|&r| keep[r as usize]).collect());
            }
        }
        Ok(self)
    }

    /// The rows that take part, as row numbers, sorted by key.
    fn sorted(&self) -> Vec<u32> {
        let mut order: Vec<u32> = (0..self.len() as u32).collect();
        order.sort_unstable_by_key(|&row| (self.key(row as usize), row));
        order
    }

    /// The key of the row at index position `index`, as JSON, for a message.
    fn key_text(&self, index: usize, spec: &KeySpec) -> String {
        File::open(&self.path)
            .ok()
            .and_then(|mut file| self.read_at(&mut file, index).ok())
            .map_or_else(|| "?".to_string(), |row| spec.text(&row))
    }

    /// Reads row `row` back from the file.
    fn read(&self, file: &mut File, row: usize) -> Result<Value> {
        self.read_at(file, self.at(row))
    }

    /// Reads the row at index position `index` back from the file.
    fn read_at(&self, file: &mut File, index: usize) -> Result<Value> {
        let start = self.offsets[index];
        let stop = self.offsets.get(index + 1).copied().unwrap_or(self.end);
        let mut buffer = vec![0u8; (stop - start) as usize];
        file.seek(SeekFrom::Start(start))
            .and_then(|_| file.read_exact(&mut buffer))
            .map_err(|e| DriftError::Io(format!("{}: {e}", self.path.display())))?;
        serde_json::from_slice(trim(&buffer)).map_err(|e| self.error_at(start, e))
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

/// A hash of the scalar parts of a key that keeps the number 1, the string "1"
/// and the number 1.0 apart, as the in-memory diff does.
fn hash_key(parts: &[Value]) -> u128 {
    hash128(|hasher| {
        parts.len().hash(hasher);
        for key in parts {
            match key {
                Value::String(text) => (0u8, text).hash(hasher),
                Value::Bool(b) => (1u8, *b).hash(hasher),
                Value::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
                    (Some(i), _, _) => (2u8, i).hash(hasher),
                    (None, Some(u), _) => (3u8, u).hash(hasher),
                    (None, None, Some(f)) => (4u8, f.to_bits()).hash(hasher),
                    _ => 5u8.hash(hasher),
                },
                _ => 6u8.hash(hasher),
            }
        }
    })
}

/// A 128-bit fingerprint of a whole value such that equal values (as `==` on
/// `serde_json::Value`: object members in any order, `1` different from `1.0`)
/// have equal fingerprints. It is the key of a row matched by its whole content.
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
                            let pointer = if spec.starts_with('/') {
                                spec.clone()
                            } else {
                                format!("/{spec}")
                            };
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
            KeyedStep::Add(n) => {
                ops.push(Delta::with_value(Operation::Add, at(n), new[n].1.clone()))
            }
            KeyedStep::Pair { old: o, new: n } => {
                walk(&new[n].1, &old[o].1, &at(n), &ctx, &mut ops)
            }
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
        assert!(
            error.contains("`region`") && error.contains("string, number or boolean"),
            "{error}"
        );
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
        let old =
            [json!({"id": 1, "v": "a"}), json!({"id": 2, "v": "b"}), json!({"id": 1, "v": "c"})];
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
}

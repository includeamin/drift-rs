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

#[cfg(feature = "csv")]
mod csv;
mod external;
mod ndjson;
#[cfg(feature = "csv")]
pub use csv::{ColumnType, CsvOptions};

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

/// How the rows of a file are stored.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub enum RecordFormat {
    /// One JSON value per line (NDJSON, JSON Lines). The default.
    #[default]
    Ndjson,
    /// Delimited text with a header row: CSV, TSV and the like. Each row is read as
    /// an object of its cells, keyed by the column names. Needs the `csv` feature.
    #[cfg(feature = "csv")]
    Csv(CsvOptions),
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
    /// How the two files store their rows.
    pub format: RecordFormat,
    /// Keep about this many bytes of rows in memory at a time, sorting on disk the
    /// rest. `None` keeps an index of every row in memory. See
    /// [`memory_limit`](Self::memory_limit).
    pub memory_limit: Option<usize>,
    /// Where the temporary files of a bounded-memory diff go; the system's
    /// temporary directory if `None`.
    pub temp_dir: Option<PathBuf>,
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

    /// Sets how the files store their rows; NDJSON unless told otherwise.
    pub fn format(mut self, format: RecordFormat) -> Self {
        self.format = format;
        self
    }

    /// Diffs with about `bytes` of memory for sorting rows instead of an index of
    /// every row, so files with more rows than fit in memory can be diffed. Rows
    /// are sorted by key through temporary files, so it is slower than the default
    /// (several times for files that would fit), and it is a budget for the sort
    /// buffers rather than a cap on the whole process.
    ///
    /// It needs the rows that are in both files to be in the same order in each,
    /// which they are when a file is a later version of the other. If they are not
    /// the diff has to move rows, which needs memory for every row: it is then done
    /// in memory if the rows seem to fit in `bytes`, and is an error if not.
    pub fn memory_limit(mut self, bytes: usize) -> Self {
        self.memory_limit = Some(bytes);
        self
    }

    /// Sets the directory for the temporary files of a bounded-memory diff.
    pub fn temp_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.temp_dir = Some(dir.into());
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
    let mut operations = Vec::new();
    diff_records_each(old, new, options, |operation| {
        operations.push(operation);
        Ok(())
    })?;
    Ok(operations)
}

/// Like [`diff_records_with`], handing each operation to `emit` as it is worked
/// out instead of collecting them, in the same order.
///
/// The list of operations can be as large as the files, so a caller that writes
/// them out (to a file, a socket, a terminal) need not hold them all. If `emit`
/// returns an error the diff stops and that error is returned.
///
/// ```
/// use drift::{diff_records_each, RecordOptions};
///
/// let dir = std::env::temp_dir().join(format!("drift-each-{}", std::process::id()));
/// std::fs::create_dir_all(&dir)?;
/// let (old, new) = (dir.join("old.ndjson"), dir.join("new.ndjson"));
/// std::fs::write(&old, "{\"id\":1,\"n\":1}\n{\"id\":2,\"n\":2}\n")?;
/// std::fs::write(&new, "{\"id\":1,\"n\":5}\n")?;
///
/// let mut seen = Vec::new();
/// diff_records_each(&old, &new, &RecordOptions::new().key("id"), |operation| {
///     seen.push(format!("{} {}", operation.op.as_str(), operation.path));
///     Ok(())
/// })?;
/// assert_eq!(seen, ["remove /1", "replace /0/n"]);
/// # std::fs::remove_dir_all(&dir).ok();
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn diff_records_each(
    old: &Path,
    new: &Path,
    options: &RecordOptions,
    mut emit: impl FnMut(Delta) -> Result<()>,
) -> Result<()> {
    let spec = KeySpec::parse(options)?;
    let policy = options.duplicates.unwrap_or(if options.whole_row {
        Duplicates::Occurrence
    } else {
        Duplicates::Error
    });
    if let Some(limit) = options.memory_limit {
        match external::diff(old, new, options, &spec, policy, limit, &mut emit)? {
            external::Outcome::Done => return Ok(()),
            external::Outcome::Reordered(rows) => {
                // What the in-memory diff holds for a row of either file.
                let needed = rows.saturating_mul(IN_MEMORY_ROW as u64);
                if needed > limit as u64 {
                    return Err(DriftError::Parse(format!(
                        "the rows in both files are in a different order in each, which \
                         needs memory for every row (about {needed} bytes for {rows} rows) \
                         and the limit is {limit}"
                    )));
                }
            }
        }
    }
    let old_rows = Rows::index(old, &spec, &options.format)?.resolve(policy, &spec)?;
    let new_rows = Rows::index(new, &spec, &options.format)?.resolve(policy, &spec)?;
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
    let mut failure = None;
    plan_keyed(old_rows.len(), &matches, |step| {
        if failure.is_none() {
            if let Err(error) = apply(step, &old_rows, &new_rows, &mut files, &ctx, &mut emit) {
                failure = Some(error);
            }
        }
    });
    failure.map_or(Ok(()), Err)
}

/// Bytes the in-memory diff takes for a row, the index and the planning together.
const IN_MEMORY_ROW: usize = 64;

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
    emit: &mut dyn FnMut(Delta) -> Result<()>,
) -> Result<()> {
    let at = |index: usize| join_pointer("", &index.to_string());
    match step {
        KeyedStep::Remove(index) => emit(Delta::new(Operation::Remove, at(index))),
        KeyedStep::Move { from, to } => emit(Delta {
            op: Operation::Move,
            path: at(to),
            value: None,
            from_path: Some(at(from)),
        }),
        KeyedStep::Add(index) => {
            let row = new.read(&mut files.new, index)?;
            emit(Delta::with_value(Operation::Add, at(index), row))
        }
        KeyedStep::Pair { old: o, new: n } => {
            // With whole rows as keys, matched rows are equal rows by definition.
            // Otherwise equal bytes mean equal rows and nothing is read; rows that
            // differ in bytes only (field order, spacing) are read and found equal
            // by `walk`, which reports nothing for them.
            if !old.whole_row && old.hash(o) != new.hash(n) {
                let (old_row, new_row) =
                    (old.read(&mut files.old, o)?, new.read(&mut files.new, n)?);
                let mut operations = Vec::new();
                walk(&new_row, &old_row, &at(n), ctx, &mut operations);
                for operation in operations {
                    emit(operation)?;
                }
            }
            Ok(())
        }
    }
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
    /// Reading or writing failed; the message already names the file.
    Io(String),
}

/// 1-based line number of `offset` in a file, found by counting the lines before
/// it. Only errors need it, so it is not kept for every row.
fn line_of(path: &Path, offset: u64) -> u32 {
    let Ok(file) = File::open(path) else { return 0 };
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

/// A problem with the row that starts at `offset`, named by file and line.
fn error_at(path: &Path, offset: u64, message: impl fmt::Display) -> DriftError {
    DriftError::Parse(format!("{}: line {}: {message}", path.display(), line_of(path, offset)))
}

/// The error for a row that could not be indexed.
fn failure_error(path: &Path, spec: &KeySpec, failure: Failure) -> DriftError {
    let label = |at: usize| spec.parts[at].label.as_str();
    let message = match failure.fault {
        Fault::NotObject => "a row must be a JSON object".to_string(),
        Fault::NoKey(at) => format!("the row has no `{}` field", label(at)),
        Fault::BadKey(at) => format!("`{}` must be a string, number or boolean", label(at)),
        Fault::Json(error) => error.to_string(),
        Fault::Io(message) => return DriftError::Io(message),
    };
    error_at(path, failure.offset, message)
}

/// What a file's rows look like, which is needed to read one back.
enum Source {
    Ndjson,
    /// The header of the file, with its column types.
    #[cfg(feature = "csv")]
    Csv(csv::Layout),
}

/// A file of records, indexed: one entry per row, in file order.
struct Rows {
    path: PathBuf,
    source: Source,
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

/// Scans a file of rows one at a time, handing each to `sink`. Returns how to read
/// a row back, and the length of the file.
fn scan_file(
    path: &Path,
    spec: &KeySpec,
    format: &RecordFormat,
    sink: &mut dyn FnMut(Scanned) -> Result<()>,
) -> Result<(Source, u64)> {
    match format {
        RecordFormat::Ndjson => {
            let end = open(path)?
                .metadata()
                .map_err(|e| DriftError::Io(format!("{}: {e}", path.display())))?
                .len();
            // The sink's own error, kept aside because a `Failure` cannot carry it.
            let mut stopped = None;
            let result = ndjson::scan_part(path, 0, end, spec, &mut |row| {
                sink(row).map_err(|error| {
                    stopped = Some(error);
                    Failure { offset: 0, fault: Fault::Io(String::new()) }
                })
            });
            match result {
                Ok(()) => Ok((Source::Ndjson, end)),
                Err(failure) => Err(stopped.unwrap_or_else(|| failure_error(path, spec, failure))),
            }
        }
        #[cfg(feature = "csv")]
        RecordFormat::Csv(options) => {
            let (layout, end) = csv::scan(path, spec, options, sink)?;
            Ok((Source::Csv(layout), end))
        }
    }
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

/// One row found while scanning a file.
struct Scanned {
    key: u128,
    hash: u128,
    offset: u64,
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
        line_of(&self.path, offset)
    }

    fn error_at(&self, offset: u64, message: impl fmt::Display) -> DriftError {
        error_at(&self.path, offset, message)
    }

    fn index(path: &Path, spec: &KeySpec, format: &RecordFormat) -> Result<Rows> {
        match format {
            RecordFormat::Ndjson => Rows::index_ndjson(path, spec),
            #[cfg(feature = "csv")]
            RecordFormat::Csv(options) => csv::index(path, spec, options),
        }
    }

    fn index_ndjson(path: &Path, spec: &KeySpec) -> Result<Rows> {
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
            source: Source::Ndjson,
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
                Err(failure) => return Err(failure_error(path, spec, failure)),
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
        match &self.source {
            Source::Ndjson => {
                serde_json::from_slice(trim(&buffer)).map_err(|e| self.error_at(start, e))
            }
            #[cfg(feature = "csv")]
            Source::Csv(layout) => {
                csv::parse_row(&buffer, layout).map_err(|e| self.error_at(start, e))
            }
        }
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
mod tests;

//! Reading CSV, TSV and other delimited text as records.
//!
//! The first record is the header. Each later record is a row, read as an object
//! of its cells keyed by the column names, the same shape an NDJSON row has, so
//! everything built on rows (keys, duplicate policies, whole-row matching, ignored
//! paths) works unchanged. Cells are strings unless their column is given a type.
//!
//! The reader skips blank lines and a leading byte order mark itself. Unlike NDJSON
//! the file is read on one thread: a quoted cell can contain a line
//! break, so a line start is not always a row start and the file cannot be cut up
//! at newlines.

use super::{
    error_at, fingerprint, hash128, hash_key, open, KeySpec, Result, Rows, Scanned, Source,
};
use crate::DriftError;
use serde_json::Value;
use std::hash::Hash;
use std::io::{BufReader, Read};
use std::path::Path;

/// How the text of a CSV cell is turned into a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ColumnType {
    /// The cell as it is, whatever it looks like: `007` stays `"007"`. The default.
    String,
    /// A whole number. An empty cell is `null`.
    Int,
    /// A finite number. An empty cell is `null`.
    Float,
    /// `true` or `false`, in any case. An empty cell is `null`.
    Bool,
    /// A JSON value written in the cell. An empty cell is `null`.
    Json,
}

/// How a CSV or TSV file is read.
///
/// ```
/// use drift::{ColumnType, CsvOptions};
///
/// let options = CsvOptions::new().delimiter(b';').column("id", ColumnType::Int);
/// assert_eq!(options.delimiter, b';');
/// ```
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CsvOptions {
    /// The byte between cells: `,` by default, `\t` for TSV.
    pub delimiter: u8,
    /// Columns that are not plain strings, by name. A name that is not in a file's
    /// header is an error, so a misspelt column is not silently left as text.
    pub columns: Vec<(String, ColumnType)>,
}

impl Default for CsvOptions {
    fn default() -> Self {
        Self { delimiter: b',', columns: Vec::new() }
    }
}

impl CsvOptions {
    /// Comma-separated, every cell a string.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the byte between cells.
    pub fn delimiter(mut self, delimiter: u8) -> Self {
        self.delimiter = delimiter;
        self
    }

    /// Gives a column a type; see [`ColumnType`].
    pub fn column(mut self, name: impl Into<String>, kind: ColumnType) -> Self {
        self.columns.push((name.into(), kind));
        self
    }
}

/// What a file's header says: the columns, their types, and the delimiter, which
/// are all needed to read a row back.
pub(super) struct Layout {
    delimiter: u8,
    names: Vec<String>,
    types: Vec<ColumnType>,
}

impl Layout {
    /// A file with no header at all has no rows.
    fn empty(delimiter: u8) -> Layout {
        Layout { delimiter, names: Vec::new(), types: Vec::new() }
    }

    /// Mixed into every row's hash, so two rows with the same cells under different
    /// columns or types are never taken for equal.
    fn seed(&self) -> u128 {
        hash128(|hasher| {
            self.names.hash(hasher);
            self.types.iter().map(|&t| t as u8).collect::<Vec<_>>().hash(hasher);
        })
    }

    fn reader<R: Read>(&self, input: R) -> csv::Reader<R> {
        csv::ReaderBuilder::new()
            .delimiter(self.delimiter)
            .has_headers(false)
            .flexible(true)
            .from_reader(input)
    }
}

/// The value of one cell, or what is wrong with it.
fn cell_value(cell: &[u8], kind: ColumnType, name: &str) -> std::result::Result<Value, String> {
    let Ok(text) = std::str::from_utf8(cell) else {
        return Err(format!("column `{name}` is not valid UTF-8"));
    };
    let wrong = |what: &str| format!("column `{name}`: `{text}` is {what}");
    Ok(match kind {
        ColumnType::String => Value::String(text.to_string()),
        _ if text.is_empty() => Value::Null,
        ColumnType::Int => text
            .parse::<i64>()
            .map(Value::from)
            .or_else(|_| text.parse::<u64>().map(Value::from))
            .map_err(|_| wrong("not an integer"))?,
        ColumnType::Float => text
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .ok_or_else(|| wrong("not a finite number"))?,
        ColumnType::Bool => match text.to_ascii_lowercase().as_str() {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => return Err(wrong("not a boolean (true or false)")),
        },
        ColumnType::Json => serde_json::from_str(text)
            .map_err(|e| format!("column `{name}`: `{text}` is not valid JSON ({e})"))?,
    })
}

/// A whole record as an object, or what is wrong with it.
fn row_value(record: &csv::ByteRecord, layout: &Layout) -> std::result::Result<Value, String> {
    if record.len() != layout.names.len() {
        return Err(format!("expected {} fields, found {}", layout.names.len(), record.len()));
    }
    let mut row = serde_json::Map::with_capacity(record.len());
    for ((name, kind), cell) in layout.names.iter().zip(&layout.types).zip(record) {
        row.insert(name.clone(), cell_value(cell, *kind, name)?);
    }
    Ok(Value::Object(row))
}

/// Reads one row back from the bytes between its start and the next row's.
pub(super) fn parse_row(bytes: &[u8], layout: &Layout) -> std::result::Result<Value, String> {
    let mut reader = layout.reader(bytes);
    let mut record = csv::ByteRecord::new();
    match reader.read_byte_record(&mut record) {
        Ok(true) => row_value(&record, layout),
        Ok(false) => Err("the row is empty".into()),
        Err(e) => Err(e.to_string()),
    }
}

/// Indexes a CSV file: one entry per row, as for NDJSON.
pub(super) fn index(path: &Path, spec: &KeySpec, options: &CsvOptions) -> Result<Rows> {
    let mut rows = Rows {
        path: path.to_path_buf(),
        source: Source::Csv(Layout::empty(options.delimiter)),
        whole_row: spec.whole_row,
        keys: Vec::new(),
        hashes: Vec::new(),
        offsets: Vec::new(),
        end: 0,
        view: None,
    };
    let (layout, end) = scan(path, spec, options, &mut |row| {
        rows.keys.push(row.key);
        rows.hashes.push(row.hash);
        rows.offsets.push(row.offset);
        Ok(())
    })?;
    rows.source = Source::Csv(layout);
    rows.end = end;
    Ok(rows)
}

/// Reads a CSV file one row at a time, handing each to `sink` without keeping it.
/// Returns the layout from the header, and the length of the file.
pub(super) fn scan(
    path: &Path,
    spec: &KeySpec,
    options: &CsvOptions,
    sink: &mut dyn FnMut(Scanned) -> Result<()>,
) -> Result<(Layout, u64)> {
    let file = open(path)?;
    let end =
        file.metadata().map_err(|e| DriftError::Io(format!("{}: {e}", path.display())))?.len();
    let io = |e: &dyn std::fmt::Display| DriftError::Io(format!("{}: {e}", path.display()));

    let plain = Layout::empty(options.delimiter);
    let mut reader = plain.reader(BufReader::with_capacity(1 << 20, file));
    let mut record = csv::ByteRecord::new();
    if !reader.read_byte_record(&mut record).map_err(|e| io(&e))? {
        return Ok((plain, end)); // nothing at all, not even a header
    }

    // The header: names that exist, are not empty and are not repeated.
    let mut names = Vec::with_capacity(record.len());
    for cell in &record {
        let name = std::str::from_utf8(cell)
            .map_err(|_| error_at(path, 0, "a column name is not valid UTF-8"))?;
        if name.is_empty() {
            return Err(error_at(path, 0, "a column name is empty"));
        }
        if names.iter().any(|n: &String| n == name) {
            return Err(error_at(
                path,
                0,
                format!("the column `{name}` appears twice in the header"),
            ));
        }
        names.push(name.to_string());
    }
    let mut types = vec![ColumnType::String; names.len()];
    for (name, kind) in &options.columns {
        match names.iter().position(|n| n == name) {
            Some(at) => types[at] = *kind,
            None => {
                return Err(DriftError::Parse(format!(
                    "{}: the column `{name}` is given a type but is not in the header",
                    path.display()
                )))
            }
        }
    }
    let layout = Layout { delimiter: options.delimiter, names, types };

    // Where the key lives: CSV rows are flat, so each part is a column.
    let mut key_columns = Vec::with_capacity(spec.parts.len());
    for part in &spec.parts {
        let [column] = part.tokens.as_slice() else {
            return Err(DriftError::Parse(format!(
                "{}: CSV rows are flat, so the key `{}` cannot reach into a row; name a column",
                path.display(),
                part.label
            )));
        };
        let at = layout.names.iter().position(|n| n == column).ok_or_else(|| {
            DriftError::Parse(format!(
                "{}: the file has no column `{}`",
                path.display(),
                part.label
            ))
        })?;
        key_columns.push(at);
    }

    let seed = layout.seed();
    while reader.read_byte_record(&mut record).map_err(|e| io(&e))? {
        let start = record.position().map_or(0, |p| p.byte());
        let problem = |message: String| error_at(path, start, message);
        let key = if spec.whole_row {
            fingerprint(&row_value(&record, &layout).map_err(problem)?)
        } else {
            if record.len() != layout.names.len() {
                return Err(problem(format!(
                    "expected {} fields, found {}",
                    layout.names.len(),
                    record.len()
                )));
            }
            let mut parts = Vec::with_capacity(key_columns.len());
            for (&at, part) in key_columns.iter().zip(&spec.parts) {
                let value = cell_value(&record[at], layout.types[at], &layout.names[at])
                    .map_err(problem)?;
                if !matches!(value, Value::String(_) | Value::Number(_) | Value::Bool(_)) {
                    return Err(error_at(
                        path,
                        start,
                        format!("`{}` must be a string, number or boolean", part.label),
                    ));
                }
                parts.push(value);
            }
            hash_key(&parts)
        };
        let hash = hash128(|hasher| {
            seed.hash(hasher);
            record.len().hash(hasher);
            for cell in &record {
                cell.hash(hasher);
            }
        });
        sink(Scanned { key, hash, offset: start })?;
    }
    Ok((layout, end))
}

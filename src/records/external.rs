//! Diffing files with more rows than fit in memory.
//!
//! Nothing here is held per row. Each file is scanned once and its rows are sorted
//! by key on disk (see `sorter`). The two sorted streams are then merge-joined,
//! which finds the rows that are only in one file and the rows that are in both.
//! What the join finds is sorted again, by row number, because that is the order
//! the operations come in; and the rows are read back from the files only when an
//! operation needs them.
//!
//! The operations are the ones the in-memory diff gives, provided the rows that
//! are in both files are in the same order in both. If they are not, the diff needs
//! `move` operations, and working those out takes memory for every row; that is
//! reported as [`Outcome::Reordered`] before anything has been emitted.

use super::{
    error_at, line_of, open, scan_file, trim, Duplicates, KeySpec, RecordFormat, RecordOptions,
    Result, Scanned, Source,
};
use crate::diff::{walk, Context};
use crate::sorter::{Record, Sorted, Sorter};
use crate::{join_pointer, Delta, DriftError, Operation};
use serde_json::Value;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// How the diff ended, if it did not fail.
pub(super) enum Outcome {
    Done,
    /// Rows in both files are in a different order in each. Nothing was emitted.
    /// The count is the rows in the two files together.
    Reordered(u64),
}

/// What a row is sorted and joined by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Row {
    key: u128,
    /// Position in the file among the rows, counting from 0.
    ordinal: u64,
    hash: u128,
    offset: u64,
}

impl Record for Row {
    const SIZE: usize = 48;

    fn encode(&self, out: &mut [u8]) {
        out[..16].copy_from_slice(&self.key.to_le_bytes());
        out[16..24].copy_from_slice(&self.ordinal.to_le_bytes());
        out[24..40].copy_from_slice(&self.hash.to_le_bytes());
        out[40..48].copy_from_slice(&self.offset.to_le_bytes());
    }

    fn decode(bytes: &[u8]) -> Self {
        Row {
            key: u128::from_le_bytes(bytes[..16].try_into().expect("16 bytes")),
            ordinal: u64::from_le_bytes(bytes[16..24].try_into().expect("8 bytes")),
            hash: u128::from_le_bytes(bytes[24..40].try_into().expect("16 bytes")),
            offset: u64::from_le_bytes(bytes[40..48].try_into().expect("8 bytes")),
        }
    }
}

/// A row ordinal, sorted from the highest down: removals are emitted that way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Descending(u64);

impl Ord for Descending {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other.0.cmp(&self.0)
    }
}

impl PartialOrd for Descending {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Record for Descending {
    const SIZE: usize = 8;

    fn encode(&self, out: &mut [u8]) {
        out.copy_from_slice(&self.0.to_le_bytes());
    }

    fn decode(bytes: &[u8]) -> Self {
        Descending(u64::from_le_bytes(bytes.try_into().expect("8 bytes")))
    }
}

/// What is not a removal, with the rows it concerns: a row of the new file that is
/// added or that matches a row of the old one. Sorted by position in the new file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Kept {
    ordinal: u64,
    offset: u64,
    old_ordinal: u64,
    old_offset: u64,
    kind: Kind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Added,
    /// Matched to a row of the old file whose bytes differ.
    Changed,
    /// Matched to a row of the old file with the same bytes.
    Same,
}

impl Record for Kept {
    const SIZE: usize = 33;

    fn encode(&self, out: &mut [u8]) {
        out[..8].copy_from_slice(&self.ordinal.to_le_bytes());
        out[8..16].copy_from_slice(&self.offset.to_le_bytes());
        out[16..24].copy_from_slice(&self.old_ordinal.to_le_bytes());
        out[24..32].copy_from_slice(&self.old_offset.to_le_bytes());
        out[32] = self.kind as u8;
    }

    fn decode(bytes: &[u8]) -> Self {
        let number = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"));
        Kept {
            ordinal: number(0),
            offset: number(8),
            old_ordinal: number(16),
            old_offset: number(24),
            kind: match bytes[32] {
                0 => Kind::Added,
                1 => Kind::Changed,
                _ => Kind::Same,
            },
        }
    }
}

fn disk(error: io::Error) -> DriftError {
    DriftError::Io(format!("temporary files: {error}"))
}

/// A file that has been scanned: how to read its rows back, and how many there are.
struct Input {
    path: PathBuf,
    source: Source,
    rows: u64,
}

impl Input {
    /// Scans `path`, returning its rows sorted by key (then by position).
    fn sort(
        path: &Path,
        spec: &KeySpec,
        format: &RecordFormat,
        budget: usize,
        dir: &Path,
    ) -> Result<(Input, Sorted<Row>)> {
        let mut sorter = Sorter::<Row>::new(budget, dir.to_path_buf());
        let mut ordinal = 0u64;
        let (source, _end) = scan_file(path, spec, format, &mut |row: Scanned| {
            sorter
                .push(Row { key: row.key, ordinal, hash: row.hash, offset: row.offset })
                .map_err(disk)?;
            ordinal += 1;
            Ok(())
        })?;
        let sorted = sorter.finish().map_err(disk)?;
        Ok((Input { path: path.to_path_buf(), source, rows: ordinal }, sorted))
    }

    /// The error for two rows with the same key.
    fn duplicate(&self, spec: &KeySpec, first: &Row, second: &Row) -> DriftError {
        let key = RowReader::open(&self.path)
            .and_then(|mut reader| reader.read(first.offset, &self.source))
            .map_or_else(|_| "?".to_string(), |row| spec.text(&row));
        error_at(
            &self.path,
            second.offset,
            format!(
                "duplicate key {key} (first seen at line {})",
                line_of(&self.path, first.offset)
            ),
        )
    }
}

/// A sorted stream of rows with its next row in view.
struct Cursor {
    rows: Sorted<Row>,
    head: Option<Row>,
}

impl Cursor {
    fn new(rows: Sorted<Row>) -> Result<Cursor> {
        let mut cursor = Cursor { rows, head: None };
        cursor.advance()?;
        Ok(cursor)
    }

    /// Takes the row in view and brings the next one into it.
    fn advance(&mut self) -> Result<Option<Row>> {
        let taken = self.head;
        self.head = self.rows.next().transpose().map_err(disk)?;
        Ok(taken)
    }

    /// The next row, if it has this key.
    fn has(&self, key: u128) -> bool {
        self.head.is_some_and(|row| row.key == key)
    }
}

/// Reads rows back from a file by where they start.
struct RowReader {
    file: File,
    path: PathBuf,
    /// The bytes of the file from `start` on that have been read, a stretch at a
    /// time. Rows are mostly asked for in file order, so most come from here.
    window: Vec<u8>,
    start: u64,
    eof: bool,
}

const CHUNK: usize = 64 * 1024;
/// How much of the window behind the row being read is kept.
const BEHIND: usize = 1 << 20;

impl RowReader {
    fn open(path: &Path) -> Result<RowReader> {
        Ok(RowReader {
            file: open(path)?,
            path: path.to_path_buf(),
            window: Vec::new(),
            start: 0,
            eof: false,
        })
    }

    /// The bytes of the row that starts at `offset`; `end_of` says where a row ends
    /// given the bytes from its start, or `None` if they run out first.
    fn bytes(&mut self, offset: u64, end_of: &dyn Fn(&[u8]) -> Option<usize>) -> Result<&[u8]> {
        let io = |e: io::Error| DriftError::Io(format!("{}: {e}", self.path.display()));
        let reach = self.start + self.window.len() as u64;
        if offset < self.start || offset > reach {
            self.window.clear();
            self.start = offset;
            self.eof = false;
            self.file.seek(SeekFrom::Start(offset)).map_err(io)?;
        } else if (offset - self.start) as usize > BEHIND {
            self.window.drain(..(offset - self.start) as usize);
            self.start = offset;
        }
        let from = (offset - self.start) as usize;
        let length = loop {
            if let Some(length) = end_of(&self.window[from..]) {
                break length;
            }
            if self.eof {
                break self.window.len() - from;
            }
            let before = self.window.len();
            self.window.resize(before + CHUNK, 0);
            let read = self.file.read(&mut self.window[before..]).map_err(io)?;
            self.window.truncate(before + read);
            self.eof = read == 0;
        };
        Ok(&self.window[from..from + length])
    }

    /// Reads the row that starts at `offset`.
    fn read(&mut self, offset: u64, source: &Source) -> Result<Value> {
        let path = self.path.clone();
        match source {
            Source::Ndjson => {
                let line = self.bytes(offset, &|bytes| {
                    bytes.iter().position(|&b| b == b'\n').map(|at| at + 1)
                })?;
                serde_json::from_slice(trim(line)).map_err(|e| error_at(&path, offset, e))
            }
            #[cfg(feature = "csv")]
            Source::Csv(layout) => {
                let record = self.bytes(offset, &|bytes| super::csv::record_len(bytes, layout))?;
                super::csv::parse_row(record, layout).map_err(|e| error_at(&path, offset, e))
            }
        }
    }
}

/// Everything the join produces for the next stage.
struct Joined {
    /// Rows only in the old file.
    removed: Sorter<Descending>,
    /// Rows of the old file left out as repeats, and how many.
    dropped: Sorter<Descending>,
    dropped_count: u64,
    /// Rows of the new file that are added or matched.
    kept: Sorter<Kept>,
}

/// Takes the rows of one key from a cursor and returns the one that takes part,
/// according to the policy. Rows left out are handed to `dropped`.
fn one_of(
    cursor: &mut Cursor,
    policy: Duplicates,
    refuse: &dyn Fn(&Row, &Row) -> DriftError,
    dropped: &mut dyn FnMut(&Row) -> Result<()>,
) -> Result<Row> {
    let first = cursor.advance()?.expect("a key has a row");
    let mut chosen = first;
    while cursor.has(first.key) {
        let next = cursor.advance()?.expect("checked");
        match policy {
            Duplicates::Error => return Err(refuse(&first, &next)),
            Duplicates::First => dropped(&next)?,
            Duplicates::Last => {
                dropped(&chosen)?;
                chosen = next;
            }
            Duplicates::Occurrence => unreachable!("occurrences are matched, not chosen"),
        }
    }
    Ok(chosen)
}

fn pair(old: &Row, new: &Row) -> Kept {
    Kept {
        ordinal: new.ordinal,
        offset: new.offset,
        old_ordinal: old.ordinal,
        old_offset: old.offset,
        kind: if old.hash == new.hash { Kind::Same } else { Kind::Changed },
    }
}

fn added(new: &Row) -> Kept {
    Kept {
        ordinal: new.ordinal,
        offset: new.offset,
        old_ordinal: 0,
        old_offset: 0,
        kind: Kind::Added,
    }
}

/// Walks the two sorted streams together.
fn join(
    old: (&Input, Sorted<Row>),
    new: (&Input, Sorted<Row>),
    spec: &KeySpec,
    policy: Duplicates,
    budget: usize,
    dir: &Path,
) -> Result<Joined> {
    let (old_input, new_input) = (old.0, new.0);
    let (mut old, mut new) = (Cursor::new(old.1)?, Cursor::new(new.1)?);
    let mut joined = Joined {
        removed: Sorter::new(budget, dir.to_path_buf()),
        dropped: Sorter::new(budget, dir.to_path_buf()),
        dropped_count: 0,
        kept: Sorter::new(budget, dir.to_path_buf()),
    };
    let old_refuses = |a: &Row, b: &Row| old_input.duplicate(spec, a, b);
    let new_refuses = |a: &Row, b: &Row| new_input.duplicate(spec, a, b);

    loop {
        let key = match (old.head, new.head) {
            (None, None) => break,
            (Some(row), None) | (None, Some(row)) => row.key,
            (Some(a), Some(b)) => a.key.min(b.key),
        };

        if policy == Duplicates::Occurrence {
            // The nth row with this key in one file goes with the nth in the other.
            loop {
                match (old.has(key), new.has(key)) {
                    (true, true) => {
                        let (o, n) = (old.advance()?, new.advance()?);
                        let (o, n) = (o.expect("checked"), n.expect("checked"));
                        joined.kept.push(pair(&o, &n)).map_err(disk)?;
                    }
                    (true, false) => {
                        let o = old.advance()?.expect("checked");
                        joined.removed.push(Descending(o.ordinal)).map_err(disk)?;
                    }
                    (false, true) => {
                        let n = new.advance()?.expect("checked");
                        joined.kept.push(added(&n)).map_err(disk)?;
                    }
                    (false, false) => break,
                }
            }
            continue;
        }

        let in_old = if old.has(key) {
            let (dropped, count) = (&mut joined.dropped, &mut joined.dropped_count);
            Some(one_of(&mut old, policy, &old_refuses, &mut |row| {
                *count += 1;
                dropped.push(Descending(row.ordinal)).map_err(disk)
            })?)
        } else {
            None
        };
        let in_new = if new.has(key) {
            // Which rows of the new file were left out does not matter: its rows
            // are numbered as they are read back, in order.
            Some(one_of(&mut new, policy, &new_refuses, &mut |_| Ok(()))?)
        } else {
            None
        };
        match (in_old, in_new) {
            (Some(o), Some(n)) => joined.kept.push(pair(&o, &n)).map_err(disk)?,
            (Some(o), None) => joined.removed.push(Descending(o.ordinal)).map_err(disk)?,
            (None, Some(n)) => joined.kept.push(added(&n)).map_err(disk)?,
            (None, None) => unreachable!("the key came from one of them"),
        }
    }
    Ok(joined)
}

/// The operations between two files, with sorting done on disk and about `limit`
/// bytes of rows held in memory at a time.
pub(super) fn diff(
    old: &Path,
    new: &Path,
    options: &RecordOptions,
    spec: &KeySpec,
    policy: Duplicates,
    limit: usize,
    emit: &mut dyn FnMut(Delta) -> Result<()>,
) -> Result<Outcome> {
    let dir = options.temp_dir.clone().unwrap_or_else(std::env::temp_dir);
    if !dir.is_dir() {
        return Err(DriftError::Io(format!(
            "{}: the directory for temporary files does not exist",
            dir.display()
        )));
    }
    // Up to this many sorters hold a batch of rows at once.
    let budget = (limit / 8).max(1);

    let (old_input, old_rows) = Input::sort(old, spec, &options.format, budget, &dir)?;
    let (new_input, new_rows) = Input::sort(new, spec, &options.format, budget, &dir)?;
    let joined = join((&old_input, old_rows), (&new_input, new_rows), spec, policy, budget, &dir)?;

    // Rows in both files must be in the same order in each. This is seen only once
    // they are in order of the new file, so look before anything is emitted.
    let mut kept = Sorter::<Kept>::new(budget, dir.clone());
    let mut last_old = None;
    for row in joined.kept.finish().map_err(disk)? {
        let row = row.map_err(disk)?;
        if row.kind != Kind::Added {
            if last_old.is_some_and(|last| last > row.old_ordinal) {
                return Ok(Outcome::Reordered(old_input.rows + new_input.rows));
            }
            last_old = Some(row.old_ordinal);
        }
        kept.push(row).map_err(disk)?;
    }

    let at = |index: u64| join_pointer("", &index.to_string());

    // Removals, highest first. A row's number counts the rows of the file that were
    // not left out as repeats, so those before it are taken off.
    let mut dropped = joined.dropped.finish().map_err(disk)?.peekable();
    let mut dropped_above = 0u64;
    for removed in joined.removed.finish().map_err(disk)? {
        let ordinal = removed.map_err(disk)?.0;
        loop {
            match dropped.peek() {
                Some(Ok(above)) if above.0 > ordinal => {
                    dropped_above += 1;
                    dropped.next();
                }
                Some(Err(_)) => {
                    return Err(disk(dropped.next().expect("peeked").expect_err("an error")))
                }
                _ => break,
            }
        }
        let number = ordinal - (joined.dropped_count - dropped_above);
        emit(Delta::new(Operation::Remove, at(number)))?;
    }

    let mut files = (RowReader::open(old)?, RowReader::open(new)?);
    let ctx = Context::new(&options.diff);
    for (number, row) in kept.finish().map_err(disk)?.enumerate() {
        let (row, number) = (row.map_err(disk)?, number as u64);
        match row.kind {
            Kind::Added => {
                let value = files.1.read(row.offset, &new_input.source)?;
                emit(Delta::with_value(Operation::Add, at(number), value))?;
            }
            // With whole rows as keys, matched rows are equal rows by definition.
            Kind::Changed if !spec.whole_row => {
                let old_row = files.0.read(row.old_offset, &old_input.source)?;
                let new_row = files.1.read(row.offset, &new_input.source)?;
                let mut operations = Vec::new();
                walk(&new_row, &old_row, &at(number), &ctx, &mut operations);
                for operation in operations {
                    emit(operation)?;
                }
            }
            Kind::Changed | Kind::Same => {}
        }
    }
    Ok(Outcome::Done)
}

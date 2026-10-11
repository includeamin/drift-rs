//! Sorting more records than fit in memory.
//!
//! An external merge sort: records are collected in memory up to a budget, each
//! full batch is sorted and written to a temporary file as a run, and the runs are
//! merged back as one sorted stream. Runs are merged a bounded number at a time, in
//! passes if there are many, so only a few files are ever open. Input that fits in
//! one batch never touches the disk.
//!
//! Records are fixed-size, which keeps reading a run back to a plain `read_exact`.

// Used by the bounded-memory diff, which comes in the next commit.
#![cfg_attr(not(test), allow(dead_code))]

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// A record that can be sorted and written to a temporary file.
pub(crate) trait Record: Copy + Ord {
    /// Bytes a record takes on disk.
    const SIZE: usize;
    fn encode(&self, out: &mut [u8]);
    fn decode(bytes: &[u8]) -> Self;
}

/// Runs merged at once. Each open run costs a file handle and a read buffer.
const DEFAULT_FAN_IN: usize = 64;
const BUFFER: usize = 64 * 1024;

/// A temporary file holding one sorted run, removed when dropped.
struct Run {
    path: PathBuf,
    records: usize,
}

impl Run {
    fn create(dir: &Path) -> io::Result<(Run, BufWriter<File>)> {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = dir.join(format!(
            "drift-sort-{}-{}.run",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let file = OpenOptions::new().write(true).create_new(true).open(&path)?;
        Ok((Run { path, records: 0 }, BufWriter::with_capacity(BUFFER, file)))
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Collects records and hands them back sorted.
pub(crate) struct Sorter<T: Record> {
    batch: Vec<T>,
    run_len: usize,
    fan_in: usize,
    dir: PathBuf,
    runs: Vec<Run>,
}

impl<T: Record> Sorter<T> {
    /// A sorter that keeps about `budget` bytes of records in memory at a time and
    /// writes its temporary files to `dir`.
    pub(crate) fn new(budget: usize, dir: PathBuf) -> Self {
        Self::with_run_len((budget / std::mem::size_of::<T>()).max(1), DEFAULT_FAN_IN, dir)
    }

    /// A sorter with the batch size, and the number of runs merged at once, given
    /// directly.
    pub(crate) fn with_run_len(run_len: usize, fan_in: usize, dir: PathBuf) -> Self {
        Self {
            batch: Vec::new(),
            run_len: run_len.max(1),
            fan_in: fan_in.max(2),
            dir,
            runs: Vec::new(),
        }
    }

    pub(crate) fn push(&mut self, record: T) -> io::Result<()> {
        if self.batch.len() == self.run_len {
            self.spill()?;
        }
        self.batch.push(record);
        Ok(())
    }

    /// Sorts the batch and writes it out as a run.
    fn spill(&mut self) -> io::Result<()> {
        self.batch.sort_unstable();
        let (mut run, mut out) = Run::create(&self.dir)?;
        let mut bytes = vec![0u8; T::SIZE];
        for record in &self.batch {
            record.encode(&mut bytes);
            out.write_all(&bytes)?;
        }
        out.flush()?;
        run.records = self.batch.len();
        self.runs.push(run);
        self.batch.clear();
        Ok(())
    }

    /// Merges the oldest runs together until few enough are left to open at once.
    fn merge_down(&mut self) -> io::Result<()> {
        while self.runs.len() > self.fan_in {
            let group: Vec<Run> = self.runs.drain(..self.fan_in).collect();
            let merged = merge_to_run::<T>(group, &self.dir)?;
            self.runs.push(merged);
        }
        Ok(())
    }

    /// Ends the input and returns the records in order.
    pub(crate) fn finish(mut self) -> io::Result<Sorted<T>> {
        self.batch.sort_unstable();
        self.merge_down()?;
        let memory = std::mem::take(&mut self.batch);
        let runs = std::mem::take(&mut self.runs);
        Ok(Sorted(Merger::new(memory, runs)?))
    }
}

/// Records in order, read back from memory and from the runs on disk.
pub(crate) struct Sorted<T: Record>(Merger<T>);

impl<T: Record> Iterator for Sorted<T> {
    type Item = io::Result<T>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
    }
}

/// Where the records of one sorted source come from.
enum Source<T: Record> {
    Memory(std::vec::IntoIter<T>),
    Disk { reader: BufReader<File>, remaining: usize, bytes: Vec<u8>, _run: Run },
}

impl<T: Record> Source<T> {
    fn next(&mut self) -> io::Result<Option<T>> {
        match self {
            Source::Memory(records) => Ok(records.next()),
            Source::Disk { reader, remaining, bytes, .. } => {
                if *remaining == 0 {
                    return Ok(None);
                }
                reader.read_exact(bytes)?;
                *remaining -= 1;
                Ok(Some(T::decode(bytes)))
            }
        }
    }
}

/// A k-way merge of sorted sources.
struct Merger<T: Record> {
    sources: Vec<Source<T>>,
    heap: BinaryHeap<Reverse<(T, usize)>>,
}

impl<T: Record> Merger<T> {
    fn new(memory: Vec<T>, runs: Vec<Run>) -> io::Result<Self> {
        let mut sources = Vec::with_capacity(runs.len() + 1);
        if !memory.is_empty() {
            sources.push(Source::Memory(memory.into_iter()));
        }
        for run in runs {
            let file = File::open(&run.path)?;
            sources.push(Source::Disk {
                reader: BufReader::with_capacity(BUFFER, file),
                remaining: run.records,
                bytes: vec![0u8; T::SIZE],
                _run: run,
            });
        }
        let mut merger = Merger { sources, heap: BinaryHeap::new() };
        for at in 0..merger.sources.len() {
            if let Some(record) = merger.sources[at].next()? {
                merger.heap.push(Reverse((record, at)));
            }
        }
        Ok(merger)
    }

    fn next(&mut self) -> Option<io::Result<T>> {
        let Reverse((record, at)) = self.heap.pop()?;
        match self.sources[at].next() {
            Ok(Some(following)) => self.heap.push(Reverse((following, at))),
            Ok(None) => {}
            Err(error) => {
                self.heap.clear();
                return Some(Err(error));
            }
        }
        Some(Ok(record))
    }
}

/// Merges runs into one new run.
fn merge_to_run<T: Record>(group: Vec<Run>, dir: &Path) -> io::Result<Run> {
    let mut merger = Merger::<T>::new(Vec::new(), group)?;
    let (mut run, mut out) = Run::create(dir)?;
    let mut bytes = vec![0u8; T::SIZE];
    while let Some(record) = merger.next() {
        record?.encode(&mut bytes);
        out.write_all(&bytes)?;
        run.records += 1;
    }
    out.flush()?;
    Ok(run)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A record with a key to sort by and a payload that tells equal keys apart.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    struct Pair {
        key: u64,
        payload: u64,
    }

    impl Record for Pair {
        const SIZE: usize = 16;
        fn encode(&self, out: &mut [u8]) {
            out[..8].copy_from_slice(&self.key.to_le_bytes());
            out[8..].copy_from_slice(&self.payload.to_le_bytes());
        }
        fn decode(bytes: &[u8]) -> Self {
            Self {
                key: u64::from_le_bytes(bytes[..8].try_into().unwrap()),
                payload: u64::from_le_bytes(bytes[8..].try_into().unwrap()),
            }
        }
    }

    /// A scratch directory that is removed afterwards.
    struct Dir(std::path::PathBuf);

    impl Dir {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "drift-sorter-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn files(&self) -> usize {
            std::fs::read_dir(&self.0).unwrap().count()
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    fn random_pairs(count: usize, seed: u64, key_range: u64) -> Vec<Pair> {
        // xorshift: deterministic, no extra dependencies.
        let mut state = seed | 1;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        (0..count).map(|_| Pair { key: next() % key_range, payload: next() }).collect()
    }

    fn sort_with(items: &[Pair], run_len: usize, fan_in: usize, dir: &Dir) -> Vec<Pair> {
        let mut sorter = Sorter::<Pair>::with_run_len(run_len, fan_in, dir.0.clone());
        for &item in items {
            sorter.push(item).unwrap();
        }
        sorter.finish().unwrap().collect::<std::io::Result<Vec<_>>>().unwrap()
    }

    #[test]
    fn the_output_is_what_sorting_in_memory_gives_whatever_the_run_length() {
        let dir = Dir::new();
        for count in [0usize, 1, 2, 7, 64, 65, 500, 2000] {
            let items = random_pairs(count, 0x9E3779B97F4A7C15 ^ count as u64, 50);
            let mut expected = items.clone();
            expected.sort();
            for run_len in [1usize, 2, 3, 10, 64, 1000, 100_000] {
                assert_eq!(
                    sort_with(&items, run_len, 8, &dir),
                    expected,
                    "{count} records, runs of {run_len}"
                );
            }
        }
    }

    #[test]
    fn many_runs_are_merged_in_passes_so_few_files_are_open_at_once() {
        let dir = Dir::new();
        let items = random_pairs(1000, 0xD1B54A32D192ED03, 1_000_000);
        let mut expected = items.clone();
        expected.sort();
        // 1000 runs of one record, merged 2, 3 and 5 at a time.
        for fan_in in [2usize, 3, 5] {
            assert_eq!(sort_with(&items, 1, fan_in, &dir), expected, "fan-in {fan_in}");
            assert_eq!(dir.files(), 0, "temporary files are removed after the output is read");
        }
    }

    #[test]
    fn input_that_fits_in_one_run_never_touches_the_disk() {
        let dir = Dir::new();
        let items = random_pairs(100, 7, 1000);
        let mut sorter = Sorter::<Pair>::with_run_len(100, 8, dir.0.clone());
        for &item in &items {
            sorter.push(item).unwrap();
        }
        assert_eq!(dir.files(), 0);
        let sorted = sorter.finish().unwrap();
        assert_eq!(dir.files(), 0);
        assert_eq!(sorted.count(), 100);
    }

    #[test]
    fn temporary_files_go_away_even_if_the_output_is_not_read_to_the_end() {
        let dir = Dir::new();
        let items = random_pairs(300, 99, 20);
        let mut sorter = Sorter::<Pair>::with_run_len(10, 4, dir.0.clone());
        for &item in &items {
            sorter.push(item).unwrap();
        }
        assert!(dir.files() > 0, "the runs are on disk while sorting");
        let mut sorted = sorter.finish().unwrap();
        assert!(sorted.next().is_some());
        drop(sorted);
        assert_eq!(dir.files(), 0);
        // And a sorter dropped without finishing leaves nothing behind either.
        let mut abandoned = Sorter::<Pair>::with_run_len(10, 4, dir.0.clone());
        for &item in &items {
            abandoned.push(item).unwrap();
        }
        assert!(dir.files() > 0);
        drop(abandoned);
        assert_eq!(dir.files(), 0);
    }

    #[test]
    fn the_size_of_a_run_comes_from_the_memory_budget() {
        let dir = Dir::new();
        // 1600 bytes of 16-byte records: runs of 100.
        let mut sorter = Sorter::<Pair>::new(1600, dir.0.clone());
        for &item in &random_pairs(1000, 5, 100) {
            sorter.push(item).unwrap();
        }
        assert_eq!(dir.files(), 9, "ten runs of 100, the last still in memory");
        // A budget too small for one record still works, with a run per record.
        let mut tiny = Sorter::<Pair>::new(1, dir.0.clone());
        for &item in &random_pairs(5, 5, 100) {
            tiny.push(item).unwrap();
        }
        assert_eq!(tiny.finish().unwrap().count(), 5);
    }

    #[test]
    fn a_missing_directory_is_an_error_when_a_run_has_to_be_written() {
        let dir = Dir::new();
        let gone = dir.0.join("not-there");
        let mut sorter = Sorter::<Pair>::with_run_len(2, 4, gone);
        let mut result = Ok(());
        for item in random_pairs(10, 1, 10) {
            result = sorter.push(item);
            if result.is_err() {
                break;
            }
        }
        assert!(result.is_err(), "writing the first run into a missing directory fails");
    }

    #[test]
    fn records_that_compare_equal_are_all_kept() {
        let dir = Dir::new();
        let items: Vec<Pair> = (0..200).map(|i| Pair { key: i % 3, payload: 7 }).collect();
        let sorted = sort_with(&items, 7, 3, &dir);
        assert_eq!(sorted.len(), 200);
        assert!(sorted.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn merging_down_leaves_no_more_runs_than_the_fan_in_allows() {
        let dir = Dir::new();
        for fan_in in [2usize, 3, 7, 64] {
            let mut sorter = Sorter::<Pair>::with_run_len(1, fan_in, dir.0.clone());
            for &item in &random_pairs(300, 3, 1000) {
                sorter.push(item).unwrap();
            }
            assert!(sorter.runs.len() > fan_in, "the case needs more runs than the fan-in");
            sorter.merge_down().unwrap();
            assert!(
                sorter.runs.len() <= fan_in,
                "fan-in {fan_in}: {} runs left",
                sorter.runs.len()
            );
            assert_eq!(
                sorter.runs.iter().map(|r| r.records).sum::<usize>() + sorter.batch.len(),
                300
            );
        }
        assert_eq!(dir.files(), 0);
    }
}

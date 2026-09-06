//! Automatic streaming vs. in-memory diffing for JSON files.
//!
//! [`diff_files`] inspects the inputs and picks a strategy: large top-level JSON
//! arrays are parsed and compared element by element without ever materialising
//! the whole document, everything else is loaded normally. Both paths emit
//! identical operations; the choice only affects peak memory.

use crate::{diff, streaming::StreamingArrayDiffer, Delta, DriftError};
use serde::de::{DeserializeSeed, Deserializer, SeqAccess, Visitor};
use serde_json::Value;
use std::{
    fmt,
    fs::File,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    sync::mpsc::{sync_channel, Receiver, SyncSender},
};

/// Inputs above this size are candidates for streaming.
const STREAM_THRESHOLD_BYTES: u64 = 32 * 1024 * 1024;
/// Elements compared per chunk, and channel depth. Bounds peak memory.
const STREAM_CHUNK: usize = 1024;

/// Pushes each element of a JSON array into a channel as it is parsed.
struct ArrayStreamer {
    tx: SyncSender<Result<Value, String>>,
}

impl<'de> Visitor<'de> for ArrayStreamer {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a JSON array")
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<(), A::Error>
    where
        A: SeqAccess<'de>,
    {
        while let Some(value) = seq.next_element::<Value>()? {
            // Receiver hung up; stop parsing rather than blocking forever.
            if self.tx.send(Ok(value)).is_err() {
                break;
            }
        }
        Ok(())
    }
}

impl<'de> DeserializeSeed<'de> for ArrayStreamer {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<(), D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_seq(self)
    }
}

/// Parses `path` on a worker thread, yielding array elements one at a time.
///
/// The bounded channel applies backpressure, so memory stays at O(STREAM_CHUNK)
/// regardless of file size.
fn stream_array(path: &Path) -> Receiver<Result<Value, String>> {
    let (tx, rx) = sync_channel(STREAM_CHUNK);
    let path: PathBuf = path.to_path_buf();
    std::thread::spawn(move || {
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) => {
                let _ = tx.send(Err(error.to_string()));
                return;
            }
        };
        let mut deserializer = serde_json::Deserializer::from_reader(BufReader::new(file));
        let streamer = ArrayStreamer { tx: tx.clone() };
        if let Err(error) = streamer.deserialize(&mut deserializer) {
            let _ = tx.send(Err(error.to_string()));
        }
    });
    rx
}

/// Pulls up to `limit` elements, propagating parse errors.
fn take_chunk(
    source: &mut dyn Iterator<Item = Result<Value, String>>,
    limit: usize,
    buffer: &mut Vec<Value>,
) -> Result<(), DriftError> {
    buffer.clear();
    for _ in 0..limit {
        match source.next() {
            Some(Ok(value)) => buffer.push(value),
            Some(Err(error)) => return Err(DriftError::Parse(error)),
            None => break,
        }
    }
    Ok(())
}

/// Walks two element streams in lockstep, emitting the same operations as [`diff`].
fn diff_streams(
    old_source: &mut dyn Iterator<Item = Result<Value, String>>,
    new_source: &mut dyn Iterator<Item = Result<Value, String>>,
) -> Result<Vec<Delta>, DriftError> {
    let mut differ = StreamingArrayDiffer::new(String::new());
    let mut old_chunk = Vec::with_capacity(STREAM_CHUNK);
    let mut new_chunk = Vec::with_capacity(STREAM_CHUNK);
    let mut index = 0usize;

    loop {
        take_chunk(old_source, STREAM_CHUNK, &mut old_chunk)?;
        take_chunk(new_source, STREAM_CHUNK, &mut new_chunk)?;

        let overlap = old_chunk.len().min(new_chunk.len());
        if overlap > 0 {
            differ.diff_chunk(&old_chunk[..overlap], &new_chunk[..overlap], index);
            index += overlap;
        }

        if old_chunk.len() > overlap {
            // New input ended first: everything left in old is a removal.
            let mut old_total = index + (old_chunk.len() - overlap);
            loop {
                take_chunk(old_source, STREAM_CHUNK, &mut old_chunk)?;
                old_total += old_chunk.len();
                if old_chunk.len() < STREAM_CHUNK {
                    break;
                }
            }
            differ.set_length_change(old_total, index);
            return Ok(differ.finalize());
        }

        if new_chunk.len() > overlap {
            // Old input ended first: everything left in new is an append.
            let old_total = index;
            let mut new_total = index;
            differ.push_additions(new_total, &new_chunk[overlap..]);
            new_total += new_chunk.len() - overlap;
            loop {
                take_chunk(new_source, STREAM_CHUNK, &mut new_chunk)?;
                if new_chunk.is_empty() {
                    break;
                }
                differ.push_additions(new_total, &new_chunk);
                new_total += new_chunk.len();
                if new_chunk.len() < STREAM_CHUNK {
                    break;
                }
            }
            differ.set_length_change(old_total, new_total);
            return Ok(differ.finalize());
        }

        if old_chunk.len() < STREAM_CHUNK {
            break;
        }
    }

    differ.set_length_change(index, index);
    Ok(differ.finalize())
}

/// Returns true when the first non-whitespace byte is `[`.
fn starts_with_array(path: &Path) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    let mut reader = BufReader::new(file);
    loop {
        let Ok(buffer) = reader.fill_buf() else {
            return false;
        };
        if buffer.is_empty() {
            return false;
        }
        if let Some(byte) = buffer.iter().find(|b| !b.is_ascii_whitespace()) {
            return *byte == b'[';
        }
        let consumed = buffer.len();
        reader.consume(consumed);
    }
}

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

fn load_json(path: &Path) -> Result<Value, DriftError> {
    let mut text = String::new();
    File::open(path)
        .and_then(|mut file| file.read_to_string(&mut text))
        .map_err(|error| DriftError::Io(error.to_string()))?;
    serde_json::from_str(&text).map_err(|error| DriftError::Parse(error.to_string()))
}

fn should_stream(old: &Path, new: &Path, threshold: u64) -> bool {
    (file_len(old) > threshold || file_len(new) > threshold)
        && starts_with_array(old)
        && starts_with_array(new)
}

/// Diffs two JSON files, streaming automatically when it is worthwhile.
///
/// Streaming is used when either file exceeds an internal size threshold and
/// both hold a top-level array. The result is always identical to
/// [`diff`] on the fully parsed documents.
pub fn diff_files(old: &Path, new: &Path) -> Result<Vec<Delta>, DriftError> {
    diff_files_with(old, new, STREAM_THRESHOLD_BYTES)
}

fn diff_files_with(old: &Path, new: &Path, threshold: u64) -> Result<Vec<Delta>, DriftError> {
    if should_stream(old, new, threshold) {
        let old_rx = stream_array(old);
        let new_rx = stream_array(new);
        return diff_streams(&mut old_rx.into_iter(), &mut new_rx.into_iter());
    }
    let old_value = load_json(old)?;
    let new_value = load_json(new)?;
    Ok(diff(&new_value, &old_value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn write_temp(value: &Value) -> PathBuf {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("drift_stream_{}_{}.json", std::process::id(), id));
        let mut file = File::create(&path).unwrap();
        file.write_all(serde_json::to_string(value).unwrap().as_bytes()).unwrap();
        path
    }

    /// Asserts the streaming path and the in-memory path agree.
    fn assert_paths_agree(old: Value, new: Value) {
        let old_path = write_temp(&old);
        let new_path = write_temp(&new);

        let expected = diff(&new, &old);
        // threshold 0 forces streaming, u64::MAX forces the in-memory path
        let streamed = diff_files_with(&old_path, &new_path, 0).unwrap();
        let loaded = diff_files_with(&old_path, &new_path, u64::MAX).unwrap();

        std::fs::remove_file(&old_path).ok();
        std::fs::remove_file(&new_path).ok();

        assert_eq!(streamed, expected, "streaming path diverged");
        assert_eq!(loaded, expected, "in-memory path diverged");
    }

    #[test]
    fn streams_scalar_changes() {
        assert_paths_agree(json!([1, 2, 3]), json!([1, 20, 3]));
    }

    #[test]
    fn streams_nested_field_changes() {
        assert_paths_agree(
            json!([{"id": 1, "name": "a"}, {"id": 2, "name": "b"}]),
            json!([{"id": 1, "name": "a"}, {"id": 2, "name": "CHANGED"}]),
        );
    }

    #[test]
    fn streams_appends() {
        assert_paths_agree(json!([1, 2]), json!([1, 2, 3, 4]));
    }

    #[test]
    fn streams_truncations() {
        assert_paths_agree(json!([1, 2, 3, 4]), json!([1, 2]));
    }

    #[test]
    fn streams_empty_arrays() {
        assert_paths_agree(json!([]), json!([]));
        assert_paths_agree(json!([]), json!([1, 2]));
        assert_paths_agree(json!([1, 2]), json!([]));
    }

    #[test]
    fn streams_across_chunk_boundaries() {
        let old: Vec<Value> = (0..STREAM_CHUNK * 2 + 7).map(|i| json!({"i": i})).collect();
        let mut new = old.clone();
        new[0] = json!({"i": "first"});
        new[STREAM_CHUNK] = json!({"i": "boundary"});
        new[STREAM_CHUNK * 2 + 6] = json!({"i": "last"});
        new.push(json!({"i": "appended"}));
        assert_paths_agree(Value::Array(old), Value::Array(new));
    }

    #[test]
    fn streams_truncation_larger_than_one_chunk() {
        let old: Vec<Value> = (0..STREAM_CHUNK * 3).map(|i| json!(i)).collect();
        let new: Vec<Value> = (0..STREAM_CHUNK / 2).map(|i| json!(i)).collect();
        assert_paths_agree(Value::Array(old), Value::Array(new));
    }

    #[test]
    fn streams_append_larger_than_one_chunk() {
        let old: Vec<Value> = (0..STREAM_CHUNK / 2).map(|i| json!(i)).collect();
        let new: Vec<Value> = (0..STREAM_CHUNK * 3).map(|i| json!(i)).collect();
        assert_paths_agree(Value::Array(old), Value::Array(new));
    }

    #[test]
    fn non_array_documents_use_in_memory_path() {
        let old = json!({"a": 1});
        let new = json!({"a": 2});
        let old_path = write_temp(&old);
        let new_path = write_temp(&new);

        // Even with the threshold at 0, an object cannot stream.
        let result = diff_files_with(&old_path, &new_path, 0).unwrap();

        std::fs::remove_file(&old_path).ok();
        std::fs::remove_file(&new_path).ok();

        assert_eq!(result, diff(&new, &old));
    }

    #[test]
    fn reports_parse_errors() {
        let path =
            std::env::temp_dir().join(format!("drift_stream_bad_{}.json", std::process::id()));
        std::fs::write(&path, b"[1, 2, ").unwrap();
        let result = diff_files_with(&path, &path, 0);
        std::fs::remove_file(&path).ok();
        assert!(matches!(result, Err(DriftError::Parse(_))));
    }
}

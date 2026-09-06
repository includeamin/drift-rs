//! Automatic streaming vs. in-memory diffing for JSON files.
//!
//! [`diff_files`] inspects the inputs and picks a strategy: large top-level JSON
//! arrays are parsed and compared element by element without ever materialising
//! the whole document, everything else is loaded normally. Both paths emit
//! identical operations; the choice only affects peak memory.

use crate::diff::diff_inner;
use crate::scan::{scan_object_members, Kind, Member};
use crate::{diff, join_pointer, streaming::StreamingArrayDiffer, Delta, DriftError};
use serde::de::{DeserializeSeed, Deserializer, SeqAccess, Visitor};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fmt,
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::mpsc::{sync_channel, Receiver, SyncSender},
};

/// Inputs above this size are candidates for streaming.
const STREAM_THRESHOLD_BYTES: u64 = 32 * 1024 * 1024;
/// Object members at least this large are streamed rather than parsed whole.
const MEMBER_STREAM_THRESHOLD_BYTES: u64 = 1024 * 1024;
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

/// Parses the array in `path` between `start` and `end` on a worker thread,
/// yielding elements one at a time.
///
/// The bounded channel applies backpressure, so memory stays at O(STREAM_CHUNK)
/// regardless of file size.
fn stream_array_range(path: &Path, start: u64, end: u64) -> Receiver<Result<Value, String>> {
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
        let mut file = file;
        if start > 0 {
            if let Err(error) = file.seek(SeekFrom::Start(start)) {
                let _ = tx.send(Err(error.to_string()));
                return;
            }
        }
        let bounded = BufReader::new(file.take(end - start));
        let mut deserializer = serde_json::Deserializer::from_reader(bounded);
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
    path: &str,
) -> Result<Vec<Delta>, DriftError> {
    let mut differ = StreamingArrayDiffer::new(path.to_string());
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

/// First non-whitespace byte, used to classify the document root.
fn first_byte(path: &Path) -> Option<u8> {
    let file = File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    loop {
        let buffer = reader.fill_buf().ok()?;
        if buffer.is_empty() {
            return None;
        }
        if let Some(byte) = buffer.iter().find(|b| !b.is_ascii_whitespace()) {
            return Some(*byte);
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

/// Parses just one member's value out of the file.
fn read_member(path: &Path, member: &Member) -> Result<Value, DriftError> {
    let mut file = File::open(path).map_err(|error| DriftError::Io(error.to_string()))?;
    file.seek(SeekFrom::Start(member.start)).map_err(|error| DriftError::Io(error.to_string()))?;
    let bounded = BufReader::new(file.take(member.len()));
    serde_json::from_reader(bounded).map_err(|error| DriftError::Parse(error.to_string()))
}

fn index_members(
    path: &Path,
    start: u64,
    end: u64,
) -> Result<BTreeMap<String, Member>, DriftError> {
    let mut file = File::open(path).map_err(|error| DriftError::Io(error.to_string()))?;
    if start > 0 {
        file.seek(SeekFrom::Start(start)).map_err(|error| DriftError::Io(error.to_string()))?;
    }
    let members = scan_object_members(BufReader::new(file.take(end - start)))?;
    // Scanner offsets are relative to the range, so rebase onto the file.
    // Collecting into a map makes later duplicate keys win, as serde_json does.
    Ok(members
        .into_iter()
        .map(|mut member| {
            member.start += start;
            member.end += start;
            (member.key.clone(), member)
        })
        .collect())
}

/// Diffs two JSON objects member by member, streaming large arrays and
/// recursing into large nested objects.
///
/// Emits operations in the same order as [`diff`]: removals, then additions,
/// then per-key recursion, each in sorted key order.
fn diff_objects(
    old: &Path,
    new: &Path,
    old_range: (u64, u64),
    new_range: (u64, u64),
    path: &str,
    member_threshold: u64,
) -> Result<Vec<Delta>, DriftError> {
    let old_members = index_members(old, old_range.0, old_range.1)?;
    let new_members = index_members(new, new_range.0, new_range.1)?;
    let mut operations = Vec::new();

    for key in old_members.keys() {
        if !new_members.contains_key(key) {
            operations.push(Delta::new(crate::Operation::Remove, join_pointer(path, key)));
        }
    }

    for (key, member) in &new_members {
        if !old_members.contains_key(key) {
            operations.push(Delta::with_value(
                crate::Operation::Add,
                join_pointer(path, key),
                read_member(new, member)?,
            ));
        }
    }

    for (key, old_member) in &old_members {
        let Some(new_member) = new_members.get(key) else {
            continue;
        };
        let item_path = join_pointer(path, key);
        let large = old_member.len() > member_threshold || new_member.len() > member_threshold;

        match (old_member.kind, new_member.kind) {
            (Kind::Array, Kind::Array) if large => {
                let old_rx = stream_array_range(old, old_member.start, old_member.end);
                let new_rx = stream_array_range(new, new_member.start, new_member.end);
                operations.extend(diff_streams(
                    &mut old_rx.into_iter(),
                    &mut new_rx.into_iter(),
                    &item_path,
                )?);
            }
            (Kind::Object, Kind::Object) if large => {
                operations.extend(diff_objects(
                    old,
                    new,
                    (old_member.start, old_member.end),
                    (new_member.start, new_member.end),
                    &item_path,
                    member_threshold,
                )?);
            }
            _ => {
                let old_value = read_member(old, old_member)?;
                let new_value = read_member(new, new_member)?;
                diff_inner(&new_value, &old_value, &item_path, &mut operations);
            }
        }
    }

    Ok(operations)
}

/// Diffs two JSON files, streaming automatically when it is worthwhile.
///
/// Streaming is used when either file exceeds an internal size threshold and
/// both roots are arrays, or both are objects whose large array members can be
/// streamed individually. The result is always identical to [`diff`] on the
/// fully parsed documents.
pub fn diff_files(old: &Path, new: &Path) -> Result<Vec<Delta>, DriftError> {
    diff_files_with(old, new, STREAM_THRESHOLD_BYTES)
}

fn diff_files_with(old: &Path, new: &Path, threshold: u64) -> Result<Vec<Delta>, DriftError> {
    diff_files_tuned(old, new, threshold, MEMBER_STREAM_THRESHOLD_BYTES)
}

fn diff_files_tuned(
    old: &Path,
    new: &Path,
    threshold: u64,
    member_threshold: u64,
) -> Result<Vec<Delta>, DriftError> {
    let large = file_len(old) > threshold || file_len(new) > threshold;
    if large {
        match (first_byte(old), first_byte(new)) {
            (Some(b'['), Some(b'[')) => {
                let old_rx = stream_array_range(old, 0, file_len(old));
                let new_rx = stream_array_range(new, 0, file_len(new));
                return diff_streams(&mut old_rx.into_iter(), &mut new_rx.into_iter(), "");
            }
            (Some(b'{'), Some(b'{')) => {
                return diff_objects(
                    old,
                    new,
                    (0, file_len(old)),
                    (0, file_len(new)),
                    "",
                    member_threshold,
                )
            }
            _ => {}
        }
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
        // threshold 0 forces streaming, u64::MAX forces the in-memory path.
        // member_threshold 0 also forces array members to stream.
        let streamed = diff_files_tuned(&old_path, &new_path, 0, 0).unwrap();
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
    fn scalar_roots_use_in_memory_path() {
        let old = json!("just a string");
        let new = json!("another string");
        let old_path = write_temp(&old);
        let new_path = write_temp(&new);

        // Neither root is an array or object, so streaming cannot apply.
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

    // ===== Top-level object roots =====

    #[test]
    fn streams_array_wrapped_in_object() {
        let old = json!({"generated_at": "t0", "users": [{"id": 1, "name": "a"}, {"id": 2, "name": "b"}]});
        let new = json!({"generated_at": "t1", "users": [{"id": 1, "name": "a"}, {"id": 2, "name": "CHANGED"}]});
        assert_paths_agree(old, new);
    }

    #[test]
    fn wrapped_array_produces_nested_paths() {
        let old = json!({"users": [{"name": "a"}, {"name": "b"}]});
        let new = json!({"users": [{"name": "a"}, {"name": "z"}]});
        let old_path = write_temp(&old);
        let new_path = write_temp(&new);

        let operations = diff_files_tuned(&old_path, &new_path, 0, 0).unwrap();

        std::fs::remove_file(&old_path).ok();
        std::fs::remove_file(&new_path).ok();

        assert_eq!(operations.len(), 1);
        assert_eq!(operations[0].path, "/users/1/name");
    }

    #[test]
    fn object_key_added_and_removed() {
        assert_paths_agree(json!({"a": 1, "b": 2}), json!({"b": 2, "c": 3}));
    }

    #[test]
    fn object_nested_value_changes() {
        assert_paths_agree(
            json!({"cfg": {"x": 1, "y": {"z": 2}}}),
            json!({"cfg": {"x": 1, "y": {"z": 3}}}),
        );
    }

    #[test]
    fn object_member_type_changes() {
        assert_paths_agree(json!({"a": [1, 2]}), json!({"a": {"b": 1}}));
        assert_paths_agree(json!({"a": {"b": 1}}), json!({"a": [1, 2]}));
        assert_paths_agree(json!({"a": [1, 2]}), json!({"a": "scalar"}));
    }

    #[test]
    fn object_array_members_change_length() {
        assert_paths_agree(json!({"a": [1, 2, 3]}), json!({"a": [1, 2]}));
        assert_paths_agree(json!({"a": [1, 2]}), json!({"a": [1, 2, 3, 4]}));
        assert_paths_agree(json!({"a": []}), json!({"a": [1]}));
        assert_paths_agree(json!({"a": [1]}), json!({"a": []}));
    }

    #[test]
    fn object_keys_needing_pointer_escaping() {
        assert_paths_agree(json!({"a/b": 1, "c~d": 2}), json!({"a/b": 9, "c~d": 2}));
    }

    #[test]
    fn object_keys_are_order_independent() {
        // Same content, different serialization order, must produce no operations.
        let old_path = write_temp(&json!({"a": 1, "b": 2}));
        let new_path =
            std::env::temp_dir().join(format!("drift_stream_ord_{}.json", std::process::id()));
        std::fs::write(&new_path, br#"{"b":2,"a":1}"#).unwrap();

        let operations = diff_files_tuned(&old_path, &new_path, 0, 0).unwrap();

        std::fs::remove_file(&old_path).ok();
        std::fs::remove_file(&new_path).ok();

        assert!(operations.is_empty(), "expected no changes, got {operations:?}");
    }

    #[test]
    fn empty_and_unchanged_objects() {
        assert_paths_agree(json!({}), json!({}));
        assert_paths_agree(json!({}), json!({"a": 1}));
        assert_paths_agree(json!({"a": 1}), json!({}));
        assert_paths_agree(json!({"a": 1}), json!({"a": 1}));
    }

    #[test]
    fn object_members_spanning_many_chunks() {
        let big_old: Vec<Value> = (0..STREAM_CHUNK * 2 + 5).map(|i| json!({"i": i})).collect();
        let mut big_new = big_old.clone();
        big_new[STREAM_CHUNK] = json!({"i": "boundary"});
        big_new.push(json!({"i": "appended"}));

        assert_paths_agree(
            json!({"meta": {"v": 1}, "items": big_old}),
            json!({"meta": {"v": 2}, "items": big_new}),
        );
    }

    #[test]
    fn mixed_roots_fall_back_to_loading() {
        // One array root, one object root: cannot stream, must still be correct.
        assert_paths_agree(json!([1, 2]), json!({"a": 1}));
        assert_paths_agree(json!({"a": 1}), json!([1, 2]));
    }

    #[test]
    fn reports_parse_errors_for_object_roots() {
        let path =
            std::env::temp_dir().join(format!("drift_stream_badobj_{}.json", std::process::id()));
        std::fs::write(&path, br#"{"a": [1, 2"#).unwrap();
        let result = diff_files_tuned(&path, &path, 0, 0);
        std::fs::remove_file(&path).ok();
        assert!(result.is_err(), "expected an error, got {result:?}");
    }

    // ===== Nested objects =====

    #[test]
    fn recurses_into_nested_objects() {
        let old = json!({"data": {"users": [{"name": "a"}, {"name": "b"}]}});
        let new = json!({"data": {"users": [{"name": "a"}, {"name": "z"}]}});
        let old_path = write_temp(&old);
        let new_path = write_temp(&new);

        let operations = diff_files_tuned(&old_path, &new_path, 0, 0).unwrap();

        std::fs::remove_file(&old_path).ok();
        std::fs::remove_file(&new_path).ok();

        assert_eq!(operations.len(), 1);
        assert_eq!(operations[0].path, "/data/users/1/name");
    }

    #[test]
    fn deeply_nested_objects_match_diff() {
        assert_paths_agree(
            json!({"a": {"b": {"c": {"d": {"e": [1, 2, 3]}}}}}),
            json!({"a": {"b": {"c": {"d": {"e": [1, 9, 3, 4]}}}}}),
        );
    }

    #[test]
    fn nested_object_keys_added_and_removed() {
        assert_paths_agree(
            json!({"cfg": {"keep": 1, "drop": 2, "nest": {"x": 1}}}),
            json!({"cfg": {"keep": 1, "add": 3, "nest": {"y": 2}}}),
        );
    }

    #[test]
    fn nested_escaped_keys_build_correct_pointers() {
        assert_paths_agree(json!({"a/b": {"c~d": {"e": 1}}}), json!({"a/b": {"c~d": {"e": 2}}}));
    }

    #[test]
    fn nested_mixed_kinds_fall_back_per_member() {
        assert_paths_agree(
            json!({"a": {"b": [1, 2]}, "c": {"d": 1}}),
            json!({"a": {"b": {"x": 1}}, "c": {"d": [1]}}),
        );
    }

    #[test]
    fn nested_arrays_spanning_chunks() {
        let big: Vec<Value> = (0..STREAM_CHUNK + 3).map(|i| json!({"i": i})).collect();
        let mut changed = big.clone();
        changed[STREAM_CHUNK] = json!({"i": "boundary"});

        assert_paths_agree(
            json!({"outer": {"inner": {"items": big}}}),
            json!({"outer": {"inner": {"items": changed}}}),
        );
    }

    #[test]
    fn nested_empty_containers() {
        assert_paths_agree(json!({"a": {"b": {}}}), json!({"a": {"b": {"c": 1}}}));
        assert_paths_agree(json!({"a": {"b": {"c": 1}}}), json!({"a": {"b": {}}}));
        assert_paths_agree(json!({"a": {}}), json!({"a": {}}));
    }
}

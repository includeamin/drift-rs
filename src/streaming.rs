//! Streaming and incremental processing for large documents.
//!
//! This module provides APIs for processing large JSON documents without
//! loading everything into memory at once. Useful for:
//! - Large arrays (millions of elements)
//! - Deep nested structures
//! - Incremental updates to huge files
//!
//! Streaming produces exactly the same operations, in the same order, as
//! [`crate::diff`] on the same data. Chunk size affects memory use, never output.
//!
//! # Example: Streaming Array Diff
//! ```ignore
//! use drift::streaming::StreamingArrayDiffer;
//!
//! let mut differ = StreamingArrayDiffer::new(String::new());
//! for i in 0..1_000 {
//!     let old_chunk = load_array_chunk(i, 1000);
//!     let new_chunk = load_array_chunk(i, 1000);
//!     differ.diff_chunk(&old_chunk, &new_chunk, i * 1000);
//! }
//! differ.set_length_change(old_len, new_len);
//! let all_operations = differ.finalize();
//! ```

use crate::diff::diff_inner;
use crate::{join_pointer, Delta, Operation};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Processes large arrays incrementally without loading entire array into memory.
///
/// Produces byte-for-byte the same operations as [`crate::diff`] on the same data.
///
/// # Performance Benefits
/// - **Memory**: O(chunk_size) instead of O(array_size)
/// - **Time**: Still O(array_size) but can be parallelized per chunk
/// - **Streaming**: Can process data as it arrives from disk/network
pub struct StreamingArrayDiffer {
    /// Path to the array being diffed
    path: String,
    /// Operations from the overlapping region, keyed by index to preserve order
    changes: BTreeMap<usize, Vec<Delta>>,
    /// Trailing removals when the array shrank
    removals: Vec<Delta>,
    /// Trailing additions when the array grew
    additions: Vec<Delta>,
}

impl StreamingArrayDiffer {
    /// Creates a new streaming array differ rooted at `path` (`""` for document root).
    pub fn new(path: String) -> Self {
        StreamingArrayDiffer {
            path,
            changes: BTreeMap::new(),
            removals: Vec::new(),
            additions: Vec::new(),
        }
    }

    /// Processes a chunk of array elements.
    ///
    /// Only the overlapping prefix of the two slices is compared; trailing
    /// elements are handled by [`Self::set_length_change`] and
    /// [`Self::push_additions`].
    ///
    /// # Arguments
    /// * `old_chunk` - Slice of old array elements
    /// * `new_chunk` - Slice of new array elements
    /// * `start_index` - Starting index in the overall array
    pub fn diff_chunk(&mut self, old_chunk: &[Value], new_chunk: &[Value], start_index: usize) {
        for (offset, (old_val, new_val)) in old_chunk.iter().zip(new_chunk.iter()).enumerate() {
            let index = start_index + offset;
            let item_path = join_pointer(&self.path, &index.to_string());
            let mut ops = Vec::new();
            diff_inner(new_val, old_val, &item_path, &mut ops);
            if !ops.is_empty() {
                self.changes.insert(index, ops);
            }
        }
    }

    /// Records removals for elements dropped from the end of the array.
    pub fn set_length_change(&mut self, old_len: usize, new_len: usize) {
        self.removals.clear();
        for idx in (new_len..old_len).rev() {
            self.removals
                .push(Delta::new(Operation::Remove, join_pointer(&self.path, &idx.to_string())));
        }
    }

    /// Records additions for elements appended past the end of the old array.
    ///
    /// `start_index` is the old array length; `items` are the new trailing elements.
    pub fn push_additions(&mut self, start_index: usize, items: &[Value]) {
        for (offset, item) in items.iter().enumerate() {
            self.additions.push(Delta::with_value(
                Operation::Add,
                join_pointer(&self.path, &(start_index + offset).to_string()),
                item.clone(),
            ));
        }
    }

    /// Returns accumulated operations in the same order as [`crate::diff`].
    pub fn finalize(self) -> Vec<Delta> {
        let mut output: Vec<Delta> = self.changes.into_values().flatten().collect();
        output.extend(self.removals);
        output.extend(self.additions);
        output
    }
}

/// Processes objects with many keys incrementally.
///
/// Produces byte-for-byte the same operations as [`crate::diff`] on the same data.
///
/// Useful for objects with thousands of fields.
pub struct StreamingObjectDiffer {
    path: String,
    removals: BTreeSet<String>,
    additions: BTreeMap<String, Value>,
    changes: BTreeMap<String, Vec<Delta>>,
}

impl StreamingObjectDiffer {
    /// Creates a new streaming object differ rooted at `path` (`""` for document root).
    pub fn new(path: String) -> Self {
        StreamingObjectDiffer {
            path,
            removals: BTreeSet::new(),
            additions: BTreeMap::new(),
            changes: BTreeMap::new(),
        }
    }

    /// Processes a batch of object key-value pairs.
    ///
    /// Batches must cover disjoint key sets.
    ///
    /// # Arguments
    /// * `old_map` - Slice of old key-value pairs
    /// * `new_map` - Slice of new key-value pairs
    pub fn diff_batch(&mut self, old_map: &[(String, Value)], new_map: &[(String, Value)]) {
        let old_kv: BTreeMap<&String, &Value> = old_map.iter().map(|(k, v)| (k, v)).collect();
        let new_kv: BTreeMap<&String, &Value> = new_map.iter().map(|(k, v)| (k, v)).collect();

        for key in old_kv.keys() {
            if !new_kv.contains_key(*key) {
                self.removals.insert((*key).clone());
            }
        }

        for (key, new_val) in &new_kv {
            match old_kv.get(key) {
                Some(old_val) => {
                    let item_path = join_pointer(&self.path, key);
                    let mut ops = Vec::new();
                    diff_inner(new_val, old_val, &item_path, &mut ops);
                    if !ops.is_empty() {
                        self.changes.insert((*key).clone(), ops);
                    }
                }
                None => {
                    self.additions.insert((*key).clone(), (*new_val).clone());
                }
            }
        }
    }

    /// Returns accumulated operations in the same order as [`crate::diff`].
    pub fn finalize(self) -> Vec<Delta> {
        let mut output = Vec::new();
        for key in &self.removals {
            output.push(Delta::new(Operation::Remove, join_pointer(&self.path, key)));
        }
        for (key, value) in &self.additions {
            output.push(Delta::with_value(
                Operation::Add,
                join_pointer(&self.path, key),
                value.clone(),
            ));
        }
        output.extend(self.changes.into_values().flatten());
        output
    }
}

/// Batches multiple patch operations for efficiency.
///
/// Instead of applying patches one by one (which traverses the tree for each),
/// group related operations together.
///
/// # Performance
/// - Reduces tree traversals from O(ops) to O(ops/batch_size)
/// - Applies operations in batches, reducing memory allocations
pub struct PatchBatcher {
    batch_size: usize,
    current_batch: Vec<Delta>,
}

impl PatchBatcher {
    /// Creates a new patch batcher with specified batch size.
    pub fn new(batch_size: usize) -> Self {
        PatchBatcher { batch_size, current_batch: Vec::new() }
    }

    /// Adds an operation to the batch.
    ///
    /// Returns the batch if it's full, otherwise returns empty vector.
    pub fn push(&mut self, op: Delta) -> Vec<Delta> {
        self.current_batch.push(op);

        if self.current_batch.len() >= self.batch_size {
            std::mem::take(&mut self.current_batch)
        } else {
            Vec::new()
        }
    }

    /// Flushes remaining operations in the batch.
    pub fn flush(&mut self) -> Vec<Delta> {
        std::mem::take(&mut self.current_batch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Runs the streaming array differ over two whole arrays in fixed-size chunks.
    fn stream_arrays(old: &[Value], new: &[Value], chunk_size: usize) -> Vec<Delta> {
        let mut differ = StreamingArrayDiffer::new(String::new());
        let overlap = old.len().min(new.len());
        let mut index = 0;
        while index < overlap {
            let end = (index + chunk_size).min(overlap);
            differ.diff_chunk(&old[index..end], &new[index..end], index);
            index = end;
        }
        differ.set_length_change(old.len(), new.len());
        if new.len() > old.len() {
            differ.push_additions(old.len(), &new[old.len()..]);
        }
        differ.finalize()
    }

    fn assert_matches_diff(old: Value, new: Value, chunk_size: usize) {
        let expected = crate::diff(&new, &old);
        let actual = stream_arrays(old.as_array().unwrap(), new.as_array().unwrap(), chunk_size);
        assert_eq!(actual, expected);
    }

    #[test]
    fn streaming_array_matches_diff_on_scalar_change() {
        assert_matches_diff(json!([1, 2, 3]), json!([1, 20, 3]), 2);
    }

    #[test]
    fn streaming_array_recurses_into_nested_objects() {
        let old = json!([{"id": 1, "name": "a"}, {"id": 2, "name": "b"}]);
        let new = json!([{"id": 1, "name": "a"}, {"id": 2, "name": "CHANGED"}]);

        let ops = stream_arrays(old.as_array().unwrap(), new.as_array().unwrap(), 1);

        assert_eq!(ops, crate::diff(&new, &old));
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].path, "/1/name");
    }

    #[test]
    fn streaming_array_matches_diff_on_append() {
        assert_matches_diff(json!([1, 2]), json!([1, 2, 3, 4]), 2);
    }

    #[test]
    fn streaming_array_matches_diff_on_truncate() {
        assert_matches_diff(json!([1, 2, 3, 4]), json!([1, 2]), 2);
    }

    #[test]
    fn streaming_array_output_is_chunk_size_independent() {
        let old = json!([{"a": 1}, {"a": 2}, {"a": 3}, {"a": 4}, {"a": 5}]);
        let new = json!([{"a": 1}, {"a": 9}, {"a": 3}, {"a": 8}, {"a": 5}, {"a": 6}]);
        let expected = crate::diff(&new, &old);

        for chunk_size in 1..=6 {
            let actual =
                stream_arrays(old.as_array().unwrap(), new.as_array().unwrap(), chunk_size);
            assert_eq!(actual, expected, "chunk_size {chunk_size} diverged");
        }
    }

    #[test]
    fn streaming_object_matches_diff() {
        let old = json!({"name": "old", "value": 1, "nested": {"x": 1}});
        let new = json!({"name": "new", "other": 2, "nested": {"x": 2}});

        let old_pairs: Vec<(String, Value)> =
            old.as_object().unwrap().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let new_pairs: Vec<(String, Value)> =
            new.as_object().unwrap().iter().map(|(k, v)| (k.clone(), v.clone())).collect();

        let mut differ = StreamingObjectDiffer::new(String::new());
        differ.diff_batch(&old_pairs, &new_pairs);

        assert_eq!(differ.finalize(), crate::diff(&new, &old));
    }

    #[test]
    fn test_patch_batcher() {
        let mut batcher = PatchBatcher::new(3);

        let batch1 = batcher.push(Delta::new(Operation::Remove, "/a".to_string()));
        assert!(batch1.is_empty());

        let batch2 = batcher.push(Delta::new(Operation::Remove, "/b".to_string()));
        assert!(batch2.is_empty());

        let batch3 = batcher.push(Delta::new(Operation::Remove, "/c".to_string()));
        assert_eq!(batch3.len(), 3);

        let remaining = batcher.flush();
        assert!(remaining.is_empty());
    }
}

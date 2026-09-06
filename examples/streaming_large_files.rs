//! Processing very large JSON files.
//!
//! Streaming is automatic: `diff_files` picks between incremental and in-memory
//! processing based on the inputs, and both paths return identical operations.
//! The `streaming` module underneath is available when you need to feed data in
//! from a custom source.
//!
//! Run with: `cargo run --example streaming_large_files`

use drift::streaming::{StreamingArrayDiffer, StreamingObjectDiffer};
use drift::{diff, diff_files, Delta};
use serde_json::{json, Value};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

/// Simulates loading an array chunk from disk/network.
fn load_array_chunk(chunk_id: usize, chunk_size: usize) -> Vec<Value> {
    (0..chunk_size).map(|i| json!({"id": chunk_id * chunk_size + i, "value": i})).collect()
}

/// Chunked equivalent of `diff` over two whole arrays.
fn streamed_diff(old: &[Value], new: &[Value], chunk_size: usize) -> Vec<Delta> {
    let mut differ = StreamingArrayDiffer::new(String::new());
    let overlap = old.len().min(new.len());
    let mut index = 0;
    while index < overlap {
        let end = (index + chunk_size).min(overlap);
        differ.diff_chunk(&old[index..end], &new[index..end], index);
        index = end;
    }
    // Both calls are required, otherwise trailing changes are lost.
    differ.set_length_change(old.len(), new.len());
    if new.len() > old.len() {
        differ.push_additions(old.len(), &new[old.len()..]);
    }
    differ.finalize()
}

/// Writes a JSON array of `count` records without building it in memory.
fn write_array(path: &Path, count: usize, modified_at: Option<usize>) -> std::io::Result<()> {
    let mut out = BufWriter::new(File::create(path)?);
    out.write_all(b"[")?;
    for i in 0..count {
        if i > 0 {
            out.write_all(b",")?;
        }
        let name = if Some(i) == modified_at { "CHANGED".to_string() } else { format!("item_{i}") };
        write!(out, r#"{{"id":{i},"name":"{name}"}}"#)?;
    }
    out.write_all(b"]")?;
    out.flush()
}

/// Example 1: the normal way to diff two files.
fn example_automatic_streaming() -> std::io::Result<()> {
    println!("\n=== Example 1: Automatic streaming via diff_files ===");

    let dir = std::env::temp_dir();
    let old_path: PathBuf = dir.join("drift_example_old.json");
    let new_path: PathBuf = dir.join("drift_example_new.json");

    // Large enough to cross the internal streaming threshold.
    let count = 1_200_000;
    write_array(&old_path, count, None)?;
    write_array(&new_path, count, Some(count / 2))?;

    for path in [&old_path, &new_path] {
        let size = std::fs::metadata(path)?.len();
        println!("  {} -> {:.1} MB", path.display(), size as f64 / (1024.0 * 1024.0));
    }

    match diff_files(&old_path, &new_path) {
        Ok(operations) => {
            println!("  operations: {}", operations.len());
            for operation in operations.iter().take(3) {
                println!("    {} {}", operation.op.as_str(), operation.path);
            }
        }
        Err(error) => println!("  failed: {error}"),
    }

    std::fs::remove_file(&old_path).ok();
    std::fs::remove_file(&new_path).ok();
    Ok(())
}

/// Example 2: driving the array differ directly, including a trailing append.
fn example_streaming_array_diff() {
    println!("\n=== Example 2: Streaming array diff ===");

    let chunk_size = 1000;
    let mut old: Vec<Value> = Vec::new();
    for chunk_id in 0..5 {
        old.extend(load_array_chunk(chunk_id, chunk_size));
    }

    let mut new = old.clone();
    for (idx, item) in new.iter_mut().enumerate() {
        if idx % 100 == 0 {
            if let Some(obj) = item.as_object_mut() {
                obj.insert("modified".to_string(), json!(true));
            }
        }
    }
    new.push(json!({"id": "appended", "value": 0}));

    let streamed = streamed_diff(&old, &new, chunk_size);
    let expected = diff(&Value::Array(new.clone()), &Value::Array(old.clone()));

    println!("  chunks of {chunk_size} over {} elements", old.len());
    println!("  operations: {}", streamed.len());
    println!("  matches diff(): {}", streamed == expected);
}

/// Example 3: objects with many keys, processed in batches.
fn example_streaming_object_diff() {
    println!("\n=== Example 3: Streaming object diff ===");

    let batch_size = 1000;
    let mut differ = StreamingObjectDiffer::new(String::new());

    let old_batch1: Vec<_> = (0..batch_size).map(|i| (format!("key_{i}"), json!(i))).collect();
    let mut new_batch1 = old_batch1.clone();
    new_batch1[100].1 = json!(999);
    differ.diff_batch(&old_batch1, &new_batch1);

    let old_batch2: Vec<_> =
        (batch_size..batch_size * 2).map(|i| (format!("key_{i}"), json!(i))).collect();
    differ.diff_batch(&old_batch2, &old_batch2);

    println!("  batches: 2 x {batch_size} keys");
    println!("  operations: {}", differ.finalize().len());
}

fn main() -> std::io::Result<()> {
    println!("drift - large file examples");
    println!("===========================");

    example_automatic_streaming()?;
    example_streaming_array_diff();
    example_streaming_object_diff();

    println!("\nDone.");
    Ok(())
}

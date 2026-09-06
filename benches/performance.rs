use criterion::{black_box, criterion_group, criterion_main, Criterion};
use drift::streaming::{PatchBatcher, StreamingArrayDiffer, StreamingObjectDiffer};
use drift::{diff, list_json_paths, patch};
use serde_json::{json, Value};

// Helper to create test data of various sizes
fn create_nested_object(depth: usize, width: usize) -> Value {
    if depth == 0 {
        json!({"value": "leaf"})
    } else {
        let mut obj = serde_json::Map::new();
        for i in 0..width {
            obj.insert(format!("field_{}", i), create_nested_object(depth - 1, width));
        }
        Value::Object(obj)
    }
}

fn create_large_array(size: usize) -> Value {
    Value::Array((0..size).map(|i| json!({"id": i, "value": format!("item_{}", i)})).collect())
}

// Benchmarks for small objects (< 1KB)
fn bench_small_diff(c: &mut Criterion) {
    let old = json!({"name": "John", "age": 30, "city": "New York"});
    let new = json!({"name": "Jane", "age": 31, "city": "New York"});

    c.bench_function("diff_small_object", |b| b.iter(|| diff(black_box(&new), black_box(&old))));
}

fn bench_small_patch(c: &mut Criterion) {
    let doc = json!({"name": "John", "age": 30});
    let operations = diff(&json!({"name": "John", "age": 31}), &doc);

    c.bench_function("patch_small_object", |b| {
        b.iter(|| patch(black_box(doc.clone()), black_box(&operations)))
    });
}

// Benchmarks for medium objects (100KB - 1MB)
fn bench_medium_diff(c: &mut Criterion) {
    let old = create_nested_object(6, 5); // ~3KB nested structure
    let mut new = old.clone();
    // Modify a deeply nested field
    if let Some(obj) = new.get_mut("field_0") {
        if let Some(obj2) = obj.get_mut("field_0") {
            if let Some(obj3) = obj2.get_mut("field_0") {
                if let Some(map) = obj3.as_object_mut() {
                    map.insert("modified".to_string(), json!(true));
                }
            }
        }
    }

    c.bench_function("diff_medium_nested_object", |b| {
        b.iter(|| diff(black_box(&new), black_box(&old)))
    });
}

fn bench_medium_array_diff(c: &mut Criterion) {
    let old = create_large_array(1000);
    let mut new = old.clone();
    // Modify middle element
    if let Value::Array(arr) = &mut new {
        if arr.len() > 500 {
            arr[500] = json!({"id": 500, "value": "modified"});
        }
    }

    c.bench_function("diff_medium_array_1000_items", |b| {
        b.iter(|| diff(black_box(&new), black_box(&old)))
    });
}

// Benchmarks for large objects
fn bench_large_array_diff(c: &mut Criterion) {
    let old = create_large_array(10000);
    let mut new = old.clone();
    if let Value::Array(arr) = &mut new {
        if arr.len() > 5000 {
            arr[5000] = json!({"id": 5000, "value": "modified"});
        }
    }

    c.bench_function("diff_large_array_10000_items", |b| {
        b.iter(|| diff(black_box(&new), black_box(&old)))
    });
}

fn bench_large_array_patch(c: &mut Criterion) {
    let doc = create_large_array(5000);
    let mut new = doc.clone();
    if let Value::Array(arr) = &mut new {
        if arr.len() > 2500 {
            arr[2500] = json!({"id": 2500, "value": "modified"});
        }
    }
    let operations = diff(&new, &doc);

    c.bench_function("patch_large_array_5000_items", |b| {
        b.iter(|| patch(black_box(doc.clone()), black_box(&operations)))
    });
}

// Benchmarks for path listing
fn bench_path_listing_small(c: &mut Criterion) {
    let doc = json!({"a": {"b": {"c": "value"}}});

    c.bench_function("list_paths_small_object", |b| {
        b.iter(|| list_json_paths(black_box(&doc), false, false, false, None))
    });
}

fn bench_path_listing_medium(c: &mut Criterion) {
    let doc = create_nested_object(5, 4);

    c.bench_function("list_paths_medium_nested", |b| {
        b.iter(|| list_json_paths(black_box(&doc), false, false, false, None))
    });
}

fn bench_path_listing_large_array(c: &mut Criterion) {
    let doc = create_large_array(5000);

    c.bench_function("list_paths_large_array_5000", |b| {
        b.iter(|| list_json_paths(black_box(&doc), false, false, false, None))
    });
}

// Memory efficiency tests
fn bench_many_small_diffs(c: &mut Criterion) {
    let base = json!({"value": 1});
    c.bench_function("multiple_small_diffs_100x", |b| {
        b.iter(|| {
            for i in 0..100 {
                let new = json!({"value": i});
                diff(black_box(&new), black_box(&base));
            }
        })
    });
}

fn bench_deep_nesting_diff(c: &mut Criterion) {
    let old = create_nested_object(10, 2); // Very deep, minimal width
    let new = old.clone();

    c.bench_function("diff_deep_nested_10_levels", |b| {
        b.iter(|| diff(black_box(&new), black_box(&old)))
    });
}

// ===== Streaming Benchmarks =====

/// Chunked equivalent of `diff` over two whole arrays.
fn streamed_diff(old: &[Value], new: &[Value], chunk_size: usize) -> Vec<drift::Delta> {
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

fn bench_streaming_array_diff(c: &mut Criterion) {
    let old = create_large_array(10_000);
    let mut new = old.clone();
    if let Value::Array(arr) = &mut new {
        for i in (0..arr.len()).step_by(1000) {
            arr[i] = json!({"modified": true});
        }
    }
    let (Value::Array(old_arr), Value::Array(new_arr)) = (&old, &new) else { unreachable!() };

    c.bench_function("streaming_array_diff_10k_items", |b| {
        b.iter(|| streamed_diff(black_box(old_arr), black_box(new_arr), 1000))
    });
}

fn bench_streaming_vs_standard_diff(c: &mut Criterion) {
    // Both arms diff the same inputs; only the strategy differs.
    let old = create_large_array(5000);
    let mut new = old.clone();
    if let Value::Array(arr) = &mut new {
        arr[2500] = json!({"modified": true});
    }
    let (Value::Array(old_arr), Value::Array(new_arr)) = (&old, &new) else { unreachable!() };

    let mut group = c.benchmark_group("streaming_comparison");

    group.bench_function("standard_diff_5k_array", |b| {
        b.iter(|| diff(black_box(&new), black_box(&old)))
    });

    group.bench_function("streaming_diff_5k_array_chunk_500", |b| {
        b.iter(|| streamed_diff(black_box(old_arr), black_box(new_arr), 500))
    });

    group.finish();
}

fn bench_streaming_object_diff(c: &mut Criterion) {
    let batch_size = 1000;
    let batch1: Vec<_> = (0..batch_size).map(|i| (format!("key_{}", i), json!(i))).collect();
    let batch2: Vec<_> =
        (batch_size..batch_size * 2).map(|i| (format!("key_{}", i), json!(i))).collect();

    c.bench_function("streaming_object_diff_2k_keys", |b| {
        b.iter(|| {
            let mut differ = StreamingObjectDiffer::new("/config".to_string());
            differ.diff_batch(black_box(&batch1), black_box(&batch1));
            differ.diff_batch(black_box(&batch2), black_box(&batch2));
            differ.finalize()
        })
    });
}

fn bench_patch_batching(c: &mut Criterion) {
    c.bench_function("patch_batcher_1000_ops_batch_100", |b| {
        b.iter(|| {
            let mut batcher = PatchBatcher::new(100);

            for i in 0..1000 {
                let op = drift::Delta::new(drift::Operation::Remove, format!("/items/{}", i));
                batcher.push(op);
            }
            batcher.flush();
        })
    });
}

fn bench_patch_batching_vs_unbatched(c: &mut Criterion) {
    let mut group = c.benchmark_group("patch_batching_comparison");

    // Individual operations
    group.bench_function("unbatched_patch_100_ops", |b| {
        b.iter(|| {
            let mut ops = Vec::new();
            for i in 0..100 {
                ops.push(drift::Delta::new(drift::Operation::Remove, format!("/items/{}", i)));
            }
            ops
        })
    });

    // Batched operations
    group.bench_function("batched_patch_100_ops_batch_25", |b| {
        b.iter(|| {
            let mut batcher = PatchBatcher::new(25);
            for i in 0..100 {
                let op = drift::Delta::new(drift::Operation::Remove, format!("/items/{}", i));
                batcher.push(op);
            }
            batcher.flush()
        })
    });

    group.finish();
}

criterion_group!(
    benches,
    // Small objects
    bench_small_diff,
    bench_small_patch,
    // Medium objects
    bench_medium_diff,
    bench_medium_array_diff,
    // Large objects
    bench_large_array_diff,
    bench_large_array_patch,
    // Path listing
    bench_path_listing_small,
    bench_path_listing_medium,
    bench_path_listing_large_array,
    // Additional tests
    bench_many_small_diffs,
    bench_deep_nesting_diff,
    // Streaming benchmarks
    bench_streaming_array_diff,
    bench_streaming_vs_standard_diff,
    bench_streaming_object_diff,
    bench_patch_batching,
    bench_patch_batching_vs_unbatched,
);

criterion_main!(benches);

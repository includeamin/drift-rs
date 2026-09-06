# drift

`drift` is a Rust library and command-line tool for calculating RFC 6902 JSON
Patch operations between structured documents.

## Installation

Download and install the latest binary for your platform:

```bash
curl -fsSL https://github.com/includeamin/drift-rs/raw/main/install.sh | bash
```

Or clone and run the installer directly:

```bash
git clone https://github.com/includeamin/drift-rs.git
cd drift-rs
bash install.sh
```

The installer automatically detects your operating system and architecture, downloads the latest verified binary from GitHub releases, and installs it to `~/.local/bin`. No Rust toolchain required!

**Options:**
- Install a specific version: `bash install.sh --version v0.13.1`
- Custom installation prefix: `PREFIX=$HOME/.cargo bash install.sh`
- Custom binary directory: `BIN_DIR=/usr/local/bin bash install.sh`

**Supported platforms:**
- Linux x86_64
- macOS Intel (x86_64)
- macOS Apple Silicon (aarch64)
- Windows x86_64

## Build and test

To build from source, Rust 1.70 or newer is required:

```bash
cargo test
cargo build --release
```

The test suite includes 60+ tests covering:
- Pointer operations (escaping, joining, splitting)
- Diff algorithms for objects and arrays
- Patch operations (add, remove, replace, test)
- Path listing with various options
- Search and filtering with regex
- Edge cases and round-trip validation
- Unicode and complex data structures

Coverage reports are generated automatically on pull requests via [Codecov](https://codecov.io).

## CLI

```bash
drift diff OLD NEW
drift patch DOCUMENT PATCH
drift paths DOCUMENT
drift check OLD NEW
```

JSON, YAML, TOML, and XML documents are supported. The format is detected from
the file extension or selected with `--format`. Use `-` for stdin and
`-o/--output` for a file destination.

`diff` supports `--stats`, `--pretty`, `--exit-code`, `--path`, `--field`,
`--grep`, `--op`, and `--invert-match`. `paths` supports `--values`,
`--containers`, `--include-root`, `--sort-keys`, and `--max-depth`.

## Library

```rust
use drift::{diff, patch};
use serde_json::json;

let old = json!({"name": "David"});
let new = json!({"name": "Alex"});
let operations = diff(&new, &old);
assert_eq!(patch(old, &operations)?, new);
# Ok::<(), drift::DriftError>(())
```

## Performance

### Benchmarking

Run the benchmark suite to measure performance on your system:

```bash
cargo bench
```

This runs comprehensive benchmarks covering:
- Small objects (< 1KB)
- Medium objects (100KB - 1MB)
- Large arrays (10,000+ items)
- Deeply nested structures
- Path listing operations
- Memory efficiency with multiple operations

Benchmark results are saved to `target/criterion/` with HTML reports for visual analysis.

### Performance Characteristics

**✅ Optimized for:**
- Small to medium-sized documents (< 100MB)
- Objects with many fields but shallow nesting
- Repeated diff/patch operations on similar data
- Sparse changes (small diff relative to document size)

**⚠️ Not optimized for:**
- Very large value cloning (entire values are cloned on add/replace)
- Array middle insertions (O(n) due to element shifting)
- Extremely deep nesting (recursion depth can impact performance)

### Optimization Recommendations

For large objects (> 10MB):
1. **Pre-filter changes** - Only diff/patch relevant subtrees
2. **Use targeted paths** - Apply operations to specific paths instead of entire objects
3. **Batch operations** - Group multiple patches to reduce overhead

Large top-level JSON arrays are streamed automatically; see below.

### Known Limitations

1. **Value cloning** - Each add/replace operation clones the value
2. **Array operations** - Inserting into middle of arrays is O(n)
3. **Full traversal** - Diff visits all nodes, even unchanged subtrees
4. **No shallow mode** - Cannot skip unchanged nested objects

Future optimizations planned:
- Copy-on-write semantics to reduce cloning
- Shallow diff option for unchanged subtrees
- Streaming for large top-level objects (arrays are already covered)

## Streaming and Incremental Processing

Streaming is **automatic**. There is no flag to set.

`drift diff` and the `drift::diff_files` library function inspect the inputs and
pick a strategy: when either file is larger than 32 MB and both hold a top-level
JSON array, elements are parsed and compared incrementally and the full document
is never materialised. Everything else is loaded normally.

Both paths emit **exactly the same operations, in the same order**. The choice
only affects peak memory.

```rust
use drift::diff_files;
use std::path::Path;

// Streams or loads automatically depending on the inputs
let operations = diff_files(Path::new("old.json"), Path::new("new.json"))?;
```

Measured on a 47.5 MB array of 400k objects, peak memory drops from 735 MB to
7.6 MB. See [Performance Characteristics](#performance-characteristics) below.

Streaming is skipped when it cannot work or would change behaviour:

- Input read from stdin (`-`), which is not re-readable
- Non-JSON formats (YAML, TOML, XML)
- Documents whose top-level value is not an array
- `--grep`, which matches against values in the old document and therefore
  needs it fully loaded

### Lower-level APIs

The `streaming` module exposes the building blocks directly, for feeding data in
from a custom source. These also produce output identical to `diff`.

### Use Cases
- **Large arrays** (millions of records)
- **Objects with many keys** (thousands of fields)
- **Incremental updates** to huge files
- **Memory-constrained environments**

### Streaming Array Differ

Process large arrays chunk by chunk without loading everything into memory:

```rust
use drift::streaming::StreamingArrayDiffer;
use serde_json::json;

let mut differ = StreamingArrayDiffer::new("/users".to_string());

// Process in chunks (e.g., 10,000 items at a time)
for chunk_id in 0..num_chunks {
    let old_chunk = load_array_chunk(chunk_id); // Load from disk
    let new_chunk = load_array_chunk_modified(chunk_id);
    
    differ.diff_chunk(&old_chunk, &new_chunk, chunk_id * chunk_size);
}

// Required: record trailing removals and/or appended elements
differ.set_length_change(old_len, new_len);
if new_len > old_len {
    differ.push_additions(old_len, &new_tail);
}

let operations = differ.finalize();
```

`diff_chunk` compares only the overlapping prefix of the two slices, so the
chunks may differ in length. Elements past the end of the shorter array are
handled by `set_length_change` and `push_additions`.

**Benefits:**
- Memory: O(chunk_size) instead of O(array_size)
- Process data as it arrives from disk/network
- Scale to billions of elements

### Streaming Object Differ

For objects with thousands of keys, process in batches:

```rust
use drift::streaming::StreamingObjectDiffer;

let mut differ = StreamingObjectDiffer::new("/config".to_string());

// Process batches of keys (e.g., 1,000 at a time)
for batch in key_batches {
    differ.diff_batch(&old_batch, &new_batch);
}

let operations = differ.finalize();
```

### Patch Batching

Apply patches more efficiently by batching operations:

```rust
use drift::streaming::PatchBatcher;
use drift::{patch, Operation, Delta};

let mut batcher = PatchBatcher::new(100); // Batch size

for i in 0..1_000_000 {
    let operation = Delta::new(Operation::Remove, format!("/items/{}", i));
    
    // Returns a batch when full, otherwise empty
    let batch = batcher.push(operation);
    if !batch.is_empty() {
        document = patch(document, &batch)?;
    }
}

// Don't forget remaining operations
let remaining = batcher.flush();
if !remaining.is_empty() {
    document = patch(document, &remaining)?;
}
```

**Benefits:**
- Reduces tree traversals from O(ops) to O(ops/batch_size)
- Lower memory allocations
- Better cache locality

### Example: Processing a large JSON file

```bash
cargo run --example streaming_large_files
```

This demonstrates:
1. Automatic streaming via `diff_files` on a 40 MB array
2. Driving the array differ in chunks, verified against `diff`
3. Diffing objects with many keys in batches
4. Batching patch operations

### Performance Characteristics

Streaming trades nothing for a large reduction in peak memory. It is **not
faster** — the work done is the same. Measured on this machine, 5k-element
array, one changed element:

| Approach | Time |
|----------|------|
| Standard | 4.97 ms |
| Streaming (chunk 500) | 4.71 ms |

The difference is memory. Measured on a 47.5 MB array of 400k objects:

| Approach | Peak RSS |
|----------|----------|
| Standard | 735 MB |
| Streaming | 7.6 MB |

Peak memory for the in-memory path runs roughly 15x the file size, because a
`serde_json::Value` tree is much larger than its serialized form. The streaming
path stays flat regardless of input size.

### Streaming Benchmarks

Run benchmarks to measure streaming performance on your system:

```bash
cargo bench streaming
```

This includes:
- Streaming vs standard diff comparison
- Array chunking benchmarks
- Object batch processing benchmarks
- Patch batching efficiency tests

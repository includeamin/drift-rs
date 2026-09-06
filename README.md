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

The installer detects your operating system and architecture, downloads the
latest verified binary from GitHub releases, and installs it to `~/.local/bin`.
No Rust toolchain required.

**Options:**
- Install a specific version: `bash install.sh --version v0.13.0`
- Custom installation prefix: `PREFIX=$HOME/.cargo bash install.sh`
- Custom binary directory: `BIN_DIR=/usr/local/bin bash install.sh`

**Supported platforms:**
- Linux x86_64
- macOS Intel (x86_64)
- macOS Apple Silicon (aarch64)
- Windows x86_64

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

## Large files

Streaming is automatic. There is no flag to set.

`drift diff` and the `drift::diff_files` library function inspect the inputs and
pick a strategy: when either file is larger than 32 MB and both hold a top-level
JSON array, elements are parsed and compared incrementally and the full document
is never materialised. Everything else is loaded normally.

Both paths emit **exactly the same operations, in the same order**. The choice
only affects peak memory.

```rust
use drift::diff_files;
use std::path::Path;

let operations = diff_files(Path::new("old.json"), Path::new("new.json"))?;
# Ok::<(), drift::DriftError>(())
```

Streaming is skipped when it cannot work or would change behaviour:

- Input read from stdin (`-`), which is not re-readable
- Non-JSON formats (YAML, TOML, XML)
- Documents whose top-level value is not an array
- `--grep`, which matches against values in the old document and therefore
  needs it fully loaded

### Lower-level APIs

`drift::streaming` exposes `StreamingArrayDiffer` and `StreamingObjectDiffer`
for feeding data in from a custom source. Both produce output identical to
`diff`.

```rust,ignore
use drift::streaming::StreamingArrayDiffer;

let mut differ = StreamingArrayDiffer::new(String::new());
for (start, (old_chunk, new_chunk)) in chunks {
    differ.diff_chunk(old_chunk, new_chunk, start);
}
// Required, or trailing removals and appends are lost
differ.set_length_change(old_len, new_len);
if new_len > old_len {
    differ.push_additions(old_len, new_tail);
}
let operations = differ.finalize();
```

`diff_chunk` compares only the overlapping prefix of the two slices, so chunks
may differ in length. Elements past the end of the shorter array are handled by
`set_length_change` and `push_additions`.

A runnable walkthrough lives in
[`examples/streaming_large_files.rs`](examples/streaming_large_files.rs):

```bash
cargo run --release --example streaming_large_files
```

## Performance

Streaming is **not faster** — it does the same work. The win is memory.

Measured on a 5k-element array with one changed element:

| Approach | Time |
|----------|------|
| Standard | 4.97 ms |
| Streaming (chunk 500) | 4.71 ms |

Measured on a 47.5 MB array of 400k objects:

| Approach | Peak RSS |
|----------|----------|
| Standard | 735 MB |
| Streaming | 7.6 MB |

Peak memory for the in-memory path runs roughly 15x the file size, because a
`serde_json::Value` tree is much larger than its serialized form. The streaming
path stays flat regardless of input size.

**Well suited to:** shallow documents, sparse changes, repeated diffs on
similar data.

**Costs to be aware of:** add/replace operations clone the value; inserting into
the middle of an array shifts every later element; diff visits all nodes, even
unchanged subtrees.

### Benchmarks

```bash
cargo bench
```

Covers small/medium/large objects, deeply nested structures, path listing, and
streaming vs standard diff. Results are written to `target/criterion/` with HTML
reports.

## Build and test

Rust 1.70 or newer is required:

```bash
cargo test
cargo build --release
```

The suite has 78 tests covering pointer operations, diff and patch behaviour,
path listing, regex search and filtering, round-trip validation, unicode, and
streaming/in-memory equivalence. Coverage reports are generated on pull requests
via [Codecov](https://codecov.io).

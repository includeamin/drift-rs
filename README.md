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

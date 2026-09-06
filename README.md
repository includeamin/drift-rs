# drift

`drift` is a Rust library and command-line tool for calculating RFC 6902 JSON
Patch operations between structured documents.

## Build and test

Rust 1.70 or newer is required.

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

## Releases

Pushing a version tag such as `v0.13.1` builds native command-line binaries
for Linux x86_64, Windows x86_64, macOS Intel, and macOS Apple Silicon. The
GitHub release includes a SHA-256 checksum beside each binary. The workflow can
also be started manually for an existing `v*` tag.

Install the native executable with:

```bash
bash install.sh
```

The installer downloads and verifies the matching published release artifact;
Cargo is not required. Install a specific release with
`bash install.sh --version v0.13.1`. `PREFIX` and `BIN_DIR` can be overridden
through environment variables.

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

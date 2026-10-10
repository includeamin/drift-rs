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

## Try it in the browser

An interactive demo runs drift as WebAssembly, entirely client-side. Paste or
drop two JSON, YAML, TOML or XML documents to get a side-by-side visual diff,
the RFC 6902 operations, and metrics (parse and diff time, sizes, operation
counts, and a patch round-trip check).

Live at <https://includeamin.github.io/drift-rs/> once Pages is enabled
(Settings → Pages → Source: *GitHub Actions*).

To run it locally:

```bash
make web-serve    # needs wasm-pack; serves http://localhost:8000
```

The WASM bindings live in [`web/`](web/) and reuse `drift::formats` for parsing.

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
`--containers`, `--include-root`, `--sort-keys`, `--max-depth`, and `--json`.

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

### Patches as data, undo and squash

`Delta` reads and writes the standard RFC 6902 JSON, so a patch is a
`Vec<Delta>` and travels through serde like any other value. Reading checks
what the RFC requires: `op` and `path`, `value` for `add`/`replace`/`test`, and
`from` for `move`/`copy`.

```rust
use drift::{compose, diff, invert, patch, DiffOptions};

let operations = diff(&new, &old);
let json = serde_json::to_string(&operations)?;          // store or send it

let undo = invert(&old, &operations)?;                    // the patch that reverses it
assert_eq!(patch(new.clone(), &undo)?, old);

let squashed = compose(&old, &many_steps, &DiffOptions::default())?;  // one equivalent patch
```

`invert` replays the patch against the document it applies to, so it must apply
cleanly. Undoing a removal puts an object member back as a new key, so the
restored document is equal to the original but the member can sit last instead
of where it was. `compose` needs the same base document: it is the difference
between that document and the patched one.

### Cargo features

| Feature | Enables |
|---------|---------|
| `yaml`, `toml`, `xml` | Reading and writing that format in `drift::formats` (JSON is always available) |
| `yaml-comments` | Keeping comments when patching YAML (implies `yaml`; needs `yaml-edit`) |
| `cli` | The `drift` binary (implies the three formats and `yaml-comments`) |

All are on by default. A library user who only needs JSON can drop the rest,
which also drops `serde_yaml`, `toml`, `roxmltree` and `clap` from the build:

```toml
drift = { version = "0.15", default-features = false }
```

Using a format whose feature is off returns an error naming the feature to
enable. The minimum supported Rust version is 1.77, checked in CI by
`scripts/check-msrv.sh`.

## Arrays

By default arrays are compared by position, so inserting an item at the front
produces a replacement for every item after it. Give drift a field that
identifies items and it matches them by value instead:

```bash
drift diff --array-key id old.json new.json   # repeatable; first usable key wins
```

```rust
use drift::{diff_with, DiffOptions};

let options = DiffOptions::new().array_key("id");
let operations = diff_with(&new, &old, &options);
```

An array is matched by a key when every item of both arrays is an object with
a unique string, number or boolean value for it; otherwise that array falls
back to positional comparison. An insert or delete is then one operation, and a
reordering produces `move` operations. `--array-key` loads both documents into
memory; the streaming paths for very large files stay positional.

## Ignoring parts of a document

Timestamps, request ids and other noise can be left out of the diff:

```bash
drift diff --ignore-path '/**/updatedAt' --ignore-path /meta old.json new.json
```

```rust
use drift::{diff_with, DiffOptions};

let options = DiffOptions::new().ignore_path("/**/updatedAt").ignore_path("/meta");
let operations = diff_with(&new, &old, &options);
```

Patterns are JSON Pointers where `*` matches one token and `**` any number
(including none), so `/users/*/lastSeen` covers that member of every user. An
ignored part is skipped, not filtered afterwards: nothing under it is compared,
and whether it was added, removed or changed is not reported. Patching with the
result leaves those parts as they were in the old document.

Adding or removing an array *item* is always reported, even when a pattern
covers it, because skipping it would shift the indices of the operations after
it; such a pattern ignores changes *inside* the item. `--ignore-path` loads both
documents into memory, like `--array-key`.

`DiffOptions` is `#[non_exhaustive]`: build it with `DiffOptions::new()` and its
methods rather than a struct literal, so new options never break callers.

## Patching files without losing their comments

`drift patch` reads a document, patches the value tree and writes it back.
Writing the tree out from scratch would drop comments and layout, so for **TOML**
the original text is edited in place instead: comments, blank lines, key order
and number or string styles (`1_000`, `0xFF`, `'literal'`) are untouched wherever
the value did not change, and a changed value keeps the comment after it.

```toml
port = 8080   # http          drift patch  [replace /port 9090]  ->   port = 9090   # http
```

Comments follow their own entries when one is removed from a list, including a
multi-line array with a comment per item and an array of `[[tables]]`. A new
table is written at the end of the document. The result is checked to read back
as the patched value, and if it does not (or the original cannot be edited in
place) the document is written out fresh, which is always correct but drops the
comments.

**YAML** is edited in place as well (feature `yaml-comments`, on by default): the
comments after values and in front of keys survive changed values, added keys and
removed keys, and untouched lines come out byte for byte. On a commented
Kubernetes manifest, five patch operations changed exactly five lines and all
eight comments survived. The library behind it is younger and less dependable
than the TOML one, so YAML has a few more limits:

- a new list or mapping is written in flow style (`labels: {team: core}`), which
  is valid YAML on one line;
- the comment of a removed entry can be left behind as a stray line;
- adding or removing items of a list is done in place where the library copes
  with the layout, and otherwise the list is replaced whole, which loses the
  comments *inside that list* (nothing else is affected);
- after every edit the result is read back, and if it differs from the patched
  value the document is written out fresh, which is correct but drops the
  comments. That happened for about 3% of 500 random multi-part edits.

The library function is `drift::formats::dump_like(original, &new, format, compact)`.
**XML** is not preserved yet: it is written fresh, so comments in XML files are
still lost.

## Format notes

Documents are converted to a JSON value tree, which loses a few things:

- **XML:** attributes become `@name` keys and text becomes `#text`. Namespace
  prefixes and `xmlns` declarations are kept. All text of an element is joined,
  so its position relative to child elements is lost. A single child element
  reads as an object and repeated children as an array, so a one-entry list
  reads back as a scalar. Pass `--xml-arrays` (or
  `ParseOptions { xml_arrays: true }`) to always read children as arrays, which
  keeps the shape stable across versions of a document.
- **Depth:** JSON, YAML and XML documents nested more than 128 levels deep are
  rejected with a `recursion limit exceeded` error.
- **TOML:** datetimes and `nan`/`inf`/`-inf` become JSON strings, since JSON has
  neither. `drift patch` (and `formats::parse_with` / `dump_with`) remembers
  where they were and writes them back as datetimes and floats; a quoted string
  that merely looks like a date stays a string. Paths into arrays are matched by
  index, so a patch that shifts an array of datetimes can lose that.

## Large files

`drift diff` and the `drift::diff_files` library function inspect the inputs and
pick a strategy. When either file is larger than 32 MB:

- **Array roots** (`[...]`) are compared element by element, streaming.
- **Object roots** (`{...}`) are indexed by byte offset, then each member is
  handled on its own: large array members stream, large object members recurse
  the same way, everything else is parsed individually. So both
  `{"users": [ ...900k... ]}` and `{"response": {"payload": {"users": [...] }}}`
  stream the array without materialising it.

Anything else is loaded normally.

Both paths emit **exactly the same operations, in the same order**. The choice
only affects peak memory, not speed — the work done is the same.

| Input | Standard | Streaming |
|-------|----------|-----------|
| 47.5 MB array of 400k objects | 735 MB | 7.6 MB |
| 36.9 MB `{"users": [600k], "meta": {}}` | 995 MB | 7 MB |
| 36.9 MB `{"response": {"payload": {"users": [600k]}}}` | 995 MB | 7 MB |

Peak memory for the in-memory path runs 15-27x the file size, because a
`serde_json::Value` tree is much larger than its serialized form. The streaming
path stays flat regardless of input size.

The table was measured before objects kept their key order. Preserving order
costs the in-memory path about a third more memory (a 36 MB array of 400k
objects: 857 MB before, 1,157 MB now) at about the same speed; streaming is
unchanged.

```rust
use drift::diff_files;
use std::path::Path;

let operations = diff_files(Path::new("old.json"), Path::new("new.json"))?;
# Ok::<(), drift::DriftError>(())
```

`diff_files` loads the whole document instead when:

- Both roots are not arrays, both are not objects, or the pair is mismatched
- A member is under 1 MB, or is neither an array nor an object; these are parsed
  individually, which is cheap at that size

`drift diff` additionally loads when:

- Input comes from stdin (`-`), which is not re-readable
- The format is not JSON (YAML, TOML, XML)
- `--grep` is used, since it matches against values in the old document and so
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

## Build and test

The library supports Rust 1.77 or newer (checked in CI). Building and testing
this repository, including the web demo, uses the current stable toolchain:

```bash
cargo test
cargo build --release
cargo bench
```

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
drift invert DOCUMENT PATCH
drift compose DOCUMENT PATCH...
```

JSON, YAML, TOML, and XML documents are supported. The format is detected from
the file extension or selected with `--format`. Use `-` for stdin and
`-o/--output` for a file destination.

`diff` supports `--stats`, `--pretty`, `--exit-code`, `--path`, `--field`,
`--grep`, `--op`, and `--invert-match`. `paths` supports `--values`,
`--containers`, `--include-root`, `--sort-keys`, `--max-depth`, and `--json`.

`invert` prints the patch that undoes `PATCH` when it is applied to `DOCUMENT`,
so a change can be rolled back; `compose` squashes several patches, applied in
the order given, into one equivalent patch (`--array-key` as for `diff`). Both
need the document the patch applies to, and the patch must apply cleanly.

```bash
drift invert config.yaml change.json -o undo.json   # first, while config.yaml is unchanged
drift patch config.yaml change.json --in-place      # apply the change
drift patch config.yaml undo.json --in-place        # later: roll it back
```

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
| `csv` | Comparing CSV and TSV files as records (needs the `csv` crate) |
| `cli` | The `drift` binary (implies the three formats, `yaml-comments` and `csv`) |

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
affects peak memory, and in these runs streaming was also somewhat faster.

| Input | Standard | Streaming |
|-------|----------|-----------|
| 48 MB array of 400k objects | 1.27 GB | 18 MB |
| 36.9 MB `{"users": [600k], "meta": {}}` | 1.29 GB | 19 MB |
| 36.9 MB `{"response": {"payload": {"users": [600k]}}}` | 1.29 GB | 21 MB |

Peak memory for the in-memory path runs 26-35x the file size, because a
`serde_json::Value` tree is much larger than its serialized form. The streaming
path stays flat regardless of input size, at about 20 MB (an idle `drift`
process is about 4 MB).

Measured as the process's own peak memory with the release binary, on generated
files of those shapes (old and new differing in every 50th object); the standard
path reads the second document from stdin, which forces it into memory. It takes
roughly 3 s against 2-3 s for streaming on this machine, so timings are close and
vary from run to run. Keeping key order made the standard path use about 30% more
memory and take 20-30% longer than before (965 MB and 2.7 s on the array);
streaming is unchanged. An earlier version of this table listed 7 MB for
streaming, which does not reproduce.

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

### Files of records (NDJSON, CSV, TSV)

A file with one JSON object per line (`.ndjson` or `.jsonl`), or a CSV or TSV file
with a header row (`.csv`, `.tsv`), can be compared without loading it, matching
rows by a key:

```bash
drift diff --array-key id old.ndjson new.ndjson
drift diff --array-key id,region old.ndjson new.ndjson     # a composite key
drift diff --array-key /user/id old.ndjson new.ndjson      # a field inside the row
drift diff --keyless old.ndjson new.ndjson                 # no key: match whole rows
```

```rust
use drift::{diff_records_with, DiffOptions, Duplicates, RecordOptions};

let options = RecordOptions::new()
    .key("id")
    .key("region")
    .duplicates(Duplicates::Last)
    .diff(DiffOptions::new().ignore_path("/*/updatedAt"));
let operations = diff_records_with(old, new, &options)?;
```

Each file is read once, in parallel, into a small index: per row a hash of its
key, a hash of its bytes and where it starts in the file (40 bytes). Only the key
is parsed while reading. Rows are matched by key, rows whose bytes are equal are
skipped without being parsed, and only rows that differ are read back from disk.
The result is exactly what the in-memory diff gives for the same rows as arrays
matched by that key (the same planner produces both), so a row inserted at the
front is one `add`, not a replacement of every row after it.
`--ignore-path '/*/field'` works inside rows. With a key, the first `--array-key`
is the row key and any others match arrays inside rows.

- **Composite and nested keys:** `--array-key id,region`, or a JSON Pointer such
  as `/user/id`. Each part must be a string, number or boolean.
- **Repeated keys** are an error naming both lines, unless `--duplicates` says
  what to do: `first` or `last` keeps one row per key and ignores the rest (rows
  are then numbered without the ignored ones), and `occurrence` matches the nth
  row with a key to the nth row with that key in the other file.
- **`--keyless`** matches rows by their whole content (field order and spacing do
  not matter), so no key is needed and rows may be any JSON value. A changed row is
  a removal plus an addition, never an edit, and identical rows are counted:
  `--duplicates occurrence` is the default there.

#### CSV and TSV

The first line is the header, and each later line is a row, read as an object of
its cells keyed by the column names, so everything above works the same way:
keys, composite keys, `--duplicates`, `--keyless` and `--ignore-path '/*/column'`.
Row numbers do not count the header.

```bash
drift diff --array-key id old.csv new.csv
drift diff --array-key id --types id:int,score:float,active:bool old.csv new.csv
drift diff --array-key id --delimiter ';' old.csv new.csv     # .tsv uses tabs by default
```

```rust
use drift::{ColumnType, CsvOptions, RecordFormat, RecordOptions};

let csv = CsvOptions::new().delimiter(b';').column("id", ColumnType::Int);
let options = RecordOptions::new().key("id").format(RecordFormat::Csv(csv));
```

- **Cells are strings** unless a column is given a type, so `007` stays `"007"` and
  nothing is guessed from the data. Types are `string`, `int`, `float`, `bool` and
  `json`; an empty cell of a typed column is `null`, and a cell that does not fit
  its type is an error naming the file, line, column and cell.
- **Quoted cells** may contain delimiters, quotes and line breaks. Lines in error
  messages are physical lines, so a row after a two-line cell is reported where it
  really is. A byte order mark and blank lines are ignored.
- **Every row is checked** while the file is indexed, not only the ones that
  change: a row with the wrong number of cells is an error, as are an empty or
  repeated column name, a key column that is not in the header, and invalid UTF-8.
- **The two files may have different columns**; a renamed column shows up as a
  removal and an addition, even when the cells are identical.
- **CSV is read on one thread**, because a quoted cell can contain a line break, so
  a line start is not always a row start and the file cannot be cut at newlines.
  Key parts are columns: a JSON Pointer into a row has nothing to reach into.

On two CSV files of 400,000 rows (21 MB each, the same rows and changes as the
NDJSON files below), the key takes 51 MB and about 1.3 s of single-threaded work,
typed columns cost the same, and `--keyless` takes 61 MB and about 3 s. The
operations are the same as for the NDJSON files, except that cells are strings
unless typed.

Measured on two NDJSON files of 400,000 rows (48 MB each) with 8,000 rows changed,
1,000 inserted in the middle and 1,000 deleted, on the same machine:

| | Peak memory | Time |
|---|---|---|
| In memory, before the keyed-diff fix | 1,283 MB | 198 s |
| In memory, now | 1,283 MB | 6.9 s |
| Streaming records, first version | 100 MB | 2.8 s |
| **Streaming records, `--array-key id`** | **52 MB** | **0.7-1.2 s** |
| Streaming records, `--array-key id,name` | 58 MB | about 1 s |
| Streaming records, `--keyless` | 65 MB | 2.6-3.6 s |

The in-memory and keyed streaming runs produced byte-identical output. Keyless is
slower because every row has to be parsed in full to be fingerprinted. The
in-memory speed-up has two causes: matched rows were looked up from the start of
the list each time, which made a keyed diff quadratic in the number of rows
(80,000 rows took 3.5 s), and equal values were walked field by field instead of
being skipped.

#### Files with more rows than fit in memory

The index above takes about 130 bytes a row across the two files. For files too
big for that, give a budget:

```bash
drift diff --array-key id --memory-limit 256M old.ndjson new.ndjson
drift diff --array-key id --memory-limit 2G --temp-dir /mnt/scratch old.csv new.csv
```

```rust
let options = RecordOptions::new().key("id").memory_limit(256 << 20);
```

Each file is scanned once and its rows are sorted by key through temporary files
(an external merge sort), the two sorted streams are merge-joined, and what the
join finds is sorted again into file order. Rows are read back from the files only
where an operation needs them, and operations are written as they are found, so
nothing is held per row. The operations are the ones the default gives, repeated
keys and `--keyless` included.

- **It is a budget, not a cap.** About an eighth of the limit goes to each of the
  sort buffers, and a few more megabytes to read buffers, so the process takes
  somewhat more than the limit when the limit is small. Memory does not grow with
  the number of rows.
- **Rows in both files must be in the same order in each**, which holds when one
  file is a later version of the other. Otherwise the diff has to emit `move`s, and
  working those out takes memory for every row. If the rows seem to fit in the
  limit (about 64 bytes a row) the diff is then done in memory; if not, it is an
  error before any output is written.
- **It is slower** than the default for files that fit in memory, and needs disk
  space for the sort: roughly 50 bytes a row for each file, in `--temp-dir`.
  Temporary files are removed when the diff ends, fails or is stopped.

Measured on generated NDJSON files of 1 and 4 million rows (137 MB and 554 MB
each; about 1% of rows changed, 0.5% removed and 0.5% added), on a busy machine
(load average above 10), so the times are only indicative:

| Rows | Mode | Peak memory | Time |
|---|---|---|---|
| 1 M | in memory | 120 MB | 9-17 s |
| 1 M | `--memory-limit 64M` | 29 MB | 7-8 s |
| 1 M | `--memory-limit 256M` | 101 MB | 5 s |
| 4 M | in memory | 600 MB | 121 s |
| 4 M | `--memory-limit 64M` | 32 MB | 44 s |
| 4 M | `--memory-limit 256M` | 101 MB | 41 s |

Peak memory with a limit stays flat as the files grow; the in-memory figure does
not.

Limits: NDJSON rows must be JSON objects (except with `--keyless`), and record
files cannot come from stdin or be used with `--grep`. Without `--memory-limit`
memory is proportional to the number of rows. Rows with equal hashes are treated as
equal, which is wrong only for a 128-bit collision.

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

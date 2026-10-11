pub mod formats;

use self::formats::{load, read, resolve, Format};
use clap::{Args, Parser, Subcommand};
use drift::{
    compose, diff, diff_files, diff_with, filter_operations, invert, patch, Delta, DiffOptions,
    Operation, VERSION,
};
use serde_json::Value;
use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

#[derive(Parser)]
#[command(name="drift", version=VERSION, about="RFC 6902 structured document diff and patch tooling")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Diff(Box<DiffArgs>),
    Patch(PatchArgs),
    Paths(PathsArgs),
    Check(CheckArgs),
    Invert(InvertArgs),
    Compose(ComposeArgs),
}
#[derive(Args)]
struct OutputArgs {
    #[arg(short, long)]
    output: Option<String>,
    #[arg(long, default_value_t = 2)]
    indent: usize,
    #[arg(long)]
    compact: bool,
}
#[derive(Args)]
struct DiffArgs {
    old: String,
    new: String,
    #[arg(long)]
    stats: bool,
    #[arg(long)]
    pretty: bool,
    #[arg(long)]
    exit_code: bool,
    #[arg(long, value_enum)]
    format: Option<Format>,
    /// XML: always read child elements as arrays, even a single one
    #[arg(long)]
    xml_arrays: bool,
    #[arg(long = "path")]
    paths: Vec<String>,
    #[arg(long = "field")]
    fields: Vec<String>,
    #[arg(long = "grep")]
    values: Vec<String>,
    #[arg(long = "op")]
    operations: Vec<String>,
    #[arg(long)]
    invert_match: bool,
    /// Match array items by this field (repeatable, first usable one wins)
    /// instead of by position. Loads both documents into memory.
    #[arg(long = "array-key")]
    array_keys: Vec<String>,
    /// Leave this part of the document out of the diff (repeatable). A JSON
    /// Pointer where `*` matches one token and `**` any number: `/updatedAt`,
    /// `/users/*/lastSeen`, `/**/id`. Loads both documents into memory.
    #[arg(long = "ignore-path")]
    ignore_paths: Vec<String>,
    /// NDJSON files: match rows by their whole content, so no key is needed;
    /// a changed row is then a removal and an addition. `--array-key` values
    /// then apply to arrays inside rows.
    #[arg(long)]
    keyless: bool,
    /// NDJSON files: what to do when several rows share a key. The default is
    /// to refuse (`occurrence` for `--keyless`).
    #[arg(long, value_enum)]
    duplicates: Option<DuplicatesArg>,
    /// CSV and TSV files: the type of some columns, as COLUMN:TYPE pairs
    /// (comma-separated or repeated), with TYPE one of string, int, float, bool
    /// or json. Cells are strings otherwise.
    #[arg(long = "types")]
    types: Vec<String>,
    /// CSV and TSV files: the character between cells (default: a comma, or a
    /// tab for .tsv). One character, or tab, comma, semicolon or pipe.
    #[arg(long)]
    delimiter: Option<String>,
    /// Record files: diff files too big for memory, sorting through temporary
    /// files with about this much memory (a number of bytes, or with a K, M or
    /// G suffix: 512M). Slower than the default. Needs rows that are in both
    /// files to be in the same order in each.
    #[arg(long = "memory-limit")]
    memory_limit: Option<String>,
    /// Record files: where `--memory-limit` puts its temporary files (default:
    /// the system's temporary directory).
    #[arg(long = "temp-dir")]
    temp_dir: Option<String>,
    #[command(flatten)]
    output: OutputArgs,
}
#[derive(Args)]
struct PatchArgs {
    document: String,
    patch: String,
    #[arg(long)]
    in_place: bool,
    #[arg(long, value_enum)]
    format: Option<Format>,
    /// XML: always read child elements as arrays, even a single one
    #[arg(long)]
    xml_arrays: bool,
    #[command(flatten)]
    output: OutputArgs,
}
#[derive(Args)]
struct PathsArgs {
    document: String,
    #[arg(long)]
    values: bool,
    #[arg(long)]
    containers: bool,
    #[arg(long)]
    include_root: bool,
    #[arg(long)]
    sort_keys: bool,
    #[arg(long)]
    max_depth: Option<usize>,
    #[arg(long)]
    json: bool,
    #[arg(long, value_enum)]
    format: Option<Format>,
    /// XML: always read child elements as arrays, even a single one
    #[arg(long)]
    xml_arrays: bool,
    #[command(flatten)]
    output: OutputArgs,
}
#[derive(Args)]
struct CheckArgs {
    old: String,
    new: String,
    #[arg(long, value_enum)]
    format: Option<Format>,
    /// XML: always read child elements as arrays, even a single one
    #[arg(long)]
    xml_arrays: bool,
    #[command(flatten)]
    output: OutputArgs,
}

/// The patch that undoes another: the operations to apply to the patched
/// document to get `DOCUMENT` back.
#[derive(Args)]
struct InvertArgs {
    /// The document the patch applies to
    document: String,
    /// The patch to undo, as RFC 6902 JSON (`-` for stdin)
    patch: String,
    #[arg(long, value_enum)]
    format: Option<Format>,
    /// XML: always read child elements as arrays, even a single one
    #[arg(long)]
    xml_arrays: bool,
    #[command(flatten)]
    output: OutputArgs,
}

/// One patch equal to applying several in turn, without the detours: a value
/// set twice is set once, and a key added then removed disappears.
#[derive(Args)]
struct ComposeArgs {
    /// The document the first patch applies to
    document: String,
    /// The patches in the order they are applied, as RFC 6902 JSON (`-` for
    /// stdin, once)
    #[arg(required = true)]
    patches: Vec<String>,
    #[arg(long, value_enum)]
    format: Option<Format>,
    /// XML: always read child elements as arrays, even a single one
    #[arg(long)]
    xml_arrays: bool,
    /// Match array items by this field (repeatable) when working out the result
    #[arg(long = "array-key")]
    array_keys: Vec<String>,
    #[command(flatten)]
    output: OutputArgs,
}

/// A patch as RFC 6902 JSON text.
fn patch_text(operations: &[Delta], compact: bool) -> Result<String, Box<dyn std::error::Error>> {
    Ok(if compact {
        serde_json::to_string(operations)?
    } else {
        serde_json::to_string_pretty(operations)?
    })
}

fn write_output(
    text: String,
    destination: &Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    // A document edited in place already ends with its own newline; adding
    // another would grow the file by a blank line on every run.
    let text = if text.ends_with('\n') { text } else { format!("{text}\n") };
    match destination {
        Some(path) if path != "-" => fs::write(path, text)?,
        _ => print!("{text}"),
    }
    Ok(())
}

/// The sign `--pretty` prints for an operation.
fn sign(op: &Operation) -> &'static str {
    match op {
        Operation::Add => "+",
        Operation::Remove => "-",
        Operation::Replace => "~",
        Operation::Move => "m",
        Operation::Copy => "c",
        Operation::Test => "?",
    }
}

/// How `drift diff` prints its operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Style {
    /// An RFC 6902 JSON array.
    Json { compact: bool },
    /// One `+ /path` line per operation (`--pretty`).
    Summary,
    /// Only the counts (`--stats`).
    Stats { compact: bool },
}

impl Style {
    fn of(args: &DiffArgs) -> Self {
        let compact = args.output.compact;
        if args.pretty {
            Style::Summary
        } else if args.stats {
            Style::Stats { compact }
        } else {
            Style::Json { compact }
        }
    }
}

/// Writes operations as they arrive, so a large diff is never held whole. The
/// bytes are those the collected output always had.
struct OperationWriter {
    out: Box<dyn io::Write>,
    style: Style,
    count: usize,
    by_op: std::collections::BTreeMap<&'static str, u64>,
    /// When writing to an existing or new regular file, the file the operations go
    /// to meanwhile and the one it replaces when all is written. A diff that fails
    /// leaves the destination as it was.
    pending: Option<(PathBuf, PathBuf)>,
}

impl OperationWriter {
    /// Writes to a file, or to standard output for `-` or no destination.
    fn to(destination: &Option<String>, style: Style) -> io::Result<Self> {
        let mut pending = None;
        let out: Box<dyn io::Write> = match destination {
            Some(path) if path != "-" => {
                let (file, replaces) = create_beside(Path::new(path))?;
                pending = replaces;
                Box::new(io::BufWriter::new(file))
            }
            _ => Box::new(io::BufWriter::new(io::stdout())),
        };
        Ok(Self { out, style, count: 0, by_op: Default::default(), pending })
    }

    fn push(&mut self, operation: &Delta) -> io::Result<()> {
        let first = self.count == 0;
        match self.style {
            Style::Summary => {
                writeln!(self.out, "{} {}", sign(&operation.op), operation.path)?;
            }
            Style::Stats { .. } => {
                *self.by_op.entry(operation.op.as_str()).or_insert(0) += 1;
            }
            Style::Json { compact: true } => {
                write!(self.out, "{}", if first { "[" } else { "," })?;
                serde_json::to_writer(&mut self.out, operation)?;
            }
            Style::Json { compact: false } => {
                write!(self.out, "{}", if first { "[\n" } else { ",\n" })?;
                // Each element indented to sit inside the array.
                let element = serde_json::to_string_pretty(operation)?;
                let indented: Vec<String> = element.lines().map(|l| format!("  {l}")).collect();
                write!(self.out, "{}", indented.join("\n"))?;
            }
        }
        self.count += 1;
        Ok(())
    }

    /// Ends the output and returns how many operations were written.
    fn finish(mut self) -> io::Result<usize> {
        match self.style {
            Style::Summary if self.count == 0 => writeln!(self.out)?,
            Style::Summary => {}
            Style::Stats { compact } => {
                let by_op: serde_json::Map<String, Value> =
                    self.by_op.iter().map(|(k, v)| (k.to_string(), Value::from(*v))).collect();
                let payload = serde_json::json!({"total": self.count, "by_op": by_op});
                let text = if compact {
                    serde_json::to_string(&payload)?
                } else {
                    serde_json::to_string_pretty(&payload)?
                };
                writeln!(self.out, "{text}")?;
            }
            Style::Json { .. } if self.count == 0 => writeln!(self.out, "[]")?,
            Style::Json { compact: true } => writeln!(self.out, "]")?,
            Style::Json { compact: false } => writeln!(self.out, "\n]")?,
        }
        self.out.flush()?;
        if let Some((temp, target)) = self.pending.take() {
            // Close the file before it is renamed.
            self.out = Box::new(io::sink());
            fs::rename(&temp, &target).inspect_err(|_| {
                let _ = fs::remove_file(&temp);
            })?;
        }
        Ok(self.count)
    }
}

impl Drop for OperationWriter {
    /// A writer dropped before `finish` (the diff failed) removes its file.
    fn drop(&mut self) {
        if let Some((temp, _)) = self.pending.take() {
            self.out = Box::new(io::sink());
            let _ = fs::remove_file(temp);
        }
    }
}

/// A file to write the output to. For a regular file, or a path that does not exist
/// yet, that is a new file in the same directory and the path to rename it to when
/// the output is complete, so a failure does not destroy what was there. Anything
/// else (a device such as /dev/null, a pipe) is written to directly.
fn create_beside(path: &Path) -> io::Result<(fs::File, Option<(PathBuf, PathBuf)>)> {
    let target = match fs::metadata(path) {
        Ok(meta) if meta.is_file() => Some((fs::canonicalize(path)?, Some(meta.permissions()))),
        Ok(_) => None,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Some((path.to_path_buf(), None)),
        Err(e) => return Err(e),
    };
    let Some((target, permissions)) = target else {
        return Ok((fs::File::create(path)?, None));
    };
    let name = target.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let temp = target.with_file_name(format!(".{name}.drift-{}.tmp", std::process::id()));
    let file = fs::File::create(&temp)?;
    if let Some(permissions) = permissions {
        fs::set_permissions(&temp, permissions)?;
    }
    Ok((file, Some((temp, target))))
}

/// What to do about repeated row keys in NDJSON files.
#[derive(Clone, Copy, clap::ValueEnum)]
enum DuplicatesArg {
    /// Refuse, naming both lines
    Error,
    /// Keep the first row with a key and ignore the others
    First,
    /// Keep the last row with a key and ignore the others
    Last,
    /// Match the nth row with a key to the nth row with that key
    Occurrence,
}

impl From<DuplicatesArg> for drift::Duplicates {
    fn from(arg: DuplicatesArg) -> Self {
        match arg {
            DuplicatesArg::Error => Self::Error,
            DuplicatesArg::First => Self::First,
            DuplicatesArg::Last => Self::Last,
            DuplicatesArg::Occurrence => Self::Occurrence,
        }
    }
}

/// The kinds of record file `drift diff` can stream.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RecordsKind {
    /// One JSON value per line.
    Ndjson,
    /// Delimited text with a header; the byte is the usual delimiter for the extension.
    Csv(u8),
}

/// The kind of record file a path is, from its extension.
fn records_kind(path: &str) -> Option<RecordsKind> {
    let extension = Path::new(path).extension().and_then(|e| e.to_str())?.to_ascii_lowercase();
    match extension.as_str() {
        "ndjson" | "jsonl" => Some(RecordsKind::Ndjson),
        "csv" => Some(RecordsKind::Csv(b',')),
        "tsv" => Some(RecordsKind::Csv(b'\t')),
        _ => None,
    }
}

/// `--memory-limit`: a number of bytes, optionally with a K, M or G suffix.
fn parse_size(text: &str) -> Result<usize, Box<dyn std::error::Error>> {
    let wrong = || format!("--memory-limit must be a size such as 512M, 2G or 65536, not `{text}`");
    let lower = text.trim().to_ascii_lowercase();
    let digits = lower.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let unit: u64 = match &lower[digits.len()..] {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        _ => return Err(wrong().into()),
    };
    let number: u64 = digits.parse().map_err(|_| wrong())?;
    let bytes = number.checked_mul(unit).ok_or_else(wrong)?;
    if bytes == 0 {
        return Err(wrong().into());
    }
    Ok(usize::try_from(bytes).unwrap_or(usize::MAX))
}

/// `--delimiter`: one character, or a word for the common ones.
fn parse_delimiter(text: &str) -> Result<u8, Box<dyn std::error::Error>> {
    match text {
        "tab" => return Ok(b'\t'),
        "comma" => return Ok(b','),
        "semicolon" => return Ok(b';'),
        "pipe" => return Ok(b'|'),
        _ => {}
    }
    match text.as_bytes() {
        [byte] if byte.is_ascii() => Ok(*byte),
        _ => Err("--delimiter must be one character (or tab, comma, semicolon or pipe)".into()),
    }
}

/// `--types`: `column:type` pairs, comma-separated or repeated.
fn parse_types(
    specs: &[String],
) -> Result<Vec<(String, drift::ColumnType)>, Box<dyn std::error::Error>> {
    let mut columns = Vec::new();
    for pair in specs.iter().flat_map(|s| s.split(',')).map(str::trim).filter(|p| !p.is_empty()) {
        let (name, kind) = pair
            .rsplit_once(':')
            .ok_or_else(|| format!("--types expects COLUMN:TYPE pairs, not `{pair}`"))?;
        let kind = match kind.to_ascii_lowercase().as_str() {
            "string" => drift::ColumnType::String,
            "int" => drift::ColumnType::Int,
            "float" => drift::ColumnType::Float,
            "bool" => drift::ColumnType::Bool,
            "json" => drift::ColumnType::Json,
            other => {
                return Err(format!(
                    "unknown column type `{other}` in --types: use string, int, float, bool or json"
                )
                .into())
            }
        };
        columns.push((name.to_string(), kind));
    }
    Ok(columns)
}

/// How the files of this diff are compared as records, or `None` if they are
/// ordinary documents. Records need a key, and cannot come from stdin or be grepped.
fn records_inputs(
    args: &DiffArgs,
) -> Result<Option<drift::RecordFormat>, Box<dyn std::error::Error>> {
    let (old, new) = (records_kind(&args.old), records_kind(&args.new));
    let csv_only = !args.types.is_empty() || args.delimiter.is_some();
    if old.is_none() && new.is_none() {
        if args.keyless || args.duplicates.is_some() {
            return Err("--keyless and --duplicates apply only to record files (NDJSON, CSV or TSV: .ndjson, .jsonl, .csv or .tsv)".into());
        }
        if csv_only {
            return Err("--types and --delimiter apply only to CSV and TSV files".into());
        }
        if args.memory_limit.is_some() || args.temp_dir.is_some() {
            return Err("--memory-limit and --temp-dir apply only to record files (NDJSON, CSV or TSV: .ndjson, .jsonl, .csv or .tsv)".into());
        }
        return Ok(None);
    }
    if args.old == "-" || args.new == "-" {
        return Err("record files are streamed from disk, so they cannot be read from stdin".into());
    }
    let (Some(old), Some(new)) = (old, new) else {
        return Err(
            "to compare records, both inputs must be files of the same kind (NDJSON, CSV or TSV)"
                .into(),
        );
    };
    let same_kind = matches!(
        (old, new),
        (RecordsKind::Ndjson, RecordsKind::Ndjson) | (RecordsKind::Csv(_), RecordsKind::Csv(_))
    );
    if !same_kind {
        return Err(
            "to compare records, both inputs must be files of the same kind (NDJSON, CSV or TSV)"
                .into(),
        );
    }
    if args.array_keys.is_empty() && !args.keyless {
        return Err(
            "record files need --array-key FIELD, the field that identifies a row, or --keyless"
                .into(),
        );
    }
    if !args.values.is_empty() {
        return Err("--grep is not available for record files, which are not loaded whole".into());
    }
    match (old, new) {
        (RecordsKind::Csv(a), RecordsKind::Csv(b)) => {
            let delimiter =
                match &args.delimiter {
                    Some(text) => parse_delimiter(text)?,
                    None if a == b => a,
                    None => return Err(
                        "a .csv and a .tsv file are split differently: say how with --delimiter"
                            .into(),
                    ),
                };
            let mut options = drift::CsvOptions::new().delimiter(delimiter);
            for (name, kind) in parse_types(&args.types)? {
                options = options.column(name, kind);
            }
            Ok(Some(drift::RecordFormat::Csv(options)))
        }
        _ => {
            if csv_only {
                return Err("--types and --delimiter apply only to CSV and TSV files".into());
            }
            Ok(Some(drift::RecordFormat::Ndjson))
        }
    }
}

fn cmd_diff(args: DiffArgs) -> Result<i32, Box<dyn std::error::Error>> {
    // A pattern that is not a JSON Pointer would silently match nothing.
    for pattern in &args.ignore_paths {
        drift::split_pointer(pattern).map_err(|_| {
            format!("invalid --ignore-path `{pattern}`: it must be empty or a JSON Pointer starting with `/`")
        })?;
    }
    let format = resolve(args.format, &args.old);
    let records = records_inputs(&args)?;

    // `--grep` inspects old values, so it needs the whole old document in memory.
    let delegate = matches!(format, drift::formats::Format::Json)
        && args.old != "-"
        && args.new != "-"
        && args.values.is_empty()
        && args.array_keys.is_empty()
        && args.ignore_paths.is_empty();

    let style = Style::of(&args);
    let kept = if let Some(record_format) = records {
        // One JSON object per line: matched by key and streamed, never loaded whole,
        // and the operations are written as they are found.
        // The first --array-key names the row key (several fields, comma-separated,
        // make a composite key); any others match arrays inside rows.
        let (key_fields, inner_keys) = if args.keyless {
            (Vec::new(), &args.array_keys[..])
        } else {
            let fields = args.array_keys[0].split(',').map(str::trim).filter(|f| !f.is_empty());
            (fields.map(String::from).collect::<Vec<_>>(), &args.array_keys[1..])
        };
        let diff = inner_keys.iter().fold(DiffOptions::new(), |o, key| o.array_key(key));
        let diff = args.ignore_paths.iter().fold(diff, |o, pattern| o.ignore_path(pattern));
        let mut options = drift::RecordOptions::new().diff(diff).format(record_format);
        options = key_fields.into_iter().fold(options, |o, field| o.key(field));
        if args.keyless {
            options = options.whole_row();
        }
        if let Some(policy) = args.duplicates {
            options = options.duplicates(policy.into());
        }
        if let Some(limit) = &args.memory_limit {
            options = options.memory_limit(parse_size(limit)?);
        }
        if let Some(dir) = &args.temp_dir {
            if args.memory_limit.is_none() {
                return Err(
                    "--temp-dir is for --memory-limit, which sorts through temporary files".into(),
                );
            }
            options = options.temp_dir(dir);
        }
        let filter = drift::OperationFilter::new(
            &args.paths,
            &args.fields,
            &args.values,
            &args.operations,
            args.invert_match,
        )?;
        let mut writer = OperationWriter::to(&args.output.output, style)?;
        drift::diff_records_each(Path::new(&args.old), Path::new(&args.new), &options, |op| {
            if filter.keep(&op, None) {
                writer.push(&op).map_err(|e| drift::DriftError::Io(e.to_string()))?;
            }
            Ok(())
        })?;
        writer.finish()?
    } else {
        let (diff_operations, old) = if delegate {
            (diff_files(Path::new(&args.old), Path::new(&args.new))?, None)
        } else {
            let old = load(&args.old, format, args.xml_arrays)?.0;
            let new = load(&args.new, format, args.xml_arrays)?.0;
            let options =
                args.array_keys.iter().fold(DiffOptions::new(), |o, key| o.array_key(key));
            let options =
                args.ignore_paths.iter().fold(options, |o, pattern| o.ignore_path(pattern));
            (diff_with(&new, &old, &options), Some(old))
        };
        let operations = filter_operations(
            &diff_operations,
            old.as_ref(),
            &args.paths,
            &args.fields,
            &args.values,
            &args.operations,
            args.invert_match,
        )?;
        let mut writer = OperationWriter::to(&args.output.output, style)?;
        for operation in &operations {
            writer.push(operation)?;
        }
        writer.finish()?
    };
    Ok(if args.exit_code && kept > 0 { 1 } else { 0 })
}

fn read_patch(path: &str) -> Result<Vec<Delta>, Box<dyn std::error::Error>> {
    let mut text = String::new();
    if path == "-" {
        io::stdin().read_to_string(&mut text)?;
    } else {
        text = fs::read_to_string(path)?;
    }
    Ok(serde_json::from_str(&text)?)
}

fn cmd_patch(args: PatchArgs) -> Result<i32, Box<dyn std::error::Error>> {
    let format = resolve(args.format, &args.document);
    // Keep the original text: TOML is edited in place so its comments survive.
    let original = read(&args.document)?;
    let options = drift::formats::ParseOptions { xml_arrays: args.xml_arrays };
    let (document, _) = drift::formats::parse_with(&original, format, &options)?;
    let result = patch(document, &read_patch(&args.patch)?)?;
    let destination = if args.in_place { Some(args.document) } else { args.output.output };
    let text = drift::formats::dump_like(&original, &result, format, args.output.compact)?;
    write_output(text, &destination)?;
    Ok(0)
}

fn cmd_paths(args: PathsArgs) -> Result<i32, Box<dyn std::error::Error>> {
    let document = load(&args.document, resolve(args.format, &args.document), args.xml_arrays)?.0;
    let paths = drift::list_json_paths(
        &document,
        args.include_root,
        args.containers,
        args.sort_keys,
        args.max_depth,
    );
    if args.json || args.output.output.is_some() {
        write_output(serde_json::to_string_pretty(&paths)?, &args.output.output)?;
    } else {
        write_output(paths.join("\n"), &None)?;
    }
    Ok(0)
}

fn cmd_check(args: CheckArgs) -> Result<i32, Box<dyn std::error::Error>> {
    let format = resolve(args.format, &args.old);
    let old = load(&args.old, format, args.xml_arrays)?.0;
    let new = load(&args.new, format, args.xml_arrays)?.0;
    let operations = diff(&new, &old);
    let ok = patch(old, &operations)? == new;
    write_output(
        serde_json::to_string(
            &serde_json::json!({"roundtrip": ok, "operations": operations.len()}),
        )?,
        &args.output.output,
    )?;
    Ok(if ok { 0 } else { 1 })
}

fn cmd_invert(args: InvertArgs) -> Result<i32, Box<dyn std::error::Error>> {
    let format = resolve(args.format, &args.document);
    let (document, _) = load(&args.document, format, args.xml_arrays)?;
    let undo = invert(&document, &read_patch(&args.patch)?)?;
    write_output(patch_text(&undo, args.output.compact)?, &args.output.output)?;
    Ok(0)
}

fn cmd_compose(args: ComposeArgs) -> Result<i32, Box<dyn std::error::Error>> {
    let format = resolve(args.format, &args.document);
    let (document, _) = load(&args.document, format, args.xml_arrays)?;
    let mut operations = Vec::new();
    for patch_path in &args.patches {
        operations.extend(read_patch(patch_path)?);
    }
    let options = args.array_keys.iter().fold(DiffOptions::new(), |o, key| o.array_key(key));
    let squashed = compose(&document, &operations, &options)?;
    write_output(patch_text(&squashed, args.output.compact)?, &args.output.output)?;
    Ok(0)
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let command = Cli::parse().command;
    let code = match command {
        Command::Diff(args) => cmd_diff(*args)?,
        Command::Patch(args) => cmd_patch(args)?,
        Command::Paths(args) => cmd_paths(args)?,
        Command::Check(args) => cmd_check(args)?,
        Command::Invert(args) => cmd_invert(args)?,
        Command::Compose(args) => cmd_compose(args)?,
    };
    std::process::exit(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_list_operation_types_alphabetically_whatever_the_order_they_arrive_in() {
        let dir = Scratch::new("stats-order");
        let path = dir.path("stats.json");
        let mut writer =
            OperationWriter::to(&Some(path.clone()), Style::Stats { compact: true }).unwrap();
        for op in [
            Delta::new(Operation::Replace, "/a"),
            Delta::new(Operation::Remove, "/b"),
            Delta::new(Operation::Add, "/c"),
            Delta::new(Operation::Remove, "/d"),
        ] {
            writer.push(&op).unwrap();
        }
        writer.finish().unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{\"total\":4,\"by_op\":{\"add\":1,\"remove\":2,\"replace\":1}}\n"
        );
    }

    fn run_patch(document: &std::path::Path, patch_text: &str) {
        let patch_file = document.with_extension("patch.json");
        fs::write(&patch_file, patch_text).unwrap();
        let cli = Cli::try_parse_from([
            "drift",
            "patch",
            document.to_str().unwrap(),
            patch_file.to_str().unwrap(),
            "--in-place",
        ])
        .unwrap();
        let Command::Patch(args) = cli.command else { panic!("not a patch command") };
        assert_eq!(cmd_patch(args).unwrap(), 0);
    }

    #[test]
    fn patching_a_toml_file_in_place_keeps_its_comments_and_does_not_grow() {
        let dir = std::env::temp_dir().join(format!("drift-cli-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.toml");
        let original = "# service\nname = \"x\"  # who\nport = 80\n\n[db]\nurl = 'u'  # where\n";
        fs::write(&file, original).unwrap();

        run_patch(&file, r#"[{"op": "replace", "path": "/port", "value": 81}]"#);
        let patched = fs::read_to_string(&file).unwrap();
        assert_eq!(patched, original.replace("port = 80", "port = 81"));

        // A patch that changes nothing must leave the file exactly as it is: the
        // trailing newline is not added a second time on every run.
        run_patch(&file, r#"[{"op": "test", "path": "/port", "value": 81}]"#);
        assert_eq!(fs::read_to_string(&file).unwrap(), patched);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn patching_a_yaml_file_in_place_keeps_its_comments_and_does_not_grow() {
        let dir = std::env::temp_dir().join(format!("drift-cli-yaml-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.yaml");
        let original = "# service\nname: x   # who\nreplicas: 2\n\nimage:\n  tag: v1   # pinned\n";
        fs::write(&file, original).unwrap();

        run_patch(&file, r#"[{"op": "replace", "path": "/image/tag", "value": "v2"}]"#);
        let patched = fs::read_to_string(&file).unwrap();
        assert_eq!(patched, original.replace("tag: v1", "tag: v2"));

        run_patch(&file, r#"[{"op": "test", "path": "/replicas", "value": 2}]"#);
        assert_eq!(fs::read_to_string(&file).unwrap(), patched);
        fs::remove_dir_all(&dir).ok();
    }

    /// A scratch directory that is removed afterwards.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("drift-{name}-{}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn file(&self, name: &str, content: &str) -> String {
            let path = self.0.join(name);
            fs::write(&path, content).unwrap();
            path.to_str().unwrap().to_string()
        }
        fn path(&self, name: &str) -> String {
            self.0.join(name).to_str().unwrap().to_string()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    fn run_cli(args: &[&str]) -> Result<i32, Box<dyn std::error::Error>> {
        match Cli::try_parse_from(args)?.command {
            Command::Diff(args) => cmd_diff(*args),
            Command::Patch(args) => cmd_patch(args),
            Command::Invert(args) => cmd_invert(args),
            Command::Compose(args) => cmd_compose(args),
            _ => panic!("not a command under test"),
        }
    }

    fn read_ops(path: &str) -> Vec<Delta> {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    fn applied(document: &str, ops: &[Delta]) -> Value {
        patch(serde_json::from_str(document).unwrap(), ops).unwrap()
    }

    #[test]
    fn invert_prints_the_patch_that_undoes_another() {
        let dir = Scratch::new("invert");
        let document = r#"{"a": 1, "list": [1, 2, 3], "o": {"k": "v"}}"#;
        let doc = dir.file("doc.json", document);
        let forward = dir.file(
            "p.json",
            r#"[{"op": "replace", "path": "/a", "value": 9},
                {"op": "remove", "path": "/list/0"},
                {"op": "add", "path": "/list/-", "value": 4},
                {"op": "remove", "path": "/o/k"}]"#,
        );
        let undo_file = dir.path("undo.json");
        assert_eq!(run_cli(&["drift", "invert", &doc, &forward, "-o", &undo_file]).unwrap(), 0);

        let undo = read_ops(&undo_file);
        let after = applied(document, &read_ops(&forward));
        assert_eq!(patch(after, &undo).unwrap(), serde_json::from_str::<Value>(document).unwrap());
        // It is a plain RFC 6902 patch with the undo of the last operation first.
        let paths: Vec<_> = undo.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(paths, ["/o/k", "/list/2", "/list/0", "/a"]);
    }

    #[test]
    fn invert_reads_the_document_in_any_format() {
        let dir = Scratch::new("invert-yaml");
        let doc = dir.file("doc.yaml", "name: app   # keep\nreplicas: 2\n");
        let forward = dir.file("p.json", r#"[{"op": "replace", "path": "/replicas", "value": 5}]"#);
        let undo_file = dir.path("undo.json");
        run_cli(&["drift", "invert", &doc, &forward, "-o", &undo_file]).unwrap();
        assert_eq!(
            serde_json::to_value(read_ops(&undo_file)).unwrap(),
            serde_json::json!([{"op": "replace", "path": "/replicas", "value": 2}])
        );
    }

    #[test]
    fn invert_fails_cleanly_for_a_patch_that_does_not_apply() {
        let dir = Scratch::new("invert-bad");
        let doc = dir.file("doc.json", r#"{"a": 1}"#);
        let forward = dir.file("p.json", r#"[{"op": "remove", "path": "/missing"}]"#);
        assert!(run_cli(&["drift", "invert", &doc, &forward]).is_err());
    }

    #[test]
    fn compose_squashes_several_patches_into_one() {
        let dir = Scratch::new("compose");
        let document = r#"{"a": 1, "list": [1, 2, 3]}"#;
        let doc = dir.file("doc.json", document);
        let first = dir.file("1.json", r#"[{"op": "replace", "path": "/a", "value": 2}]"#);
        let second = dir.file(
            "2.json",
            r#"[{"op": "replace", "path": "/a", "value": 3}, {"op": "add", "path": "/tmp", "value": 1}]"#,
        );
        let third = dir.file(
            "3.json",
            r#"[{"op": "remove", "path": "/tmp"}, {"op": "remove", "path": "/list/2"}]"#,
        );
        let squashed_file = dir.path("squashed.json");
        assert_eq!(
            run_cli(&["drift", "compose", &doc, &first, &second, &third, "-o", &squashed_file])
                .unwrap(),
            0
        );

        let squashed = read_ops(&squashed_file);
        let sequential = [first, second, third]
            .iter()
            .fold(serde_json::from_str::<Value>(document).unwrap(), |doc, file| {
                patch(doc, &read_ops(file)).unwrap()
            });
        assert_eq!(patch(serde_json::from_str(document).unwrap(), &squashed).unwrap(), sequential);
        assert_eq!(
            serde_json::to_value(&squashed).unwrap(),
            serde_json::json!([
                {"op": "replace", "path": "/a", "value": 3},
                {"op": "remove", "path": "/list/2"},
            ])
        );
    }

    #[test]
    fn compose_of_a_patch_and_its_undo_is_empty() {
        let dir = Scratch::new("compose-undo");
        let doc = dir.file("doc.json", r#"{"a": [1, 2], "b": 1}"#);
        let forward = dir.file(
            "p.json",
            r#"[{"op": "remove", "path": "/a/0"}, {"op": "replace", "path": "/b", "value": 7}]"#,
        );
        let undo = dir.path("undo.json");
        run_cli(&["drift", "invert", &doc, &forward, "-o", &undo]).unwrap();
        let out = dir.path("out.json");
        run_cli(&["drift", "compose", &doc, &forward, &undo, "-o", &out]).unwrap();
        assert!(read_ops(&out).is_empty());
    }

    #[test]
    fn compose_needs_a_document_and_at_least_one_patch() {
        assert!(Cli::try_parse_from(["drift", "compose", "doc.json"]).is_err());
        assert!(Cli::try_parse_from(["drift", "invert", "doc.json"]).is_err());
    }

    fn rows(range: std::ops::Range<u64>, edit: impl Fn(u64) -> u64) -> String {
        range.map(|i| format!("{{\"id\":{i},\"v\":{},\"stamp\":{i}}}\n", edit(i))).collect()
    }

    #[test]
    fn diffing_ndjson_files_streams_them_and_matches_rows_by_the_first_array_key() {
        let dir = Scratch::new("ndjson");
        let old = dir.file("old.ndjson", &rows(1..101, |i| i));
        // A new first row, one changed row, and the last row dropped.
        let new_text = format!(
            "{{\"id\":0,\"v\":0,\"stamp\":0}}\n{}",
            rows(1..100, |i| if i == 50 { 999 } else { i })
        );
        let new = dir.file("new.ndjson", &new_text);
        let out = dir.path("ops.json");
        assert_eq!(
            run_cli(&["drift", "diff", &old, &new, "--array-key", "id", "--compact", "-o", &out])
                .unwrap(),
            0
        );

        let ops = read_ops(&out);
        let paths: Vec<_> = ops.iter().map(|d| (d.op.as_str(), d.path.as_str())).collect();
        // Remove, add and replace: and no run of replacements for the row inserted at the front.
        assert_eq!(paths, [("remove", "/99"), ("add", "/0"), ("replace", "/50/v")]);
    }

    #[test]
    fn ndjson_diffs_honour_ignore_path_and_the_other_filters() {
        let dir = Scratch::new("ndjson-ignore");
        let old = dir.file("old.jsonl", &rows(0..5, |i| i));
        let new = dir.file(
            "new.JSONL",
            &(0..5)
                .map(|i| format!("{{\"id\":{i},\"v\":{i},\"stamp\":{}}}\n", i + 100))
                .collect::<String>(),
        );
        let out = dir.path("ops.json");
        // Only the ignored field differs, so nothing is reported.
        run_cli(&[
            "drift",
            "diff",
            &old,
            &new,
            "--array-key",
            "id",
            "--ignore-path",
            "/*/stamp",
            "-o",
            &out,
        ])
        .unwrap();
        assert!(read_ops(&out).is_empty());
        // Without ignoring it, every row differs and --op/--path still filter the result.
        run_cli(&[
            "drift",
            "diff",
            &old,
            &new,
            "--array-key",
            "id",
            "--compact",
            "--path",
            "/2/**",
            "-o",
            &out,
        ])
        .unwrap();
        assert_eq!(read_ops(&out).len(), 1);
    }

    #[test]
    fn ndjson_diffs_say_what_they_need() {
        let dir = Scratch::new("ndjson-errors");
        let old = dir.file("old.ndjson", &rows(0..3, |i| i));
        let new = dir.file("new.ndjson", &rows(0..3, |i| i + 1));
        let message = |args: &[&str]| run_cli(args).unwrap_err().to_string();
        assert!(message(&["drift", "diff", &old, &new]).contains("--array-key"));
        assert!(message(&["drift", "diff", &old, &new, "--array-key", "id", "--grep", "x"])
            .contains("--grep"));
        assert!(message(&["drift", "diff", "-", &new, "--array-key", "id"]).contains("stdin"));
        // Mixing an NDJSON file with another format is a mistake worth a message.
        let json = dir.file("doc.json", "[]");
        assert!(message(&["drift", "diff", &old, &json, "--array-key", "id"]).contains("both"));
    }

    fn ndjson_ops(args: &[&str], out: &str) -> Vec<(String, String)> {
        let mut full = vec!["drift", "diff", "--compact", "-o", out];
        full.extend_from_slice(args);
        run_cli(&full).unwrap();
        read_ops(out).into_iter().map(|d| (d.op.as_str().to_string(), d.path)).collect()
    }

    #[test]
    fn a_comma_separated_array_key_is_a_composite_row_key() {
        let dir = Scratch::new("ndjson-composite");
        let old = dir.file(
            "old.ndjson",
            "{\"id\":1,\"region\":\"eu\",\"v\":1}\n{\"id\":1,\"region\":\"us\",\"v\":2}\n",
        );
        let new = dir.file(
            "new.ndjson",
            "{\"id\":1,\"region\":\"us\",\"v\":9}\n{\"id\":1,\"region\":\"eu\",\"v\":1}\n",
        );
        let out = dir.path("ops.json");
        // Matched by (id, region): a move and a change, not a confusion of the two rows.
        let ops = ndjson_ops(&[&old, &new, "--array-key", "id,region"], &out);
        assert_eq!(ops, [("move".into(), "/0".into()), ("replace".into(), "/0/v".into())]);
        // By id alone the key repeats, which is refused.
        assert!(run_cli(&["drift", "diff", &old, &new, "--array-key", "id"])
            .unwrap_err()
            .to_string()
            .contains("duplicate"));
    }

    #[test]
    fn the_row_key_can_be_a_json_pointer() {
        let dir = Scratch::new("ndjson-pointer");
        let old = dir
            .file("old.ndjson", "{\"user\":{\"id\":1},\"v\":1}\n{\"user\":{\"id\":2},\"v\":2}\n");
        let new = dir
            .file("new.ndjson", "{\"user\":{\"id\":2},\"v\":2}\n{\"user\":{\"id\":1},\"v\":7}\n");
        let ops = ndjson_ops(&[&old, &new, "--array-key", "/user/id"], &dir.path("ops.json"));
        assert_eq!(ops, [("move".into(), "/0".into()), ("replace".into(), "/1/v".into())]);
    }

    #[test]
    fn duplicates_can_be_resolved_instead_of_refused() {
        let dir = Scratch::new("ndjson-dups");
        let old = dir.file("old.ndjson", "{\"id\":1,\"v\":\"a\"}\n{\"id\":1,\"v\":\"b\"}\n");
        let new = dir.file("new.ndjson", "{\"id\":1,\"v\":\"a\"}\n{\"id\":1,\"v\":\"c\"}\n");
        let out = dir.path("ops.json");
        assert!(run_cli(&["drift", "diff", &old, &new, "--array-key", "id"]).is_err());
        // First keeps row 1 of each (no change); last keeps the second (b becomes c).
        assert!(ndjson_ops(&[&old, &new, "--array-key", "id", "--duplicates", "first"], &out)
            .is_empty());
        assert_eq!(
            ndjson_ops(&[&old, &new, "--array-key", "id", "--duplicates", "last"], &out),
            [("replace".to_string(), "/0/v".to_string())]
        );
        // Occurrence pairs the nth row of a key with the nth: only the second changed.
        assert_eq!(
            ndjson_ops(&[&old, &new, "--array-key", "id", "--duplicates", "occurrence"], &out),
            [("replace".to_string(), "/1/v".to_string())]
        );
        assert!(Cli::try_parse_from([
            "drift",
            "diff",
            "a.ndjson",
            "b.ndjson",
            "--duplicates",
            "sometimes"
        ])
        .is_err());
    }

    #[test]
    fn a_diff_that_fails_leaves_the_output_file_as_it_was_and_one_that_works_replaces_it() {
        let dir = Scratch::new("output-kept");
        let good = dir.file("good.ndjson", "{\"id\":1,\"v\":1}\n{\"id\":2,\"v\":2}\n");
        let changed = dir.file("changed.ndjson", "{\"id\":1,\"v\":9}\n{\"id\":2,\"v\":2}\n");
        let repeated = dir.file("repeated.ndjson", "{\"id\":1}\n{\"id\":1}\n");
        let out = dir.path("out.json");
        std::fs::write(&out, "PREVIOUS").unwrap();

        let failed = run_cli(&[
            "drift",
            "diff",
            "--compact",
            "-o",
            &out,
            &good,
            &repeated,
            "--array-key",
            "id",
        ]);
        assert!(failed.is_err());
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "PREVIOUS");
        // A failure after some output has been written, too.
        let late = dir.file("late.ndjson", "{\"id\":1,\"v\":9}\n{\"id\":2,\"v\":3}\nnot json\n");
        assert!(run_cli(&[
            "drift",
            "diff",
            "--compact",
            "-o",
            &out,
            &good,
            &late,
            "--array-key",
            "id"
        ])
        .is_err());
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "PREVIOUS");

        run_cli(&["drift", "diff", "--compact", "-o", &out, &good, &changed, "--array-key", "id"])
            .unwrap();
        assert!(std::fs::read_to_string(&out).unwrap().starts_with("[{\"op\":\"replace\""));
        // A path that did not exist is created only by a diff that works.
        let fresh = dir.path("fresh.json");
        assert!(run_cli(&["drift", "diff", "-o", &fresh, &good, &repeated, "--array-key", "id"])
            .is_err());
        assert!(!std::path::Path::new(&fresh).exists());
        // And nothing is left beside the output.
        let mut names: Vec<_> = std::fs::read_dir(dir.path(""))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert!(names.iter().all(|n| !n.contains(".tmp")), "{names:?}");
    }

    #[test]
    fn sizes_are_bytes_or_have_a_unit() {
        assert_eq!(parse_size("65536").unwrap(), 65536);
        assert_eq!(parse_size("64k").unwrap(), 64 << 10);
        assert_eq!(parse_size("512M").unwrap(), 512 << 20);
        assert_eq!(parse_size("2GiB").unwrap(), 2 << 30);
        for wrong in ["", "M", "0", "-1", "1.5G", "12X", "99999999999999999999G"] {
            assert!(parse_size(wrong).is_err(), "{wrong}");
        }
    }

    #[test]
    fn a_memory_limit_gives_the_same_operations_and_leaves_no_files() {
        let dir = Scratch::new("ndjson-limit");
        let rows = |changed: bool| -> String {
            (0..300)
                .filter(|i| i % 9 != 0)
                .map(|i| {
                    format!("{{\"id\":{i},\"v\":{}}}\n", if changed && i % 4 == 0 { 1 } else { 0 })
                })
                .collect()
        };
        let (old, new) =
            (dir.file("old.ndjson", &rows(false)), dir.file("new.ndjson", &rows(true)));
        let plain = ndjson_ops(&[&old, &new, "--array-key", "id"], &dir.path("a.json"));
        assert!(plain.len() > 20);
        let temp = dir.path("tmp");
        std::fs::create_dir_all(&temp).unwrap();
        let limited = ndjson_ops(
            &[&old, &new, "--array-key", "id", "--memory-limit", "2k", "--temp-dir", &temp],
            &dir.path("b.json"),
        );
        assert_eq!(limited, plain);
        assert_eq!(std::fs::read_dir(&temp).unwrap().count(), 0);
    }

    #[test]
    fn memory_limit_and_temp_dir_are_refused_where_they_do_nothing() {
        let dir = Scratch::new("limit-refused");
        let ndjson = dir.file("a.ndjson", "{\"id\":1}\n");
        let json = dir.file("a.json", "{\"a\":1}");
        let run = |args: &[&str]| {
            let mut full = vec!["drift", "diff", "-o", "/dev/null"];
            full.extend_from_slice(args);
            run_cli(&full).unwrap_err().to_string()
        };
        assert!(run(&[&json, &json, "--memory-limit", "1M"]).contains("apply only to record files"));
        assert!(run(&[&ndjson, &ndjson, "--array-key", "id", "--temp-dir", "/tmp"])
            .contains("--temp-dir is for --memory-limit"));
        assert!(run(&[&ndjson, &ndjson, "--array-key", "id", "--memory-limit", "lots"])
            .contains("--memory-limit must be a size"));
    }

    #[test]
    fn keyless_matches_whole_rows_and_needs_no_array_key() {
        let dir = Scratch::new("ndjson-keyless");
        let old = dir.file("old.ndjson", "{\"a\":1}\n{\"a\":2}\n{\"a\":2}\n");
        let new = dir.file("new.ndjson", "{\"a\":2}\n{\"a\":1}\n{\"a\":9}\n");
        let ops = ndjson_ops(&[&old, &new, "--keyless"], &dir.path("ops.json"));
        // One of the two identical rows {"a":2} is gone, {"a":9} is new, and the
        // surviving rows were reordered: removals, a move and an addition, no edits.
        assert!(ops.iter().all(|(op, _)| op != "replace"), "{ops:?}");
        assert!(
            ops.iter().any(|(op, _)| op == "remove") && ops.iter().any(|(op, _)| op == "add"),
            "{ops:?}"
        );
        // The same file against itself has no differences.
        assert!(ndjson_ops(&[&old, &old, "--keyless"], &dir.path("same.json")).is_empty());
    }

    #[test]
    fn the_key_options_only_apply_to_ndjson_files() {
        let dir = Scratch::new("ndjson-misuse");
        let a = dir.file("a.json", "[]");
        let b = dir.file("b.json", "[]");
        for flag in [&["--keyless"][..], &["--duplicates", "first"][..]] {
            let mut args = vec!["drift", "diff", &a, &b];
            args.extend_from_slice(flag);
            let message = run_cli(&args).unwrap_err().to_string();
            assert!(message.contains("NDJSON"), "{flag:?}: {message}");
        }
    }

    #[test]
    fn csv_and_tsv_files_are_compared_as_rows_with_a_header() {
        let dir = Scratch::new("csv-cli");
        let old = dir.file("old.csv", "id,name,score\n1,Ada,10\n2,Grace,20\n3,Linus,30\n");
        let new = dir.file("new.csv", "id,name,score\n0,Alan,5\n1,Ada,10\n2,Grace,21\n");
        let out = dir.path("ops.json");
        let ops = ndjson_ops(&[&old, &new, "--array-key", "id"], &out);
        assert_eq!(
            ops,
            [
                ("remove".to_string(), "/2".to_string()),
                ("add".into(), "/0".into()),
                ("replace".into(), "/2/score".into())
            ]
        );
        // The cells are text unless given a type, so a changed score is a string...
        assert_eq!(read_ops(&out).last().unwrap().value, Some(serde_json::json!("21")));
        // ...and a number with one.
        ndjson_ops(&[&old, &new, "--array-key", "id", "--types", "score:int"], &out);
        assert_eq!(read_ops(&out).last().unwrap().value, Some(serde_json::json!(21)));
        // A .tsv file is split at tabs without being told to.
        let old = dir.file("old.tsv", "id\tv\n1\ta,b\n2\tc\n");
        let new = dir.file("new.tsv", "id\tv\n1\ta,b\n2\td\n");
        assert_eq!(
            ndjson_ops(&[&old, &new, "--array-key", "id"], &out),
            [("replace".to_string(), "/1/v".to_string())]
        );
    }

    #[test]
    fn the_delimiter_and_column_types_can_be_set() {
        let dir = Scratch::new("csv-options");
        let old = dir.file("old.csv", "id;price;active\n1;1,5;true\n2;2,5;false\n");
        let new = dir.file("new.csv", "id;price;active\n1;1,5;false\n2;2,5;false\n");
        let out = dir.path("ops.json");
        let ops = ndjson_ops(
            &[&old, &new, "--array-key", "id", "--delimiter", ";", "--types", "id:int,active:bool"],
            &out,
        );
        assert_eq!(ops, [("replace".to_string(), "/0/active".to_string())]);
        assert_eq!(read_ops(&out)[0].value, Some(serde_json::json!(false)));
        // Words work for the delimiter too, and --types can be repeated.
        let pipe_old = dir.file("p-old.csv", "id|v\n1|a\n");
        let pipe_new = dir.file("p-new.csv", "id|v\n1|b\n");
        let ops = ndjson_ops(
            &[
                &pipe_old,
                &pipe_new,
                "--array-key",
                "id",
                "--delimiter",
                "pipe",
                "--types",
                "id:int",
                "--types",
                "v:string",
            ],
            &out,
        );
        assert_eq!(ops, [("replace".to_string(), "/0/v".to_string())]);
    }

    #[test]
    fn csv_diffs_say_what_is_wrong_with_the_options() {
        let dir = Scratch::new("csv-errors");
        let a = dir.file("a.csv", "id,v\n1,x\n");
        let b = dir.file("b.csv", "id,v\n1,y\n");
        let message = |extra: &[&str]| {
            let mut args = vec!["drift", "diff", &a, &b, "--array-key", "id"];
            args.extend_from_slice(extra);
            run_cli(&args).unwrap_err().to_string()
        };
        assert!(message(&["--types", "v"]).contains("COLUMN:TYPE"));
        assert!(message(&["--types", "v:number"]).contains("string, int, float, bool or json"));
        assert!(message(&["--types", "nope:int"]).contains("`nope`"));
        assert!(message(&["--delimiter", "ab"]).contains("one character"));
        // Different kinds of file cannot be compared as records.
        let nd = dir.file("c.ndjson", "{\"id\":1}\n");
        let error =
            run_cli(&["drift", "diff", &a, &nd, "--array-key", "id"]).unwrap_err().to_string();
        assert!(error.contains("same kind"), "{error}");
        // These options are for CSV and TSV only.
        let nd2 = dir.file("d.ndjson", "{\"id\":1}\n");
        let error =
            run_cli(&["drift", "diff", &nd, &nd2, "--array-key", "id", "--types", "id:int"])
                .unwrap_err()
                .to_string();
        assert!(error.contains("CSV"), "{error}");
        let json = dir.file("e.json", "[]");
        let error =
            run_cli(&["drift", "diff", &json, &json, "--delimiter", ";"]).unwrap_err().to_string();
        assert!(error.contains("CSV"), "{error}");
    }

    #[test]
    fn csv_files_can_be_matched_without_a_key() {
        let dir = Scratch::new("csv-keyless");
        let old = dir.file("old.csv", "a,b\n1,x\n2,y\n2,y\n");
        let new = dir.file("new.csv", "a,b\n2,y\n1,x\n3,z\n");
        let ops = ndjson_ops(&[&old, &new, "--keyless"], &dir.path("ops.json"));
        assert!(ops.iter().all(|(op, _)| op != "replace"), "{ops:?}");
        assert!(
            ops.iter().any(|(op, _)| op == "add") && ops.iter().any(|(op, _)| op == "remove"),
            "{ops:?}"
        );
    }

    /// Operations of every shape the writer has to print.
    fn sample_ops(count: usize) -> Vec<Delta> {
        let all = vec![
            Delta::with_value(
                Operation::Add,
                "/0",
                serde_json::json!({"id": 1, "tags": ["a", "é"], "o": {"k": null}}),
            ),
            Delta::new(Operation::Remove, "/3"),
            Delta::with_value(
                Operation::Replace,
                "/1/name",
                serde_json::json!("Ada \"Lovelace\"\n"),
            ),
            Delta {
                op: Operation::Move,
                path: "/0".into(),
                value: None,
                from_path: Some("/4".into()),
            },
            Delta::with_value(Operation::Add, "/9", serde_json::json!([])),
            Delta::with_value(Operation::Test, "/a~1b", serde_json::json!(1.5)),
            Delta {
                op: Operation::Copy,
                path: "/x".into(),
                value: None,
                from_path: Some("/y".into()),
            },
        ];
        all.into_iter().cycle().take(count).collect()
    }

    /// What `drift diff` printed before the output was written incrementally.
    fn collected_output(ops: &[Delta], style: Style) -> String {
        let text = match style {
            Style::Summary => ops
                .iter()
                .map(|o| format!("{} {}", sign(&o.op), o.path))
                .collect::<Vec<_>>()
                .join("\n"),
            Style::Stats { compact } | Style::Json { compact } => {
                let payload = if matches!(style, Style::Stats { .. }) {
                    let mut counts = std::collections::BTreeMap::new();
                    for o in ops {
                        *counts.entry(o.op.as_str()).or_insert(0u64) += 1;
                    }
                    let counts: serde_json::Map<String, Value> =
                        counts.into_iter().map(|(k, v)| (k.to_string(), Value::from(v))).collect();
                    serde_json::json!({"total": ops.len(), "by_op": counts})
                } else {
                    serde_json::to_value(ops).unwrap()
                };
                if compact {
                    serde_json::to_string(&payload).unwrap()
                } else {
                    serde_json::to_string_pretty(&payload).unwrap()
                }
            }
        };
        if text.ends_with('\n') {
            text
        } else {
            format!("{text}\n")
        }
    }

    #[test]
    fn the_incremental_writer_prints_exactly_what_the_collected_output_did() {
        let dir = Scratch::new("writer");
        for style in [
            Style::Json { compact: false },
            Style::Json { compact: true },
            Style::Summary,
            Style::Stats { compact: false },
            Style::Stats { compact: true },
        ] {
            for count in [0, 1, 2, 7, 15] {
                let ops = sample_ops(count);
                let path = dir.path("out.txt");
                let mut writer = OperationWriter::to(&Some(path.clone()), style).unwrap();
                for op in &ops {
                    writer.push(op).unwrap();
                }
                assert_eq!(writer.finish().unwrap(), count);
                assert_eq!(
                    fs::read_to_string(&path).unwrap(),
                    collected_output(&ops, style),
                    "{style:?} with {count} operations"
                );
            }
        }
    }
}

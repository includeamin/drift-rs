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
    path::Path,
};

#[derive(Parser)]
#[command(name="drift", version=VERSION, about="RFC 6902 structured document diff and patch tooling")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Diff(DiffArgs),
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

/// Operations per type, with types in alphabetical order so the output is stable.
fn op_counts(operations: &[Delta]) -> Value {
    let mut counts = std::collections::BTreeMap::new();
    for operation in operations {
        *counts.entry(operation.op.as_str()).or_insert(0u64) += 1;
    }
    Value::Object(
        counts.into_iter().map(|(op, count)| (op.to_string(), Value::from(count))).collect(),
    )
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

/// Files of records, one JSON object per line.
fn is_records(path: &str) -> bool {
    let extension = Path::new(path).extension().and_then(|e| e.to_str());
    extension.is_some_and(|e| e.eq_ignore_ascii_case("ndjson") || e.eq_ignore_ascii_case("jsonl"))
}

/// Whether this diff compares two NDJSON files as records, and if so whether it
/// can: they need a key and cannot come from stdin or be grepped.
fn records_inputs(args: &DiffArgs) -> Result<bool, Box<dyn std::error::Error>> {
    let (old, new) = (is_records(&args.old), is_records(&args.new));
    if !old && !new {
        if args.keyless || args.duplicates.is_some() {
            return Err(
                "--keyless and --duplicates apply only to NDJSON files (.ndjson or .jsonl)".into(),
            );
        }
        return Ok(false);
    }
    if args.old == "-" || args.new == "-" {
        return Err("NDJSON files are streamed from disk, so they cannot be read from stdin".into());
    }
    if !(old && new) {
        return Err(
            "to compare records, both inputs must be NDJSON files (.ndjson or .jsonl)".into()
        );
    }
    if args.array_keys.is_empty() && !args.keyless {
        return Err(
            "NDJSON files need --array-key FIELD, the field that identifies a row, or --keyless"
                .into(),
        );
    }
    if !args.values.is_empty() {
        return Err("--grep is not available for NDJSON files, which are not loaded whole".into());
    }
    Ok(true)
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

    let (diff_operations, old) = if records {
        // One JSON object per line: matched by key and streamed, never loaded whole.
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
        let mut options = drift::RecordOptions::new().diff(diff);
        options = key_fields.into_iter().fold(options, |o, field| o.key(field));
        if args.keyless {
            options = options.whole_row();
        }
        if let Some(policy) = args.duplicates {
            options = options.duplicates(policy.into());
        }
        (drift::diff_records_with(Path::new(&args.old), Path::new(&args.new), &options)?, None)
    } else if delegate {
        (diff_files(Path::new(&args.old), Path::new(&args.new))?, None)
    } else {
        let old = load(&args.old, format, args.xml_arrays)?.0;
        let new = load(&args.new, format, args.xml_arrays)?.0;
        let options = args.array_keys.iter().fold(DiffOptions::new(), |o, key| o.array_key(key));
        let options = args.ignore_paths.iter().fold(options, |o, pattern| o.ignore_path(pattern));
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
    if args.pretty {
        let text = operations
            .iter()
            .map(|operation| {
                format!(
                    "{} {}",
                    match operation.op {
                        Operation::Add => "+",
                        Operation::Remove => "-",
                        Operation::Replace => "~",
                        Operation::Move => "m",
                        Operation::Copy => "c",
                        Operation::Test => "?",
                    },
                    operation.path
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        write_output(text, &args.output.output)?;
    } else {
        let payload = if args.stats {
            let counts = op_counts(&operations);
            serde_json::json!({"total": operations.len(), "by_op": counts})
        } else {
            serde_json::to_value(&operations)?
        };
        write_output(
            if args.output.compact {
                serde_json::to_string(&payload)?
            } else {
                serde_json::to_string_pretty(&payload)?
            },
            &args.output.output,
        )?;
    }
    Ok(if args.exit_code && !operations.is_empty() { 1 } else { 0 })
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
        Command::Diff(args) => cmd_diff(args)?,
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
    fn op_counts_are_alphabetical_whatever_the_operation_order() {
        let ops = [
            Delta::new(Operation::Replace, "/a"),
            Delta::new(Operation::Remove, "/b"),
            Delta::new(Operation::Add, "/c"),
            Delta::new(Operation::Remove, "/d"),
        ];
        assert_eq!(
            serde_json::to_string(&op_counts(&ops)).unwrap(),
            r#"{"add":1,"remove":2,"replace":1}"#
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
            Command::Diff(args) => cmd_diff(args),
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
}

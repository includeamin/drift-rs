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

fn cmd_diff(args: DiffArgs) -> Result<i32, Box<dyn std::error::Error>> {
    // A pattern that is not a JSON Pointer would silently match nothing.
    for pattern in &args.ignore_paths {
        drift::split_pointer(pattern).map_err(|_| {
            format!("invalid --ignore-path `{pattern}`: it must be empty or a JSON Pointer starting with `/`")
        })?;
    }
    let format = resolve(args.format, &args.old);

    // `--grep` inspects old values, so it needs the whole old document in memory.
    let delegate = matches!(format, drift::formats::Format::Json)
        && args.old != "-"
        && args.new != "-"
        && args.values.is_empty()
        && args.array_keys.is_empty()
        && args.ignore_paths.is_empty();

    let (diff_operations, old) = if delegate {
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
}

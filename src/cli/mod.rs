pub mod formats;

use self::formats::{dump, load, resolve, Format};
use clap::{Args, Parser, Subcommand};
use drift::{diff, diff_files, filter_operations, patch, Delta, Operation, VERSION};
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
    no_color: bool,
    #[arg(long)]
    exit_code: bool,
    #[arg(long, value_enum)]
    format: Option<Format>,
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
    #[command(flatten)]
    output: OutputArgs,
}
#[derive(Args)]
struct CheckArgs {
    old: String,
    new: String,
    #[arg(long, value_enum)]
    format: Option<Format>,
    #[command(flatten)]
    output: OutputArgs,
}

fn write_output(
    text: String,
    destination: &Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    match destination {
        Some(path) if path != "-" => fs::write(path, format!("{text}\n"))?,
        _ => println!("{text}"),
    }
    Ok(())
}

fn delta_json(delta: &Delta) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("op".into(), Value::String(delta.op.as_str().into()));
    map.insert("path".into(), Value::String(delta.path.clone()));
    if let Some(value) = &delta.value {
        if matches!(delta.op, Operation::Add | Operation::Replace | Operation::Test) {
            map.insert("value".into(), value.clone());
        }
    }
    if let Some(from) = &delta.from_path {
        map.insert("from".into(), Value::String(from.clone()));
    }
    Value::Object(map)
}

fn cmd_diff(args: DiffArgs) -> Result<i32, Box<dyn std::error::Error>> {
    let format = resolve(args.format, &args.old);

    // `--grep` inspects old values, so it needs the whole old document in memory.
    let delegate = matches!(format, Format::Json)
        && args.old != "-"
        && args.new != "-"
        && args.values.is_empty();

    let (diff_operations, old) = if delegate {
        (diff_files(Path::new(&args.old), Path::new(&args.new))?, None)
    } else {
        let old = load(&args.old, format)?;
        let new = load(&args.new, format)?;
        (diff(&new, &old), Some(old))
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
            let mut counts = serde_json::Map::new();
            for operation in &operations {
                let count = counts.entry(operation.op.as_str()).or_insert(Value::from(0));
                *count = Value::from(count.as_u64().unwrap_or(0) + 1);
            }
            serde_json::json!({"total": operations.len(), "by_op": counts})
        } else {
            Value::Array(operations.iter().map(delta_json).collect())
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

fn parse_delta(raw: &Value) -> Result<Delta, Box<dyn std::error::Error>> {
    let op = match raw["op"].as_str().ok_or("missing op")? {
        "add" => Operation::Add,
        "remove" => Operation::Remove,
        "replace" => Operation::Replace,
        "move" => Operation::Move,
        "copy" => Operation::Copy,
        "test" => Operation::Test,
        _ => return Err("unsupported operation".into()),
    };
    Ok(Delta {
        op,
        path: raw["path"].as_str().unwrap_or("").into(),
        value: raw.get("value").cloned(),
        from_path: raw.get("from").and_then(Value::as_str).map(String::from),
    })
}

fn read_patch(path: &str) -> Result<Vec<Delta>, Box<dyn std::error::Error>> {
    let mut text = String::new();
    if path == "-" {
        io::stdin().read_to_string(&mut text)?;
    } else {
        text = fs::read_to_string(path)?;
    }
    serde_json::from_str::<Vec<Value>>(&text)?.iter().map(parse_delta).collect::<Result<_, _>>()
}

fn cmd_patch(args: PatchArgs) -> Result<i32, Box<dyn std::error::Error>> {
    let format = resolve(args.format, &args.document);
    let result = patch(load(&args.document, format)?, &read_patch(&args.patch)?)?;
    let destination = if args.in_place { Some(args.document) } else { args.output.output };
    write_output(dump(&result, format, args.output.compact)?, &destination)?;
    Ok(0)
}

fn cmd_paths(args: PathsArgs) -> Result<i32, Box<dyn std::error::Error>> {
    let document = load(&args.document, resolve(args.format, &args.document))?;
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
    let old = load(&args.old, format)?;
    let new = load(&args.new, format)?;
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

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let command = Cli::parse().command;
    let code = match command {
        Command::Diff(args) => cmd_diff(args)?,
        Command::Patch(args) => cmd_patch(args)?,
        Command::Paths(args) => cmd_paths(args)?,
        Command::Check(args) => cmd_check(args)?,
    };
    std::process::exit(code)
}

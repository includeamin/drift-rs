//! End-to-end tests: run the real `drift` binary.
#![cfg(feature = "cli")]

use std::io::Write;
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

fn drift(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_drift")).args(args).output().unwrap()
}

fn drift_stdin(args: &[&str], input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_drift"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

fn write(dir: &TempDir, name: &str, text: &str) -> String {
    let path = dir.path().join(name);
    std::fs::write(&path, text).unwrap();
    path.to_str().unwrap().to_string()
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn json(out: &Output) -> serde_json::Value {
    serde_json::from_str(&stdout(out)).unwrap()
}

#[test]
fn diff_prints_operations_and_exits_zero() {
    let dir = TempDir::new().unwrap();
    let old = write(&dir, "old.json", r#"{"a":1}"#);
    let new = write(&dir, "new.json", r#"{"a":2}"#);
    let out = drift(&["diff", &old, &new]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(json(&out), serde_json::json!([{"op":"replace","path":"/a","value":2}]));
}

#[test]
fn exit_code_flag_signals_differences() {
    let dir = TempDir::new().unwrap();
    let old = write(&dir, "old.json", r#"{"a":1}"#);
    let same = write(&dir, "same.json", r#"{"a":1}"#);
    let new = write(&dir, "new.json", r#"{"a":2}"#);
    assert_eq!(drift(&["diff", "--exit-code", &old, &same]).status.code(), Some(0));
    assert_eq!(drift(&["diff", "--exit-code", &old, &new]).status.code(), Some(1));
}

#[test]
fn diff_reads_one_document_from_stdin() {
    let dir = TempDir::new().unwrap();
    let old = write(&dir, "old.json", r#"{"a":1}"#);
    let out = drift_stdin(&["diff", &old, "-", "--format", "json"], r#"{"a":1,"b":2}"#);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(json(&out), serde_json::json!([{"op":"add","path":"/b","value":2}]));
}

#[test]
fn missing_file_fails_with_a_message() {
    let out = drift(&["diff", "/nonexistent/old.json", "/nonexistent/new.json"]);
    assert_ne!(out.status.code(), Some(0));
    assert!(!out.stderr.is_empty());
}

#[test]
fn invalid_document_fails() {
    let dir = TempDir::new().unwrap();
    let bad = write(&dir, "bad.json", "{not json");
    let good = write(&dir, "good.json", "{}");
    assert_ne!(drift(&["diff", &bad, &good]).status.code(), Some(0));
}

#[test]
fn patch_in_place_rewrites_the_file_and_adds_no_blank_lines() {
    let dir = TempDir::new().unwrap();
    let doc = write(&dir, "doc.json", "{\"a\":1}\n");
    let patch = write(&dir, "p.json", r#"[{"op":"replace","path":"/a","value":2}]"#);
    for _ in 0..2 {
        assert_eq!(drift(&["patch", &doc, &patch, "--in-place"]).status.code(), Some(0));
    }
    let text = std::fs::read_to_string(&doc).unwrap();
    assert_eq!(serde_json::from_str::<serde_json::Value>(&text).unwrap()["a"], 2);
    assert!(!text.ends_with("\n\n"));
}

#[test]
fn patch_reads_the_patch_from_stdin() {
    let dir = TempDir::new().unwrap();
    let doc = write(&dir, "doc.json", r#"{"a":1}"#);
    let out = drift_stdin(
        &["patch", &doc, "-", "--compact"],
        r#"[{"op":"add","path":"/b","value":true}]"#,
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(json(&out), serde_json::json!({"a":1,"b":true}));
}

#[test]
fn failing_patch_leaves_the_file_untouched() {
    let dir = TempDir::new().unwrap();
    let doc = write(&dir, "doc.json", r#"{"a":1}"#);
    let patch = write(&dir, "p.json", r#"[{"op":"remove","path":"/missing"}]"#);
    let out = drift(&["patch", &doc, &patch, "--in-place"]);
    assert_ne!(out.status.code(), Some(0));
    assert_eq!(std::fs::read_to_string(&doc).unwrap(), r#"{"a":1}"#);
}

#[test]
fn yaml_patch_keeps_comments() {
    let dir = TempDir::new().unwrap();
    let doc = write(&dir, "c.yaml", "# top\nport: 80 # http\nname: x\n");
    let patch = write(&dir, "p.json", r#"[{"op":"replace","path":"/port","value":90}]"#);
    assert_eq!(drift(&["patch", &doc, &patch, "--in-place"]).status.code(), Some(0));
    assert_eq!(std::fs::read_to_string(&doc).unwrap(), "# top\nport: 90 # http\nname: x\n");
}

#[test]
fn output_flag_writes_a_file() {
    let dir = TempDir::new().unwrap();
    let old = write(&dir, "old.json", "[1]");
    let new = write(&dir, "new.json", "[1,2]");
    let dest = dir.path().join("out.json");
    let out = drift(&["diff", &old, &new, "-o", dest.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty());
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dest).unwrap()).unwrap();
    assert_eq!(written, serde_json::json!([{"op":"add","path":"/1","value":2}]));
}

#[test]
fn diff_stats_and_pretty() {
    let dir = TempDir::new().unwrap();
    let old = write(&dir, "old.json", r#"{"a":1,"b":2}"#);
    let new = write(&dir, "new.json", r#"{"a":9}"#);
    let stats = json(&drift(&["diff", "--stats", &old, &new]));
    assert_eq!(stats["total"], 2);
    let pretty = stdout(&drift(&["diff", "--pretty", &old, &new]));
    assert!(pretty.contains("~ /a") && pretty.contains("- /b"), "{pretty}");
}

#[test]
fn check_reports_a_roundtrip() {
    let dir = TempDir::new().unwrap();
    let old = write(&dir, "old.json", r#"{"a":[1,2,3]}"#);
    let new = write(&dir, "new.json", r#"{"a":[3,2],"b":null}"#);
    let out = drift(&["check", &old, &new]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(json(&out)["roundtrip"], true);
}

#[test]
fn invert_then_patch_restores_the_document() {
    let dir = TempDir::new().unwrap();
    let doc = write(&dir, "doc.json", r#"{"a":1,"b":[1,2]}"#);
    let patch = write(
        &dir,
        "p.json",
        r#"[{"op":"replace","path":"/a","value":5},{"op":"remove","path":"/b/0"}]"#,
    );
    let undo = drift(&["invert", &doc, &patch]);
    assert_eq!(undo.status.code(), Some(0));
    let undo_path = write(&dir, "undo.json", &stdout(&undo));
    assert_eq!(drift(&["patch", &doc, &patch, "--in-place"]).status.code(), Some(0));
    assert_eq!(drift(&["patch", &doc, &undo_path, "--in-place"]).status.code(), Some(0));
    let restored: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&doc).unwrap()).unwrap();
    assert_eq!(restored, serde_json::json!({"a":1,"b":[1,2]}));
}

#[test]
fn compose_squashes_patches() {
    let dir = TempDir::new().unwrap();
    let doc = write(&dir, "doc.json", r#"{"a":1}"#);
    let p1 = write(&dir, "p1.json", r#"[{"op":"replace","path":"/a","value":2}]"#);
    let p2 = write(&dir, "p2.json", r#"[{"op":"replace","path":"/a","value":3}]"#);
    let out = drift(&["compose", &doc, &p1, &p2]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(json(&out), serde_json::json!([{"op":"replace","path":"/a","value":3}]));
}

#[test]
fn paths_lists_pointers() {
    let dir = TempDir::new().unwrap();
    let doc = write(&dir, "doc.json", r#"{"a":{"b":1}}"#);
    let out = drift(&["paths", &doc]);
    assert_eq!(out.status.code(), Some(0));
    assert!(stdout(&out).contains("/a/b"));
}

#[test]
fn array_key_and_ignore_path_options() {
    let dir = TempDir::new().unwrap();
    let old = write(&dir, "old.json", r#"{"u":[{"id":1,"t":1},{"id":2,"t":1}]}"#);
    let new = write(&dir, "new.json", r#"{"u":[{"id":0,"t":1},{"id":1,"t":2},{"id":2,"t":2}]}"#);
    let keyed = json(&drift(&["diff", "--array-key", "id", "--ignore-path", "/**/t", &old, &new]));
    assert_eq!(keyed, serde_json::json!([{"op":"add","path":"/u/0","value":{"id":0,"t":1}}]));
}

#[test]
fn ndjson_records_matched_by_key() {
    let dir = TempDir::new().unwrap();
    let old = write(&dir, "old.ndjson", "{\"id\":1,\"v\":1}\n{\"id\":2,\"v\":1}\n");
    let new =
        write(&dir, "new.ndjson", "{\"id\":0,\"v\":0}\n{\"id\":1,\"v\":1}\n{\"id\":2,\"v\":1}\n");
    let out = drift(&["diff", "--array-key", "id", &old, &new]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(json(&out).as_array().unwrap().len(), 1);
}

#[test]
fn csv_records_with_types() {
    let dir = TempDir::new().unwrap();
    let old = write(&dir, "old.csv", "id,score\n1,10\n2,20\n");
    let new = write(&dir, "new.csv", "id,score\n1,10\n2,25\n");
    let out = drift(&["diff", "--array-key", "id", "--types", "id:int,score:int", &old, &new]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(json(&out), serde_json::json!([{"op":"replace","path":"/1/score","value":25}]));
}

#[test]
fn version_flag() {
    let out = drift(&["--version"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(stdout(&out).contains(env!("CARGO_PKG_VERSION")));
}

//! Writing a changed YAML document back without losing its comments.
//!
//! The same idea as for TOML: parse the original again, with a lossless syntax
//! tree, and bring it in line with the patched value tree, leaving whatever did
//! not change untouched. The editing library (`yaml-edit`) is reliable for
//! scalars and keys but not for nested block structures, so new lists and
//! mappings are inserted in flow style (`{a: 1, b: [x, y]}`), which is valid
//! YAML that sits on one line and cannot be mis-indented. The result is always
//! checked to read back as the target value.

use super::align::{align, Step};
use super::{parse_with, Format, ParseOptions};
use serde_json::{Map, Value};
use std::str::FromStr;
use yaml_edit::{Mapping, Scalar, ScalarValue, Sequence, YamlFile, YamlNode};

type Result<T> = std::result::Result<T, String>;

/// How lists whose items are added or removed are edited.
#[derive(Clone, Copy, PartialEq)]
enum Lists {
    /// In place, item by item, which keeps each item's comment. The editing
    /// library mis-writes some layouts (items that are not indented, appending
    /// after a comment, removing the last item of a nested list).
    InPlace,
    /// By replacing the whole list with a flow-style one. The comments inside
    /// that list are lost, but nothing else is at risk.
    Replace,
}

/// `original` with the changes that turn it into `new`, comments and layout kept.
///
/// `new` is the document as a value tree, normally `original` read and then
/// patched. Fails if `new` cannot be written this way, or if what was written
/// does not read back as `new`; the caller then writes the document fresh.
pub(super) fn edit(original: &str, new: &Value) -> Result<String> {
    // Lists edited in place keep their comments but are where the editing
    // library goes wrong, so if that does not read back, replace them instead.
    attempt(original, new, Lists::InPlace).or_else(|_| attempt(original, new, Lists::Replace))
}

fn attempt(original: &str, new: &Value, lists: Lists) -> Result<String> {
    let options = ParseOptions::default();
    let (old, _) = parse_with(original, Format::Yaml, &options).map_err(|e| e.to_string())?;
    let file = YamlFile::from_str(original).map_err(|e| e.to_string())?;
    let document = file.document().ok_or("the file has no document")?;
    match (&old, new) {
        (Value::Object(old), Value::Object(new)) => {
            mapping(&document.as_mapping().ok_or("the root is not a mapping")?, old, new, lists)?
        }
        (Value::Array(old), Value::Array(new)) => {
            // Nothing above a root list to replace it in.
            sequence(
                &document.as_sequence().ok_or("the root is not a list")?,
                old,
                new,
                Lists::InPlace,
            )?;
        }
        _ => {
            return Err(
                "only a mapping or a list can be edited in place, and not turned into the other"
                    .into(),
            )
        }
    }
    let mut text = file.to_string();
    // A mapping or list with no members has no block form: `{}` / `[]`.
    if new.as_object().is_some_and(Map::is_empty) || new.as_array().is_some_and(Vec::is_empty) {
        if !text.ends_with('\n') && !text.is_empty() {
            text.push('\n');
        }
        text.push_str(if new.is_object() { "{}\n" } else { "[]\n" });
    }
    // The editing library mis-writes some structures, so never trust it blindly.
    match parse_with(&text, Format::Yaml, &options) {
        Ok((read_back, _)) if read_back == *new => Ok(text),
        _ => Err(format!("the edited document did not read back as the target:\n{text}")),
    }
}

fn mapping(
    m: &Mapping,
    old: &Map<String, Value>,
    new: &Map<String, Value>,
    lists: Lists,
) -> Result<()> {
    for key in old.keys().filter(|key| !new.contains_key(*key)) {
        m.remove(key.as_str());
    }
    for (key, value) in new {
        match old.get(key) {
            Some(before) if before == value => {}
            Some(before) => member_in_place(m, key, before, value, lists)?,
            None => put(m, key, value)?,
        }
    }
    Ok(())
}

fn member_in_place(
    m: &Mapping,
    key: &str,
    before: &Value,
    value: &Value,
    lists: Lists,
) -> Result<()> {
    match (before, value) {
        (Value::Object(old), Value::Object(new)) => {
            // An alias (`*name`) is not a mapping: replace it with the value it stood for.
            match m.get_mapping(key) {
                Some(child) => mapping(&child, old, new, lists),
                None => put(m, key, value),
            }
        }
        (Value::Array(old), Value::Array(new)) => match m.get_sequence(key) {
            Some(child) if sequence(&child, old, new, lists)? => Ok(()),
            _ => put(m, key, value),
        },
        _ => put(m, key, value),
    }
}

/// Sets a member, whether it is new or replaces one (the comment after the
/// old value stays).
fn put(m: &Mapping, key: &str, value: &Value) -> Result<()> {
    match scalar(value) {
        Some(scalar) => m.set(key, scalar),
        None => m.set(key, &flow(value)?),
    }
    Ok(())
}

/// Edits a list item by item, matching old and new items by alignment as for
/// TOML. Returns `false`, having changed nothing, when the list has to be
/// replaced as a whole instead (see [`Lists::Replace`]).
///
/// `insert` is used rather than `push`, which writes the new item inside the
/// comment of the last one.
fn sequence(s: &Sequence, old: &[Value], new: &[Value], lists: Lists) -> Result<bool> {
    let steps = align(old, new);
    let restructures = steps.iter().any(|step| matches!(step, Step::Remove | Step::Insert(_)));
    if restructures && lists == Lists::Replace {
        return Ok(false);
    }
    let mut at = 0;
    for step in steps {
        match step {
            Step::Keep => at += 1,
            Step::Edit(i, j) => {
                item_in_place(s, at, &old[i], &new[j], lists)?;
                at += 1;
            }
            Step::Remove => {
                s.remove(at);
            }
            Step::Insert(j) => {
                match scalar(&new[j]) {
                    Some(scalar) => s.insert(at, scalar),
                    None => s.insert(at, &flow(&new[j])?),
                }
                at += 1;
            }
        }
    }
    Ok(true)
}

fn item_in_place(
    s: &Sequence,
    at: usize,
    before: &Value,
    value: &Value,
    lists: Lists,
) -> Result<()> {
    match (before, value, s.get(at)) {
        (Value::Object(old), Value::Object(new), Some(YamlNode::Mapping(child))) => {
            mapping(&child, old, new, lists)
        }
        (Value::Array(old), Value::Array(new), Some(YamlNode::Sequence(child)))
            if sequence(&child, old, new, lists)? =>
        {
            Ok(())
        }
        _ => {
            let replaced = match scalar(value) {
                Some(scalar) => s.set(at, scalar),
                None => s.set(at, &flow(value)?),
            };
            replaced.then_some(()).ok_or_else(|| format!("item {at} could not be replaced"))
        }
    }
}

/// A scalar, or `None` for a list or mapping.
///
/// The scalar is written to text and parsed back into a node, so numbers keep
/// exactly the digits they have (`1.0` stays a float, which the library's own
/// number writing would turn into `1`) while strings get its quoting.
fn scalar(value: &Value) -> Option<Scalar> {
    let text = match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(text) if needs_quotes(text) => {
            ScalarValue::double_quoted(text.as_str()).to_yaml_string()
        }
        Value::String(text) => ScalarValue::string(text.as_str()).to_yaml_string(),
        Value::Array(_) | Value::Object(_) => return None,
    };
    YamlFile::from_str(&text).ok()?.document()?.as_scalar()
}

/// Strings the library's own quoting gets wrong: ones with line breaks or
/// control characters, and `.inf`/`.nan`, which would read back as floats.
fn needs_quotes(text: &str) -> bool {
    let special =
        matches!(text.to_ascii_lowercase().trim_start_matches(['+', '-']), ".inf" | ".nan");
    special || text.chars().any(char::is_control)
}

/// A list or mapping in flow style, which is valid YAML on a single line. The
/// editing library cannot be trusted to indent a block one inside another.
/// JSON is a subset of YAML, so the node is parsed from the value's JSON text.
fn flow(value: &Value) -> Result<YamlNode> {
    let text = serde_json::to_string(value).map_err(|e| e.to_string())?;
    let file = YamlFile::from_str(&text).map_err(|e| e.to_string())?;
    let document = file.document().ok_or("the value has no document")?;
    document
        .as_mapping()
        .map(YamlNode::Mapping)
        .or_else(|| document.as_sequence().map(YamlNode::Sequence))
        .ok_or_else(|| "the value is not a list or a mapping".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::{dump, parse, Format};
    use serde_json::{json, Value};

    const SAMPLE: &str = r#"# Deployment config
# maintained by ops

name: app   # display name
replicas: 2
image: "nginx:1.25"   # pinned
ratio: 0.5
enabled: true

env:
  LOG_LEVEL: info   # verbosity
  REGION: eu

# scaling rules
scaling:
  min: 1
  max: 5
  rules:
    - cpu     # first
    - memory  # second

ports:
  - 80
  - 443
"#;

    fn value(text: &str) -> Value {
        parse(text, Format::Yaml).unwrap()
    }

    /// `SAMPLE` edited by `change`, written back, and checked to read as the edit.
    fn edited(change: impl FnOnce(&mut Value)) -> String {
        let mut new = value(SAMPLE);
        change(&mut new);
        let out = edit(SAMPLE, &new).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(value(&out), new, "the output must read back as the edit:\n{out}");
        out
    }

    fn lines_differing(a: &str, b: &str) -> Vec<(String, String)> {
        let (a, b): (Vec<_>, Vec<_>) = (a.lines().collect(), b.lines().collect());
        assert_eq!(a.len(), b.len(), "line counts differ:\n{a:?}\n{b:?}");
        a.iter()
            .zip(&b)
            .filter(|(x, y)| x != y)
            .map(|(x, y)| (x.to_string(), y.to_string()))
            .collect()
    }

    #[test]
    fn an_unchanged_document_is_written_back_byte_for_byte() {
        assert_eq!(edit(SAMPLE, &value(SAMPLE)).unwrap(), SAMPLE);
    }

    #[test]
    fn changing_a_scalar_changes_only_that_line_and_keeps_its_comment() {
        let out = edited(|v| v["replicas"] = json!(3));
        assert_eq!(lines_differing(SAMPLE, &out), [("replicas: 2".into(), "replicas: 3".into())]);

        let out = edited(|v| v["name"] = json!("other"));
        assert_eq!(
            lines_differing(SAMPLE, &out),
            [("name: app   # display name".into(), "name: other   # display name".into())]
        );
        let out = edited(|v| v["image"] = json!("nginx:1.26"));
        assert!(out.contains("# pinned") && out.contains("nginx:1.26"), "{out}");
    }

    #[test]
    fn nested_scalars_keep_their_comments_too() {
        let out = edited(|v| v["env"]["LOG_LEVEL"] = json!("debug"));
        assert_eq!(
            lines_differing(SAMPLE, &out),
            [("  LOG_LEVEL: info   # verbosity".into(), "  LOG_LEVEL: debug   # verbosity".into())]
        );
        let out = edited(|v| v["scaling"]["max"] = json!(10));
        assert_eq!(lines_differing(SAMPLE, &out), [("  max: 5".into(), "  max: 10".into())]);
    }

    #[test]
    fn untouched_lines_keep_their_exact_formatting() {
        let out = edited(|v| v["replicas"] = json!(9));
        for line in [
            "# Deployment config",
            "image: \"nginx:1.25\"   # pinned",
            "    - cpu     # first",
            "# scaling rules",
        ] {
            assert!(out.contains(line), "lost `{line}`:\n{out}");
        }
    }

    #[test]
    fn a_value_keeps_its_type_when_it_looks_like_another() {
        for text in [
            "9090",
            "true",
            "null",
            "~",
            "yes",
            "no",
            "1e3",
            "0x1F",
            "2024-01-01",
            "12:30",
            ".inf",
            ".nan",
            "-.inf",
        ] {
            let out = edited(|v| v["name"] = json!(text));
            assert_eq!(value(&out)["name"], json!(text), "{text}: {out}");
        }
        let out = edited(|v| v["name"] = Value::Null);
        assert_eq!(value(&out)["name"], Value::Null);
        let out = edited(|v| v["name"] = json!(true));
        assert_eq!(value(&out)["name"], json!(true));
    }

    #[test]
    fn awkward_strings_survive() {
        for text in [
            "a: b",
            "#hash",
            "- dash",
            "it's",
            "say \"hi\"",
            "multi\nline",
            "line1\nline2\n",
            "\nstarts nl",
            "tab\there",
            "unicode ☃",
            "emoji 😀",
            "a\u{7f}b",
            "",
            " lead",
            "trail ",
        ] {
            let out = edited(|v| v["name"] = json!(text));
            assert_eq!(value(&out)["name"], json!(text), "{text:?}: {out}");
        }
    }

    #[test]
    fn keys_are_added_and_removed_without_touching_the_others() {
        let out = edited(|v| v["timeout"] = json!(30));
        assert!(out.contains("timeout: 30"), "{out}");
        for comment in
            ["# display name", "# pinned", "# verbosity", "# first", "# second", "# scaling rules"]
        {
            assert!(out.contains(comment), "lost `{comment}`:\n{out}");
        }
        let out = edited(|v| v["env"]["TZ"] = json!("UTC"));
        assert!(out.contains("TZ: UTC") && out.contains("# verbosity"), "{out}");

        let out = edited(|v| {
            v.as_object_mut().unwrap().remove("ratio");
            v["env"].as_object_mut().unwrap().remove("REGION");
        });
        assert!(!out.contains("ratio") && !out.contains("REGION"), "{out}");
        assert!(out.contains("# display name") && out.contains("# verbosity"), "{out}");
    }

    #[test]
    fn new_lists_and_mappings_are_inserted_and_read_back() {
        let out = edited(|v| {
            v["labels"] = json!({"team": "core", "tiers": ["web", "api"], "empty": {}, "none": []})
        });
        assert!(out.contains("# scaling rules"), "{out}");
        let out = edited(|v| v["env"]["extra"] = json!({"a": [1, {"b": null}]}));
        assert!(out.contains("# verbosity"), "{out}");
        let out = edited(|v| v["matrix"] = json!([[1, 2], [3, [4, 5]], {"k": "v"}]));
        assert!(out.contains("# first"), "{out}");
    }

    #[test]
    fn lists_grow_shrink_and_change_and_always_read_back() {
        let out = edited(|v| v["ports"] = json!([80, 8080, 443]));
        assert!(out.contains("8080"), "{out}");
        let out = edited(|v| v["ports"] = json!([443]));
        assert!(!out.contains("- 80\n"), "{out}");
        let out = edited(|v| v["scaling"]["rules"] = json!(["memory"]));
        assert_eq!(value(&out)["scaling"]["rules"], json!(["memory"]));
        let out = edited(|v| v["scaling"]["rules"] = json!([]));
        assert_eq!(value(&out)["scaling"]["rules"], json!([]));
        let out = edited(|v| v["scaling"]["rules"] = json!(["disk", "cpu", "memory"]));
        assert!(out.contains("disk"), "{out}");
    }

    #[test]
    fn a_list_edited_in_place_keeps_its_item_comments_where_the_editor_copes_with_the_layout() {
        let text = "name: x  # who\ntags:\n  - a  # one\n  - b  # two\n  - c  # three\n";
        let edit_tags = |tags: Value| {
            let mut new = value(text);
            new["tags"] = tags;
            let out = edit(text, &new).unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(value(&out), new, "{out}");
            out
        };
        for out in [
            edit_tags(json!(["a", "b", "c", "d"])),
            edit_tags(json!(["x", "a", "b", "c"])),
            edit_tags(json!(["a", "x", "b", "c"])),
            edit_tags(json!(["a", "b"])),
            edit_tags(json!(["b", "c"])),
        ] {
            assert!(out.contains("# who"), "{out}");
        }
        let kept = edit_tags(json!(["a", "b", "c", "d"]));
        assert!(
            kept.contains("# one") && kept.contains("# two") && kept.contains("# three"),
            "{kept}"
        );
    }

    #[test]
    fn when_the_editor_mishandles_a_list_only_that_lists_comments_are_at_risk() {
        // `rules` is nested, commented and followed by more keys: the layout in
        // which appending natively writes the item in the wrong place.
        let out = edited(|v| v["scaling"]["rules"] = json!(["cpu", "memory", "disk"]));
        for comment in
            ["# Deployment config", "# display name", "# pinned", "# verbosity", "# scaling rules"]
        {
            assert!(out.contains(comment), "lost `{comment}` outside the list:\n{out}");
        }
        // And everything after the list is still there.
        assert!(out.contains("ports:") && out.contains("- 443"), "{out}");
    }

    #[test]
    fn a_value_can_change_kind() {
        let out = edited(|v| v["replicas"] = json!({"min": 1, "max": 3}));
        assert!(out.contains("# display name"), "{out}");
        let out = edited(|v| v["env"] = json!("flat"));
        assert!(out.contains("env: flat") && !out.contains("LOG_LEVEL"), "{out}");
        let out = edited(|v| v["ports"] = json!(443));
        assert!(!out.contains("- 80"), "{out}");
        let out = edited(|v| v["name"] = json!(["a", "b"]));
        assert!(out.contains("# display name") || out.contains("name:"), "{out}");
    }

    #[test]
    fn anchors_and_aliases_are_replaced_by_the_value_they_stood_for() {
        let text = "base: &base\n  a: 1\nother: *base  # reuse\n";
        let mut new = value(text);
        new["other"]["a"] = json!(2);
        let out = edit(text, &new).unwrap();
        assert_eq!(value(&out), new, "{out}");
    }

    #[test]
    fn the_root_can_be_a_list_and_other_roots_are_refused() {
        let text = "# items\n- a   # one\n- b\n";
        let new = json!(["a", "b", "c"]);
        let out = edit(text, &new).unwrap();
        assert_eq!(value(&out), new, "{out}");
        assert!(out.contains("# one"), "{out}");
        assert!(edit("a: 1\n", &json!("scalar")).is_err());
        assert!(edit("a: 1\n", &json!([1])).is_err(), "a mapping cannot become a list in place");
    }

    #[test]
    fn a_file_that_does_not_parse_is_an_error() {
        assert!(edit("a: [1, 2\n", &json!({"a": 1})).is_err());
    }

    #[test]
    fn random_edits_are_either_refused_or_read_back_exactly() {
        // xorshift: deterministic, no extra dependencies.
        let mut state = 0x7F4A7C159E3779B9u64;
        let mut next = move |n: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % n
        };
        const WORDS: [&str; 8] = ["a", "yes", "9090", "x y", "q: r", "", "null", "- d"];
        fn scalar(next: &mut dyn FnMut(u64) -> u64) -> Value {
            match next(5) {
                0 => json!(next(100) as i64 - 50),
                1 => json!(WORDS[next(8) as usize]),
                2 => json!(next(2) == 0),
                3 => Value::Null,
                _ => json!(next(100) as f64 / 4.0),
            }
        }
        fn tree(depth: usize, next: &mut dyn FnMut(u64) -> u64) -> Value {
            match if depth == 0 { 0 } else { next(4) } {
                0 | 1 => scalar(next),
                2 => Value::Array((0..next(4)).map(|_| tree(depth - 1, next)).collect()),
                _ => table(depth - 1, next),
            }
        }
        fn table(depth: usize, next: &mut dyn FnMut(u64) -> u64) -> Value {
            let mut map = serde_json::Map::new();
            for key in ["a", "b", "c", "d", "e"] {
                if next(3) != 0 {
                    map.insert(key.to_string(), tree(depth, next));
                }
            }
            Value::Object(map)
        }
        let (mut done, mut refused) = (0, 0);
        for round in 0..500 {
            let before = table(3, &mut next);
            let text = dump(&before, Format::Yaml, false).unwrap();
            let mut after = value(&text);
            for key in ["a", "b", "c", "d", "e"] {
                match next(4) {
                    0 => {
                        after.as_object_mut().unwrap().remove(key);
                    }
                    1 => after[key] = tree(3, &mut next),
                    _ => {}
                }
            }
            match edit(&text, &after) {
                Ok(out) => {
                    assert_eq!(
                        value(&out),
                        after,
                        "round {round}\n--- before\n{text}\n--- after\n{out}"
                    );
                    done += 1;
                }
                Err(_) => refused += 1,
            }
        }
        // Refusing is allowed (the caller then writes the document fresh), but it
        // must be rare: about 3% of these random edits, which stack several
        // changes, hit a layout the editing library mis-writes.
        assert!(
            done * 10 >= (done + refused) * 9,
            "{refused} of {} edits were refused",
            done + refused
        );
    }

    #[test]
    fn numbers_keep_their_exact_form() {
        for number in [
            json!(1.0),
            json!(0.0),
            json!(-3.0),
            json!(16.0),
            json!(0.75),
            json!(1e300),
            json!(5),
            json!(-7),
            json!(u64::MAX),
        ] {
            let out = edited(|v| v["ratio"] = number.clone());
            assert_eq!(value(&out)["ratio"], number, "{number}: {out}");
        }
    }

    #[test]
    fn the_last_member_or_item_can_be_removed_leaving_an_empty_document() {
        let out = edit("a: 1\n", &json!({})).unwrap();
        assert_eq!(value(&out), json!({}), "{out:?}");
        let out = edit("# keep\n- 1\n", &json!([])).unwrap();
        assert_eq!(value(&out), json!([]), "{out:?}");
        assert!(out.contains("# keep"), "{out:?}");
    }
}

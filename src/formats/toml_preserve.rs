//! Writing a changed TOML document back without losing its comments.
//!
//! `drift patch` reads a document, patches the value tree and writes it back.
//! Writing the tree out from scratch drops comments, blank lines and number or
//! string styles. Instead the original text is parsed again, with `toml_edit`
//! which keeps all of that, and brought in line with the patched tree: whatever
//! did not change is not touched at all, so it comes out byte for byte, and
//! only the differences are edited.

use super::{parse_with, Hints, ParseOptions};
use crate::join_pointer;
use serde_json::{Map, Value};
use toml_edit::{Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table};

type Result<T> = std::result::Result<T, String>;

/// `original` with the changes that turn it into `new`, comments and layout kept.
///
/// `new` is the document as a value tree, normally `original` read and then
/// patched. Fails if `new` cannot be written as TOML (a `null`, or a root that
/// is not an object).
pub(super) fn edit(original: &str, new: &Value) -> Result<String> {
    let (old, hints) = parse_with(original, super::Format::Toml, &ParseOptions::default())
        .map_err(|e| e.to_string())?;
    let (Value::Object(old), Value::Object(new)) = (&old, new) else {
        return Err("a TOML document must be an object at the root".into());
    };
    let mut document: DocumentMut =
        original.parse().map_err(|e: toml_edit::TomlError| e.to_string())?;
    table(document.as_table_mut(), old, new, "", &hints)?;
    Ok(document.to_string())
}

/// Brings a table in line with `new`, given it currently holds `old`.
fn table(
    t: &mut Table,
    old: &Map<String, Value>,
    new: &Map<String, Value>,
    path: &str,
    hints: &Hints,
) -> Result<()> {
    for key in old.keys().filter(|key| !new.contains_key(*key)) {
        t.remove(key);
    }
    for (key, value) in new {
        let child = join_pointer(path, key);
        match (old.get(key), t.get_mut(key)) {
            (Some(before), Some(item)) => {
                if item_in_place(item, before, value, &child, hints)? {
                    // A `[table]` became a plain value or the reverse: the old
                    // key's spacing belonged to the other form.
                    if let Some(mut key) = t.key_mut(key) {
                        key.leaf_decor_mut().clear();
                    }
                }
            }
            _ => {
                t.insert(key, new_item(value, &child, hints)?);
            }
        }
    }
    Ok(())
}

/// Returns whether the item was replaced by one of a different kind.
fn item_in_place(
    item: &mut Item,
    old: &Value,
    new: &Value,
    path: &str,
    hints: &Hints,
) -> Result<bool> {
    if old == new {
        return Ok(false);
    }
    match (&mut *item, old, new) {
        (Item::Table(t), Value::Object(o), Value::Object(n)) => table(t, o, n, path, hints)?,
        // An empty list cannot stay `[[tables]]`: with no entries nothing is written.
        (Item::ArrayOfTables(a), Value::Array(o), Value::Array(n))
            if !n.is_empty() && all_objects(n) =>
        {
            tables(a, o, n, path, hints)?
        }
        (Item::Value(value), _, _) => value_in_place(value, old, new, path, hints)?,
        _ => {
            *item = new_item(new, path, hints)?;
            return Ok(true);
        }
    }
    Ok(false)
}

fn value_in_place(
    value: &mut toml_edit::Value,
    old: &Value,
    new: &Value,
    path: &str,
    hints: &Hints,
) -> Result<()> {
    if old == new {
        return Ok(());
    }
    match (&mut *value, old, new) {
        (toml_edit::Value::InlineTable(t), Value::Object(o), Value::Object(n)) => {
            inline_table(t, o, n, path, hints)
        }
        (toml_edit::Value::Array(a), Value::Array(o), Value::Array(n)) => {
            array(a, o, n, path, hints)
        }
        _ => {
            // A different value in the same spot keeps the spacing and the
            // comment that followed the old one.
            let decor = value.decor().clone();
            *value = new_value(new, path, hints)?;
            *value.decor_mut() = decor;
            Ok(())
        }
    }
}

fn inline_table(
    t: &mut InlineTable,
    old: &Map<String, Value>,
    new: &Map<String, Value>,
    path: &str,
    hints: &Hints,
) -> Result<()> {
    for key in old.keys().filter(|key| !new.contains_key(*key)) {
        t.remove(key);
    }
    for (key, value) in new {
        let child = join_pointer(path, key);
        match (old.get(key), t.get_mut(key)) {
            (Some(before), Some(slot)) => value_in_place(slot, before, value, &child, hints)?,
            _ => {
                t.insert(key, new_value(value, &child, hints)?);
            }
        }
    }
    Ok(())
}

/// What to do with the items of an array, in order, to turn `old` into `new`.
#[derive(Debug, PartialEq)]
enum Step {
    /// Leave old item `.0` alone: it equals new item `.1`.
    Keep,
    /// Old item `.0` becomes new item `.1`, edited in place.
    Edit(usize, usize),
    /// Delete the old item at the cursor.
    Remove,
    /// Insert new item `.0` at the cursor.
    Insert(usize),
}

/// Aligns two lists so that unchanged items stay as they are and only the
/// differences are edited: equal items are kept, a changed item is edited in
/// place, and extra ones are removed or inserted. Without this, deleting the
/// first of three entries would rewrite entry one into entry two, leaving its
/// comment on the wrong entry.
///
/// Works out the longest common subsequence of equal items after trimming the
/// equal ends; lists too large for that fall back to matching by position.
fn align(old: &[Value], new: &[Value]) -> Vec<Step> {
    const MAX_CELLS: usize = 250_000;
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let (mid_old, mid_new) = (&old[prefix..old.len() - suffix], &new[prefix..new.len() - suffix]);

    let mut steps: Vec<Step> = (0..prefix).map(|_| Step::Keep).collect();
    if mid_old.len().saturating_mul(mid_new.len()) > MAX_CELLS {
        gap(&mut steps, prefix..old.len() - suffix, prefix..new.len() - suffix);
    } else {
        // lcs[i][j]: length of the longest common subsequence of mid_old[i..] and mid_new[j..].
        let width = mid_new.len() + 1;
        let mut lcs = vec![0u32; (mid_old.len() + 1) * width];
        for i in (0..mid_old.len()).rev() {
            for j in (0..mid_new.len()).rev() {
                lcs[i * width + j] = if mid_old[i] == mid_new[j] {
                    lcs[(i + 1) * width + j + 1] + 1
                } else {
                    lcs[(i + 1) * width + j].max(lcs[i * width + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        let (mut gap_old, mut gap_new) = (prefix, prefix);
        while i < mid_old.len() && j < mid_new.len() {
            if mid_old[i] == mid_new[j] {
                gap(&mut steps, gap_old..prefix + i, gap_new..prefix + j);
                steps.push(Step::Keep);
                (i, j) = (i + 1, j + 1);
                (gap_old, gap_new) = (prefix + i, prefix + j);
            } else if lcs[(i + 1) * width + j] >= lcs[i * width + j + 1] {
                i += 1;
            } else {
                j += 1;
            }
        }
        gap(&mut steps, gap_old..old.len() - suffix, gap_new..new.len() - suffix);
    }
    steps.extend((0..suffix).map(|_| Step::Keep));
    steps
}

/// Between two kept items: pair the leftovers off as in-place edits, then
/// remove or insert whatever is left over on one side.
fn gap(steps: &mut Vec<Step>, old: std::ops::Range<usize>, new: std::ops::Range<usize>) {
    let paired = old.len().min(new.len());
    steps.extend((0..paired).map(|k| Step::Edit(old.start + k, new.start + k)));
    steps.extend((paired..old.len()).map(|_| Step::Remove));
    steps.extend((paired..new.len()).map(|k| Step::Insert(new.start + k)));
}

/// True when the array is written over several lines, where each item's
/// comment follows it on its line.
fn is_multiline(a: &Array) -> bool {
    a.trailing().as_str().is_some_and(|t| t.contains('\n'))
        || a.iter().any(|v| prefix(v).contains('\n'))
}

/// Text before an item: the comment ending the previous item's line, then
/// whatever leads into this one. `toml_edit` keeps both in this item's decor.
fn prefix(value: &toml_edit::Value) -> String {
    value.decor().prefix().and_then(|p| p.as_str()).unwrap_or("").to_string()
}

/// Splits text at the end of its first line: what finishes the previous item's
/// line, and what comes after.
fn after_first_line(text: &str) -> (&str, &str) {
    text.find('\n').map_or(("", text), |end| text.split_at(end + 1))
}

/// Removes an item, taking its own comment with it and leaving the others
/// where they are. A comment after an item is stored with the *next* item, so
/// a plain removal would delete the previous item's comment instead.
fn remove_item(a: &mut Array, at: usize) {
    if !is_multiline(a) {
        a.remove(at);
        return;
    }
    let removed = a.get(at).map(prefix).unwrap_or_default();
    let carry = after_first_line(&removed).0.to_string();
    if let Some(next) = a.get(at + 1) {
        let lead = after_first_line(&prefix(next)).1.to_string();
        a.remove(at);
        if let Some(next) = a.get_mut(at) {
            next.decor_mut().set_prefix(format!("{carry}{lead}"));
        }
    } else {
        let trailing = a.trailing().as_str().unwrap_or("").to_string();
        let end = after_first_line(&trailing).1.to_string();
        a.remove(at);
        a.set_trailing(format!("{carry}{end}"));
    }
}

/// Inserts an item on a line of its own, without moving the neighbours'
/// comments onto it.
fn insert_item(a: &mut Array, at: usize, mut value: toml_edit::Value) {
    if !is_multiline(a) {
        a.insert(at, value);
        return;
    }
    if let Some(following) = a.get(at) {
        let before = prefix(following);
        let (carry, lead) = after_first_line(&before);
        let indent = before.rsplit('\n').next().unwrap_or("");
        value.decor_mut().set_prefix(format!("{carry}{indent}"));
        let lead = format!("\n{lead}");
        if let Some(following) = a.get_mut(at) {
            following.decor_mut().set_prefix(lead);
        }
    } else {
        let indent = at.checked_sub(1).and_then(|last| a.get(last)).map_or_else(
            || "  ".to_string(),
            |v| prefix(v).rsplit('\n').next().unwrap_or("").to_string(),
        );
        let trailing = a.trailing().as_str().unwrap_or("").to_string();
        value.decor_mut().set_prefix(format!("{trailing}{indent}"));
        a.set_trailing("\n");
    }
    a.insert_formatted(at, value);
}

/// An inline array, item by item. The path of an edited item is its *old* one,
/// which is where the [`Hints`] for its datetimes were recorded.
fn array(a: &mut Array, old: &[Value], new: &[Value], path: &str, hints: &Hints) -> Result<()> {
    let mut at = 0;
    for step in align(old, new) {
        match step {
            Step::Keep => at += 1,
            Step::Edit(i, j) => {
                if let Some(slot) = a.get_mut(at) {
                    value_in_place(
                        slot,
                        &old[i],
                        &new[j],
                        &join_pointer(path, &i.to_string()),
                        hints,
                    )?;
                }
                at += 1;
            }
            Step::Remove => remove_item(a, at),
            Step::Insert(j) => {
                let value = new_value(&new[j], &join_pointer(path, &j.to_string()), hints)?;
                insert_item(a, at, value);
                at += 1;
            }
        }
    }
    Ok(())
}

/// An array of tables. `toml_edit` can only append to one, and a table added
/// anywhere else would print out of order, so the aligned edit is used when it
/// only appends; otherwise items are matched by position, which is always right
/// but can leave a comment on a neighbouring entry.
fn tables(
    a: &mut ArrayOfTables,
    old: &[Value],
    new: &[Value],
    path: &str,
    hints: &Hints,
) -> Result<()> {
    let mut steps = align(old, new);
    let appends_only = steps
        .iter()
        .skip_while(|step| !matches!(step, Step::Insert(_)))
        .all(|step| matches!(step, Step::Insert(_)));
    if !appends_only {
        steps.clear();
        let common = old.len().min(new.len());
        steps.extend((0..common).map(|k| Step::Edit(k, k)));
        steps.extend((common..old.len()).map(|_| Step::Remove));
        steps.extend((common..new.len()).map(Step::Insert));
    }
    let mut at = 0;
    for step in steps {
        match step {
            Step::Keep => at += 1,
            Step::Edit(i, j) => {
                if let (Some(t), Value::Object(o), Value::Object(n)) =
                    (a.get_mut(at), &old[i], &new[j])
                {
                    table(t, o, n, &join_pointer(path, &i.to_string()), hints)?;
                }
                at += 1;
            }
            Step::Remove => a.remove(at),
            Step::Insert(j) => {
                if let Item::Table(t) =
                    new_item(&new[j], &join_pointer(path, &j.to_string()), hints)?
                {
                    a.push(t);
                }
            }
        }
    }
    Ok(())
}

fn all_objects(items: &[Value]) -> bool {
    items.iter().all(Value::is_object)
}

/// A value placed in a table: objects become `[tables]` and lists of objects
/// `[[arrays of tables]]`, the usual way to write them.
fn new_item(value: &Value, path: &str, hints: &Hints) -> Result<Item> {
    Ok(match value {
        Value::Object(map) => {
            let mut t = Table::new();
            for (key, member) in map {
                t.insert(key, new_item(member, &join_pointer(path, key), hints)?);
            }
            Item::Table(t)
        }
        Value::Array(items) if !items.is_empty() && all_objects(items) => {
            let mut a = ArrayOfTables::new();
            for (index, member) in items.iter().enumerate() {
                if let Item::Table(t) =
                    new_item(member, &join_pointer(path, &index.to_string()), hints)?
                {
                    a.push(t);
                }
            }
            Item::ArrayOfTables(a)
        }
        _ => Item::Value(new_value(value, path, hints)?),
    })
}

/// A value placed inline: in an array, an inline table, or a key's right-hand side.
fn new_value(value: &Value, path: &str, hints: &Hints) -> Result<toml_edit::Value> {
    Ok(match value {
        Value::Null => return Err("null cannot be represented in TOML".into()),
        Value::Bool(b) => (*b).into(),
        Value::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => i.into(),
            (None, Some(f)) => f.into(),
            (None, None) => return Err("invalid TOML number".into()),
        },
        Value::String(text) => restored_string(text, path, hints),
        Value::Array(items) => {
            let mut a = Array::new();
            for (index, member) in items.iter().enumerate() {
                a.push(new_value(member, &join_pointer(path, &index.to_string()), hints)?);
            }
            a.into()
        }
        Value::Object(map) => {
            let mut t = InlineTable::new();
            for (key, member) in map {
                t.insert(key, new_value(member, &join_pointer(path, key), hints)?);
            }
            t.into()
        }
    })
}

/// A string where the source had a datetime or `nan`/`inf` goes back to that
/// type, if it still reads as one; see [`Hints`].
fn restored_string(text: &str, path: &str, hints: &Hints) -> toml_edit::Value {
    if hints.datetimes.contains(path) {
        if let Ok(datetime) = text.parse::<toml_edit::Datetime>() {
            return datetime.into();
        }
    }
    if hints.special_floats.contains(path) {
        match text {
            "nan" => return f64::NAN.into(),
            "inf" => return f64::INFINITY.into(),
            "-inf" => return f64::NEG_INFINITY.into(),
            _ => {}
        }
    }
    text.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::{dump, parse, Format};
    use serde_json::{json, Value};

    const SAMPLE: &str = r#"# Service configuration
# maintained by ops

title = "My App"   # display name
port = 8080
ratio = 1_000      # underscores
mask = 0xFF
quote = 'literal \n'

[server]
host = "localhost"  # bind address
tags = ["a", "b"]   # keep these

# database settings
[database]
url = "postgres://db"
timeout = 30

[[workers]]
name = "w1"   # first

[[workers]]
name = "w2"

[owner]
inline = { x = 1, y = 2 }  # point
when = 2024-05-06T07:08:09Z
"#;

    fn value(text: &str) -> Value {
        parse(text, Format::Toml).unwrap()
    }

    /// `SAMPLE` edited by `change`, written back, with a check that it reads as the edit.
    fn edited(change: impl FnOnce(&mut Value)) -> String {
        let mut new = value(SAMPLE);
        change(&mut new);
        let out = edit(SAMPLE, &new).unwrap();
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
    fn changing_one_value_changes_only_that_line_and_keeps_its_comment() {
        let out = edited(|v| v["port"] = json!(9090));
        assert_eq!(lines_differing(SAMPLE, &out), [("port = 8080".into(), "port = 9090".into())]);

        let out = edited(|v| v["title"] = json!("Other"));
        assert_eq!(
            lines_differing(SAMPLE, &out),
            [(
                r#"title = "My App"   # display name"#.into(),
                r#"title = "Other"   # display name"#.into()
            )]
        );
    }

    #[test]
    fn untouched_lines_keep_their_exact_formatting() {
        let out = edited(|v| v["port"] = json!(1));
        for line in [
            "ratio = 1_000      # underscores",
            "mask = 0xFF",
            r"quote = 'literal \n'",
            "inline = { x = 1, y = 2 }  # point",
        ] {
            assert!(out.contains(line), "lost `{line}`:\n{out}");
        }
    }

    #[test]
    fn comments_on_other_lines_and_sections_survive_every_kind_of_edit() {
        let comments = [
            "# Service configuration",
            "# maintained by ops",
            "# display name",
            "# bind address",
            "# keep these",
            "# database settings",
            "# first",
            "# point",
        ];
        let out = edited(|v| {
            v["port"] = json!(1);
            v["server"]["debug"] = json!(true);
            v["new_section"] = json!({"k": "v"});
            v["workers"][1]["name"] = json!("renamed");
        });
        for comment in comments {
            assert!(out.contains(comment), "lost `{comment}`:\n{out}");
        }
    }

    #[test]
    fn added_keys_and_sections_are_appended_where_they_belong() {
        let out = edited(|v| {
            v["server"]["debug"] = json!(true);
            v["extra"] = json!({"a": 1, "nested": {"b": [1, 2]}});
        });
        let server = out.find("[server]").unwrap();
        let database = out.find("[database]").unwrap();
        let debug = out.find("debug = true").unwrap();
        assert!(server < debug && debug < database, "{out}");
        assert!(out.contains("[extra]"), "{out}");
    }

    #[test]
    fn removed_keys_and_sections_disappear_but_their_neighbours_do_not() {
        let out = edited(|v| {
            v["database"].as_object_mut().unwrap().remove("timeout");
            v.as_object_mut().unwrap().remove("owner");
        });
        assert!(!out.contains("timeout") && !out.contains("[owner]"), "{out}");
        assert!(
            out.contains("url = \"postgres://db\"") && out.contains("# database settings"),
            "{out}"
        );
    }

    #[test]
    fn arrays_grow_shrink_and_change_in_place_keeping_the_trailing_comment() {
        let out = edited(|v| v["server"]["tags"] = json!(["a", "b", "c"]));
        assert!(out.contains(r#"tags = ["a", "b", "c"]   # keep these"#), "{out}");
        let out = edited(|v| v["server"]["tags"] = json!(["z"]));
        assert!(
            out.contains("# keep these") && out.contains(r#""z""#) && !out.contains(r#""b""#),
            "{out}"
        );
        let out = edited(|v| v["server"]["tags"] = json!([]));
        assert!(out.contains("# keep these"), "{out}");
    }

    #[test]
    fn arrays_of_tables_are_edited_added_and_removed() {
        let out = edited(|v| v["workers"][0]["name"] = json!("first"));
        assert!(out.contains(r#"name = "first"   # first"#), "{out}");
        let out = edited(|v| v["workers"].as_array_mut().unwrap().push(json!({"name": "w3"})));
        assert_eq!(out.matches("[[workers]]").count(), 3, "{out}");
        let out = edited(|v| {
            v["workers"].as_array_mut().unwrap().remove(0);
        });
        assert_eq!(out.matches("[[workers]]").count(), 1, "{out}");
        assert!(out.contains(r#"name = "w2""#) && !out.contains("# first"), "{out}");
        let out = edited(|v| v["workers"] = json!([]));
        assert!(!out.contains("[[workers]]"), "{out}");
    }

    #[test]
    fn inline_tables_are_edited_in_place() {
        let out = edited(|v| v["owner"]["inline"]["y"] = json!(99));
        assert!(
            out.contains("# point") && out.contains("y = 99") && out.contains("x = 1"),
            "{out}"
        );
        let out = edited(|v| v["owner"]["inline"]["z"] = json!(3));
        assert!(out.contains("z = 3"), "{out}");
    }

    #[test]
    fn a_value_can_change_kind() {
        let out = edited(|v| v["port"] = json!({"http": 80, "https": 443}));
        assert!(out.contains("http = 80"), "{out}");
        let out = edited(|v| v["server"] = json!("flat"));
        assert!(out.contains(r#"server = "flat""#) && !out.contains("[server]"), "{out}");
        let out = edited(|v| v["title"] = json!([1, 2]));
        assert!(out.contains("title = [1, 2]") && out.contains("# display name"), "{out}");
    }

    #[test]
    fn datetimes_stay_datetimes_whether_untouched_or_changed() {
        let untouched = edited(|v| v["port"] = json!(1));
        assert!(untouched.contains("when = 2024-05-06T07:08:09Z"), "{untouched}");
        let changed = edited(|v| v["owner"]["when"] = json!("2025-01-02T03:04:05Z"));
        assert!(
            changed.contains("when = 2025-01-02T03:04:05Z") && !changed.contains("\"2025"),
            "{changed}"
        );
    }

    #[test]
    fn a_value_toml_cannot_hold_is_an_error() {
        let mut new = value(SAMPLE);
        new["port"] = Value::Null;
        assert!(edit(SAMPLE, &new).unwrap_err().contains("null"));
        assert!(edit(SAMPLE, &json!([1])).is_err());
    }

    #[test]
    fn empty_and_commentless_documents_work() {
        let out = edit("", &json!({"a": 1, "b": {"c": [1]}})).unwrap();
        assert_eq!(value(&out), json!({"a": 1, "b": {"c": [1]}}));
        assert_eq!(edit("# only a comment\n", &json!({})).unwrap(), "# only a comment\n");
    }

    #[test]
    fn random_edits_always_read_back_as_the_edit() {
        // xorshift: deterministic, no extra dependencies.
        let mut state = 0xC13FA9A902A6328Fu64;
        let mut next = move |n: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % n
        };
        fn scalar(next: &mut dyn FnMut(u64) -> u64) -> Value {
            match next(4) {
                0 => json!(next(100) as i64 - 50),
                1 => json!(format!("s{}", next(5))),
                2 => json!(next(2) == 0),
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
        for round in 0..400 {
            let before = table(3, &mut next);
            let text = dump(&before, Format::Toml, false)
                .unwrap_or_else(|e| panic!("round {round}: {e}\n{before}"));
            let reread = value(&text);
            let mut after = reread.clone();
            // Change a few random places by rebuilding random subtrees.
            for key in ["a", "b", "c", "d", "e"] {
                match next(4) {
                    0 => {
                        after.as_object_mut().unwrap().remove(key);
                    }
                    1 => {
                        after[key] = tree(3, &mut next);
                    }
                    _ => {}
                }
            }
            let out = edit(&text, &after)
                .unwrap_or_else(|e| panic!("round {round}: {e}\n{text}\n{after}"));
            assert_eq!(value(&out), after, "round {round}\n--- before\n{text}\n--- after\n{out}");
        }
    }

    fn steps(old: Value, new: Value) -> Vec<Step> {
        align(old.as_array().unwrap(), new.as_array().unwrap())
    }

    #[test]
    fn alignment_keeps_equal_items_and_removes_or_inserts_the_rest() {
        use Step::*;
        assert_eq!(steps(json!([1, 2, 3]), json!([1, 2, 3])), [Keep, Keep, Keep]);
        assert_eq!(steps(json!([1, 2, 3]), json!([2, 3])), [Remove, Keep, Keep]);
        assert_eq!(steps(json!([1, 2, 3]), json!([1, 3])), [Keep, Remove, Keep]);
        assert_eq!(steps(json!([1, 2, 3]), json!([1, 2])), [Keep, Keep, Remove]);
        assert_eq!(steps(json!([2, 3]), json!([1, 2, 3])), [Insert(0), Keep, Keep]);
        assert_eq!(steps(json!([1, 2]), json!([1, 2, 3])), [Keep, Keep, Insert(2)]);
        assert_eq!(steps(json!([]), json!([1, 2])), [Insert(0), Insert(1)]);
        assert_eq!(steps(json!([1, 2]), json!([])), [Remove, Remove]);
    }

    #[test]
    fn a_changed_item_is_edited_in_place_not_replaced() {
        use Step::*;
        assert_eq!(steps(json!([1, 2, 3]), json!([1, 9, 3])), [Keep, Edit(1, 1), Keep]);
        // One changed and one dropped in the same gap: edit the first, remove the other.
        assert_eq!(steps(json!([1, 2, 3, 4]), json!([1, 9, 4])), [Keep, Edit(1, 1), Remove, Keep]);
        // Shifted by a removal, the surviving items still match their old selves.
        assert_eq!(steps(json!([{"a": 1}, {"a": 2}]), json!([{"a": 2}])), [Remove, Keep]);
    }

    #[test]
    fn alignment_replays_to_the_new_list() {
        // xorshift: deterministic, no extra dependencies.
        let mut state = 0x1B873593u64;
        let mut next = move |n: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % n
        };
        for _ in 0..500 {
            let make = |next: &mut dyn FnMut(u64) -> u64| -> Vec<Value> {
                (0..next(8)).map(|_| json!(next(4))).collect()
            };
            let (old, new) = (make(&mut next), make(&mut next));
            let mut list = old.clone();
            let mut at = 0;
            for step in align(&old, &new) {
                match step {
                    Step::Keep => at += 1,
                    Step::Edit(_, j) => {
                        list[at] = new[j].clone();
                        at += 1;
                    }
                    Step::Remove => {
                        list.remove(at);
                    }
                    Step::Insert(j) => {
                        list.insert(at, new[j].clone());
                        at += 1;
                    }
                }
            }
            assert_eq!(list, new, "{old:?} -> {new:?}");
        }
    }

    #[test]
    fn huge_lists_still_align_by_position() {
        let old: Vec<Value> = (0..1000).map(|i| json!(i)).collect();
        let new: Vec<Value> = (0..1000).map(|i| json!(i + 1)).collect();
        let steps = align(&old, &new);
        assert_eq!(steps.len(), 1000);
        assert!(steps.iter().all(|s| matches!(s, Step::Edit(..))));
    }

    #[test]
    fn removing_an_entry_keeps_each_comment_with_its_own_entry() {
        let text = "[[w]]\nn = 1  # one\n\n[[w]]\nn = 2  # two\n\n[[w]]\nn = 3  # three\n";
        let without = |index: usize| {
            let mut new = value(text);
            new["w"].as_array_mut().unwrap().remove(index);
            let out = edit(text, &new).unwrap();
            assert_eq!(value(&out), new, "{out}");
            out
        };
        let out = without(0);
        assert!(
            out.contains("n = 2  # two") && out.contains("n = 3  # three") && !out.contains("one"),
            "{out}"
        );
        let out = without(1);
        assert!(
            out.contains("n = 1  # one") && out.contains("n = 3  # three") && !out.contains("two"),
            "{out}"
        );
        let out = without(2);
        assert!(
            out.contains("n = 1  # one") && out.contains("n = 2  # two") && !out.contains("three"),
            "{out}"
        );
    }

    #[test]
    fn inline_arrays_keep_comments_when_an_item_is_removed_from_the_middle() {
        let text = "list = [\n  \"a\",  # first\n  \"b\",  # second\n  \"c\",  # third\n]\n";
        let new = json!({"list": ["a", "c"]});
        let out = edit(text, &new).unwrap();
        assert_eq!(value(&out), new, "{out}");
        assert!(
            out.contains("# first") && out.contains("# third") && !out.contains("# second"),
            "{out}"
        );
    }

    #[test]
    fn inserting_into_the_middle_of_an_array_of_tables_keeps_the_order() {
        // toml_edit can only append to one, so this takes the positional path; the
        // order must still come out right.
        let text = "[[w]]\nn = 1\n\n[[w]]\nn = 3\n\n[other]\nx = 1\n";
        let new = json!({"w": [{"n": 1}, {"n": 2}, {"n": 3}], "other": {"x": 1}});
        let out = edit(text, &new).unwrap();
        assert_eq!(value(&out), new, "{out}");
        let at_front = json!({"w": [{"n": 0}, {"n": 1}, {"n": 3}], "other": {"x": 1}});
        let out = edit(text, &at_front).unwrap();
        assert_eq!(value(&out), at_front, "{out}");
    }

    #[test]
    fn an_array_of_tables_can_be_emptied_and_refilled() {
        let text = "[[w]]\nn = 1\n";
        for new in [json!({"w": []}), json!({"w": [{"n": 1}, {"n": 2}]}), json!({"w": [1, 2]})] {
            let out = edit(text, &new).unwrap();
            assert_eq!(value(&out), new, "{out}");
        }
    }

    #[test]
    fn comments_in_multiline_arrays_stay_with_their_own_items() {
        // xorshift: deterministic, no extra dependencies.
        let mut state = 0x8E5E2B0F4C1A9D37u64;
        let mut next = move |n: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % n
        };
        for round in 0..400 {
            // Items v0.. with a comment each, in a multi-line array.
            let count = 1 + next(6) as usize;
            let mut text = String::from("list = [\n");
            for k in 0..count {
                text.push_str(&format!("  \"v{k}\",  # note {k}\n"));
            }
            text.push_str("]\nafter = 1\n");

            // Keep a random subset, then insert some new items (v100..) at random places.
            let mut items: Vec<String> =
                (0..count).filter(|_| next(3) != 0).map(|k| format!("v{k}")).collect();
            for fresh in 0..next(3) {
                let at = next(items.len() as u64 + 1) as usize;
                items.insert(at, format!("v{}", 100 + fresh));
            }
            let new = json!({"list": items, "after": 1});
            let out = edit(&text, &new).unwrap_or_else(|e| panic!("round {round}: {e}\n{text}"));
            assert_eq!(value(&out), new, "round {round}\n{text}\n-->\n{out}");

            for k in 0..count {
                let (item, note) = (format!("\"v{k}\""), format!("# note {k}"));
                if items.contains(&format!("v{k}")) {
                    let line = out.lines().find(|l| l.contains(&note));
                    assert!(
                        line.is_some_and(|l| l.contains(&item)),
                        "round {round}: `{note}` is not on the line of {item}\n{text}\n-->\n{out}"
                    );
                } else if let Some(line) = out.lines().find(|l| l.contains(&note)) {
                    // An item removed where another was added is an edit of that
                    // item, which keeps its comment, as `port = 1 # why` does.
                    assert!(
                        line.contains("\"v10"),
                        "round {round}: `{note}` outlived its item\n{out}"
                    );
                }
            }
            // A new item may only carry the comment of an item that was removed
            // (when it took its place), never one that was kept.
            for fresh in out.lines().filter(|l| l.contains("\"v10")) {
                for k in (0..count).filter(|k| items.contains(&format!("v{k}"))) {
                    let note = format!("# note {k}");
                    assert!(!fresh.contains(&note), "round {round}: {fresh} stole `{note}`\n{out}");
                }
            }
        }
    }
}

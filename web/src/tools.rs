//! Applying, undoing and squashing patches on pasted text.

use crate::{list, resolve};
use drift::formats::{self, Format};
use drift::{patch, Delta, DiffOptions};
use serde_json::{json, Value};

/// Turns a result into the JSON string the page reads.
pub fn reply(result: Result<Value, String>) -> String {
    let value = match result {
        Ok(mut value) => {
            value["ok"] = json!(true);
            value
        }
        Err(error) => json!({"ok": false, "error": error}),
    };
    value.to_string()
}

fn pretty(operations: &[Delta]) -> String {
    serde_json::to_string_pretty(operations).unwrap_or_default()
}

fn document(text: &str, hint: &str) -> Result<(Value, Format), String> {
    let format = resolve(hint, text);
    let value = formats::parse(text, format).map_err(|e| format!("the document: {e}"))?;
    Ok((value, format))
}

/// A patch: a JSON array of operations.
fn operations(text: &str) -> Result<Vec<Delta>, String> {
    serde_json::from_str(text)
        .map_err(|e| format!("the patch is not a JSON array of operations: {e}"))
}

pub fn apply(text: &str, hint: &str, patch_text: &str) -> Result<Value, String> {
    let (value, format) = document(text, hint)?;
    let operations = operations(patch_text)?;
    let count = operations.len();
    let patched = patch(value, &operations).map_err(|e| format!("applying the patch: {e}"))?;
    let written = formats::dump_like(text, &patched, format, false).map_err(|e| e.to_string())?;
    // Kept means the text is not what writing the result fresh gives: the layout
    // and comments of the original survived.
    let fresh = formats::dump(&patched, format, false).map_err(|e| e.to_string())?;
    Ok(json!({
        "text": written,
        "format": format.as_str(),
        "operations": count,
        "kept": written != fresh,
        "preserving": matches!(format, Format::Yaml | Format::Toml),
    }))
}

pub fn invert(text: &str, hint: &str, patch_text: &str) -> Result<Value, String> {
    let (value, _) = document(text, hint)?;
    let operations = operations(patch_text)?;
    let undo =
        drift::invert(&value, &operations).map_err(|e| format!("inverting the patch: {e}"))?;
    Ok(json!({"patch": pretty(&undo), "before": operations.len(), "after": undo.len()}))
}

/// `patches` is one patch, or an array of patches applied in turn.
fn patches(text: &str) -> Result<Vec<Delta>, String> {
    let value: Value = serde_json::from_str(text).map_err(|e| format!("the patches: {e}"))?;
    match &value {
        Value::Array(items) if items.iter().all(Value::is_array) && !items.is_empty() => {
            let mut all = Vec::new();
            for (at, item) in items.iter().enumerate() {
                let one: Vec<Delta> = serde_json::from_value(item.clone())
                    .map_err(|e| format!("patch {}: {e}", at + 1))?;
                all.extend(one);
            }
            Ok(all)
        }
        _ => operations(text),
    }
}

pub fn compose(
    text: &str,
    hint: &str,
    patches_text: &str,
    array_keys: &str,
    ignore_paths: &str,
) -> Result<Value, String> {
    let (value, _) = document(text, hint)?;
    let operations = patches(patches_text)?;
    let options = list(array_keys).fold(DiffOptions::new(), |o, key| o.array_key(key));
    let options = list(ignore_paths).fold(options, |o, pattern| o.ignore_path(pattern));
    let squashed =
        drift::compose(&value, &operations, &options).map_err(|e| format!("squashing: {e}"))?;
    Ok(json!({"patch": pretty(&squashed), "before": operations.len(), "after": squashed.len()}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applying_to_toml_keeps_the_comments_and_to_json_has_none_to_keep() {
        let toml = "port = 80  # http\nhost = \"a\"\n";
        let patch = r#"[{"op":"replace","path":"/port","value":8080}]"#;
        let out = apply(toml, "toml", patch).unwrap();
        assert_eq!(out["text"], "port = 8080  # http\nhost = \"a\"\n");
        assert_eq!(out["kept"], true);
        let out =
            apply(r#"{"a":1}"#, "json", r#"[{"op":"replace","path":"/a","value":2}]"#).unwrap();
        assert_eq!(out["kept"], false);
        assert_eq!(out["preserving"], false);
    }

    #[test]
    fn applying_to_yaml_keeps_comments() {
        let yaml = "# service\nname: web  # the name\nreplicas: 2\n";
        let patch = r#"[{"op":"replace","path":"/replicas","value":5}]"#;
        let out = apply(yaml, "yaml", patch).unwrap();
        assert_eq!(out["text"], "# service\nname: web  # the name\nreplicas: 5\n");
        assert_eq!(out["kept"], true);
    }

    #[test]
    fn a_patch_that_does_not_apply_or_parse_is_an_error() {
        let doc = r#"{"a":1}"#;
        let e = apply(doc, "json", r#"[{"op":"remove","path":"/zzz"}]"#).unwrap_err();
        assert!(e.contains("applying the patch"), "{e}");
        let e = apply(doc, "json", "{").unwrap_err();
        assert!(e.contains("not a JSON array"), "{e}");
        assert!(apply("{", "json", "[]").unwrap_err().contains("the document"));
    }

    #[test]
    fn inverting_undoes_the_patch() {
        let doc = r#"{"a":1,"b":[1,2]}"#;
        let patch_text =
            r#"[{"op":"replace","path":"/a","value":2},{"op":"remove","path":"/b/0"}]"#;
        let undo = invert(doc, "json", patch_text).unwrap();
        let after = apply(doc, "json", patch_text).unwrap()["text"].as_str().unwrap().to_string();
        let back = apply(&after, "json", undo["patch"].as_str().unwrap()).unwrap();
        let parsed: Value = serde_json::from_str(back["text"].as_str().unwrap()).unwrap();
        assert_eq!(parsed, serde_json::from_str::<Value>(doc).unwrap());
    }

    #[test]
    fn squashing_several_patches_gives_one_with_the_same_effect() {
        let doc = r#"{"count":1}"#;
        let two = r#"[[{"op":"replace","path":"/count","value":2}],[{"op":"replace","path":"/count","value":3}]]"#;
        let out = compose(doc, "json", two, "", "").unwrap();
        assert_eq!((out["before"].as_u64(), out["after"].as_u64()), (Some(2), Some(1)));
        // A single flat patch is accepted too.
        let flat = r#"[{"op":"replace","path":"/count","value":2}]"#;
        assert_eq!(compose(doc, "json", flat, "", "").unwrap()["after"], 1);
        assert!(compose(doc, "json", "[[{\"op\":\"bad\"}]]", "", "").is_err());
    }
}

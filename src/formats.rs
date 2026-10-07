//! Parsing and serialising the document formats drift understands.
//!
//! Every format is converted to and from a [`serde_json::Value`], which is what
//! [`crate::diff`] and [`crate::patch`] operate on.

use crate::join_pointer;
use crate::DriftError;
use serde_json::Value;
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Json,
    Yaml,
    Toml,
    Xml,
}

impl Format {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Yaml => "yaml",
            Self::Toml => "toml",
            Self::Xml => "xml",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "json" => Some(Self::Json),
            "yaml" | "yml" => Some(Self::Yaml),
            "toml" => Some(Self::Toml),
            "xml" => Some(Self::Xml),
            _ => None,
        }
    }

    /// Format implied by a file name's extension, if it has a known one.
    pub fn from_extension(path: &str) -> Option<Self> {
        let extension = path.rsplit_once('.')?.1;
        Self::from_name(extension)
    }

    /// Guesses the format of pasted text that has no file name.
    ///
    /// `<` starts XML; otherwise the first parser that accepts the text wins,
    /// JSON then TOML then YAML. YAML goes last because it accepts almost any
    /// text, including `key = value` lines as a plain scalar.
    pub fn sniff(text: &str) -> Self {
        let trimmed = text.trim_start();
        if trimmed.starts_with('<') {
            return Self::Xml;
        }
        if parse(text, Self::Json).is_ok() {
            return Self::Json;
        }
        if parse(text, Self::Toml).is_ok() {
            return Self::Toml;
        }
        if parse(text, Self::Yaml).is_ok() {
            return Self::Yaml;
        }
        Self::Json
    }
}

/// Reading options.
#[derive(Debug, Clone, Default)]
pub struct ParseOptions {
    /// XML only: always represent child elements as arrays, even a single one,
    /// so a list with one entry keeps its shape instead of reading as a scalar.
    pub xml_arrays: bool,
}

/// What the source document knew that JSON cannot express.
///
/// TOML datetimes and `nan`/`inf` floats become JSON strings, so a document
/// read, patched and written back would otherwise turn them into quoted
/// strings. The paths where they were found are kept here, and [`dump_with`]
/// uses them to write those values back as their original type.
#[derive(Debug, Clone, Default)]
pub struct Hints {
    datetimes: BTreeSet<String>,
    special_floats: BTreeSet<String>,
}

pub fn parse(text: &str, format: Format) -> Result<Value, DriftError> {
    parse_with(text, format, &ParseOptions::default()).map(|(value, _)| value)
}

/// Like [`parse`], also returning [`Hints`] for writing the document back.
pub fn parse_with(
    text: &str,
    format: Format,
    options: &ParseOptions,
) -> Result<(Value, Hints), DriftError> {
    let error = |e: &dyn std::fmt::Display| DriftError::Parse(format!("{}: {e}", format.as_str()));
    let mut hints = Hints::default();
    let value = match format {
        Format::Json => serde_json::from_str(text).map_err(|e| error(&e))?,
        Format::Yaml => serde_yaml::from_str(text).map_err(|e| error(&e))?,
        Format::Toml => {
            let table: toml::Value = toml::from_str(text).map_err(|e| error(&e))?;
            toml_to_json(table, "", &mut hints)
        }
        Format::Xml => xml_to_json(text, options).map_err(|e| error(&*e))?,
    };
    Ok((value, hints))
}

pub fn dump(value: &Value, format: Format, compact: bool) -> Result<String, DriftError> {
    dump_with(value, format, compact, &Hints::default())
}

/// Like [`dump`], restoring the types recorded in `hints`.
pub fn dump_with(
    value: &Value,
    format: Format,
    compact: bool,
    hints: &Hints,
) -> Result<String, DriftError> {
    let error = |e: &dyn std::fmt::Display| DriftError::Parse(format!("{}: {e}", format.as_str()));
    match format {
        Format::Json if compact => serde_json::to_string(value).map_err(|e| error(&e)),
        Format::Json => serde_json::to_string_pretty(value).map_err(|e| error(&e)),
        Format::Yaml => serde_yaml::to_string(value).map_err(|e| error(&e)),
        Format::Toml => {
            toml::to_string_pretty(&json_to_toml(value, "", hints).map_err(|e| error(&*e))?)
                .map_err(|e| error(&e))
        }
        Format::Xml => json_to_xml(value).map_err(|e| error(&*e)),
    }
}

fn toml_to_json(value: toml::Value, path: &str, hints: &mut Hints) -> Value {
    match value {
        toml::Value::String(v) => Value::String(v),
        toml::Value::Integer(v) => Value::Number(v.into()),
        // JSON has no NaN or infinity; keep them as strings rather than losing them.
        toml::Value::Float(v) => serde_json::Number::from_f64(v).map_or_else(
            || {
                hints.special_floats.insert(path.into());
                Value::String(
                    if v.is_nan() {
                        "nan"
                    } else if v > 0.0 {
                        "inf"
                    } else {
                        "-inf"
                    }
                    .into(),
                )
            },
            Value::Number,
        ),
        toml::Value::Boolean(v) => Value::Bool(v),
        toml::Value::Datetime(v) => {
            hints.datetimes.insert(path.into());
            Value::String(v.to_string())
        }
        toml::Value::Array(v) => Value::Array(
            v.into_iter()
                .enumerate()
                .map(|(i, item)| toml_to_json(item, &join_pointer(path, &i.to_string()), hints))
                .collect(),
        ),
        toml::Value::Table(v) => Value::Object(
            v.into_iter()
                .map(|(k, item)| {
                    let child = join_pointer(path, &k);
                    (k, toml_to_json(item, &child, hints))
                })
                .collect(),
        ),
    }
}

fn json_to_toml(
    value: &Value,
    path: &str,
    hints: &Hints,
) -> Result<toml::Value, Box<dyn std::error::Error>> {
    Ok(match value {
        Value::Object(map) => toml::Value::Table(
            map.iter()
                .map(|(key, value)| {
                    Ok((key.clone(), json_to_toml(value, &join_pointer(path, key), hints)?))
                })
                .collect::<Result<_, Box<dyn std::error::Error>>>()?,
        ),
        Value::Array(values) => toml::Value::Array(
            values
                .iter()
                .enumerate()
                .map(|(i, value)| json_to_toml(value, &join_pointer(path, &i.to_string()), hints))
                .collect::<Result<_, _>>()?,
        ),
        Value::String(text) => restore_toml_type(text, path, hints),
        Value::Number(value) => value
            .as_i64()
            .map(toml::Value::Integer)
            .or_else(|| value.as_f64().map(toml::Value::Float))
            .ok_or("invalid TOML number")?,
        Value::Bool(value) => toml::Value::Boolean(*value),
        Value::Null => return Err("null cannot be represented in TOML".into()),
    })
}

/// A string at a path that held a datetime or `nan`/`inf` in the source goes
/// back to that type, if it still reads as one. Anything else stays a string.
fn restore_toml_type(text: &str, path: &str, hints: &Hints) -> toml::Value {
    if hints.datetimes.contains(path) {
        if let Ok(datetime) = text.parse::<toml::value::Datetime>() {
            return toml::Value::Datetime(datetime);
        }
    }
    if hints.special_floats.contains(path) {
        match text {
            "nan" => return toml::Value::Float(f64::NAN),
            "inf" => return toml::Value::Float(f64::INFINITY),
            "-inf" => return toml::Value::Float(f64::NEG_INFINITY),
            _ => {}
        }
    }
    toml::Value::String(text.into())
}

fn xml_to_json(text: &str, options: &ParseOptions) -> Result<Value, Box<dyn std::error::Error>> {
    let doc = roxmltree::Document::parse(text)?;
    /// Name as written in the source, with its namespace prefix.
    fn qualified(node: roxmltree::Node, local: &str, uri: Option<&str>) -> String {
        match uri.and_then(|uri| node.lookup_prefix(uri)).filter(|prefix| !prefix.is_empty()) {
            Some(prefix) => format!("{prefix}:{local}"),
            None => local.to_string(),
        }
    }
    fn element(node: roxmltree::Node, always_array: bool) -> Value {
        let mut object = serde_json::Map::new();
        // Namespace declarations made on this element (not inherited ones).
        for ns in node.namespaces() {
            let inherited = node.parent_element().is_some_and(|parent| {
                parent.namespaces().any(|p| p.name() == ns.name() && p.uri() == ns.uri())
            });
            if !inherited {
                let key = match ns.name() {
                    Some(prefix) => format!("@xmlns:{prefix}"),
                    None => "@xmlns".to_string(),
                };
                object.insert(key, Value::String(ns.uri().into()));
            }
        }
        for attr in node.attributes() {
            let name = qualified(node, attr.name(), attr.namespace());
            object.insert(format!("@{name}"), Value::String(attr.value().into()));
        }
        for child in node.children().filter(|node| node.is_element()) {
            let value = element(child, always_array);
            let tag = child.tag_name();
            let key = qualified(child, tag.name(), tag.namespace());
            if always_array {
                match object.entry(key).or_insert_with(|| Value::Array(Vec::new())) {
                    Value::Array(values) => values.push(value),
                    _ => unreachable!("with always_array every child entry is an array"),
                }
            } else if let Some(existing) = object.get_mut(&key) {
                if let Value::Array(values) = existing {
                    values.push(value);
                } else {
                    *existing = Value::Array(vec![existing.take(), value]);
                }
            } else {
                object.insert(key, value);
            }
        }
        // All text nodes, joined: ordering relative to child elements is lost.
        let text = node
            .children()
            .filter(|child| child.is_text())
            .filter_map(|child| child.text())
            .map(str::trim)
            .filter(|piece| !piece.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if object.is_empty() {
            Value::String(text)
        } else {
            if !text.is_empty() {
                object.insert("#text".into(), Value::String(text));
            }
            Value::Object(object)
        }
    }
    let root = doc.root_element();
    let tag = root.tag_name();
    let name = qualified(root, tag.name(), tag.namespace());
    Ok(Value::Object([(name, element(root, options.xml_arrays))].into_iter().collect()))
}

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn json_to_xml(value: &Value) -> Result<String, Box<dyn std::error::Error>> {
    fn element(tag: &str, value: &Value, output: &mut String) {
        output.push('<');
        output.push_str(tag);
        if let Value::Object(map) = value {
            for (key, value) in map {
                if let Some(attr) = key.strip_prefix('@') {
                    let text = match value {
                        Value::String(text) => text.clone(),
                        other => other.to_string(),
                    };
                    output.push_str(&format!(" {}=\"{}\"", attr, escape_xml(&text)));
                }
            }
        }
        output.push('>');
        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    if key.starts_with('@') {
                        continue;
                    }
                    if key == "#text" {
                        output.push_str(&escape_xml(value.as_str().unwrap_or("")));
                    } else if let Value::Array(values) = value {
                        for value in values {
                            element(key, value, output);
                        }
                    } else {
                        element(key, value, output);
                    }
                }
            }
            Value::String(value) => output.push_str(&escape_xml(value)),
            Value::Number(value) => output.push_str(&value.to_string()),
            Value::Bool(value) => output.push_str(&value.to_string()),
            Value::Null | Value::Array(_) => {}
        }
        output.push_str("</");
        output.push_str(tag);
        output.push('>');
    }
    let Value::Object(map) = value else {
        return Err("XML documents must have exactly one root element".into());
    };
    if map.len() != 1 {
        return Err("XML documents must have exactly one root element".into());
    }
    let (tag, value) = map.iter().next().unwrap();
    let mut output = String::new();
    element(tag, value, &mut output);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extension_and_name_detection() {
        assert_eq!(Format::from_extension("a/b.yml"), Some(Format::Yaml));
        assert_eq!(Format::from_extension("cfg.TOML"), Some(Format::Toml));
        assert_eq!(Format::from_extension("noext"), None);
        assert_eq!(Format::from_name("XML"), Some(Format::Xml));
    }

    #[test]
    fn sniffing_pasted_text() {
        assert_eq!(Format::sniff("  {\"a\": 1}"), Format::Json);
        assert_eq!(Format::sniff("[1, 2]"), Format::Json);
        assert_eq!(Format::sniff("<a><b/></a>"), Format::Xml);
        assert_eq!(Format::sniff("[server]\nport = 80\n"), Format::Toml);
        assert_eq!(Format::sniff("a: 1\nb:\n  - x\n"), Format::Yaml);
    }

    #[test]
    fn parse_errors_name_the_format() {
        let message = parse("{", Format::Json).unwrap_err().to_string();
        assert!(message.contains("json"), "{message}");
    }

    #[test]
    fn xml_attributes_are_not_json_quoted() {
        let xml = json_to_xml(&json!({"a": {"@id": "x", "#text": "hi"}})).unwrap();
        assert_eq!(xml, "<a id=\"x\">hi</a>");
    }

    #[test]
    fn xml_special_characters_are_escaped_and_round_trip() {
        let value = json!({"a": {"@t": "\"q\" & <r>", "#text": "1 < 2 & 3"}});
        let xml = json_to_xml(&value).unwrap();
        assert!(!xml.contains("< 2"));
        assert_eq!(parse(&xml, Format::Xml).unwrap(), value);
    }

    #[test]
    fn xml_namespaces_are_preserved() {
        let xml =
            r#"<a:root xmlns:a="urn:a" xmlns="urn:d"><a:item a:id="1">x</a:item><plain/></a:root>"#;
        let value = parse(xml, Format::Xml).unwrap();
        let root = &value["a:root"];
        assert_eq!(root["@xmlns:a"], "urn:a");
        assert_eq!(root["@xmlns"], "urn:d");
        assert_eq!(root["a:item"]["@a:id"], "1");
        assert_eq!(root["a:item"]["#text"], "x");
        // Round trip keeps the document equivalent.
        assert_eq!(parse(&json_to_xml(&value).unwrap(), Format::Xml).unwrap(), value);
    }

    #[test]
    fn xml_mixed_content_keeps_all_text() {
        let value = parse("<p>Hello <b>big</b> world</p>", Format::Xml).unwrap();
        assert_eq!(value["p"]["#text"], "Hello world");
        assert_eq!(value["p"]["b"], "big");
    }

    #[test]
    fn toml_non_finite_floats_are_not_dropped() {
        let value = parse("a = nan\nb = inf\nc = -inf\nd = 1.5\n", Format::Toml).unwrap();
        assert_eq!(value["a"], "nan");
        assert_eq!(value["b"], "inf");
        assert_eq!(value["c"], "-inf");
        assert_eq!(value["d"], 1.5);
    }

    #[test]
    fn xml_lang_attribute_keeps_its_prefix() {
        let value = parse(r#"<a xml:lang="en">x</a>"#, Format::Xml).unwrap();
        assert_eq!(value["a"]["@xml:lang"], "en");
    }

    #[test]
    fn toml_datetimes_and_special_floats_survive_a_patch() {
        let text = "when = 2024-05-06T07:08:09Z\nday = 2024-05-06\nratio = nan\nname = \"x\"\n";
        let (value, hints) = parse_with(text, Format::Toml, &ParseOptions::default()).unwrap();
        let mut value = value;
        value["name"] = "y".into();
        value["day"] = "2025-01-02".into();
        let out = dump_with(&value, Format::Toml, false, &hints).unwrap();
        let reread: toml::Value = toml::from_str(&out).unwrap();
        assert!(matches!(reread["when"], toml::Value::Datetime(_)), "{out}");
        assert_eq!(reread["day"].as_datetime().unwrap().to_string(), "2025-01-02");
        assert!(matches!(reread["day"], toml::Value::Datetime(_)), "{out}");
        assert!(reread["ratio"].as_float().is_some_and(f64::is_nan), "{out}");
        assert_eq!(reread["name"].as_str(), Some("y"));
    }

    #[test]
    fn quoted_strings_that_look_like_dates_stay_strings() {
        let (value, hints) =
            parse_with("a = \"2024-05-06\"\n", Format::Toml, &ParseOptions::default()).unwrap();
        let out = dump_with(&value, Format::Toml, false, &hints).unwrap();
        assert!(toml::from_str::<toml::Value>(&out).unwrap()["a"].is_str(), "{out}");
    }

    #[test]
    fn xml_arrays_option_keeps_single_children_as_lists() {
        let xml = "<r><item>1</item></r>";
        let default = parse(xml, Format::Xml).unwrap();
        assert_eq!(default["r"]["item"], "1");
        let options = ParseOptions { xml_arrays: true };
        let (listed, _) = parse_with(xml, Format::Xml, &options).unwrap();
        assert_eq!(listed["r"]["item"], serde_json::json!(["1"]));
        // The list shape round-trips.
        let text = dump(&listed, Format::Xml, false).unwrap();
        assert_eq!(parse_with(&text, Format::Xml, &options).unwrap().0, listed);
    }

    #[test]
    fn key_order_survives_a_round_trip() {
        let cases = [
            (Format::Json, "{\"zebra\": 1, \"apple\": 2, \"mango\": 3}"),
            (Format::Yaml, "zebra: 1\napple: 2\nmango: 3\n"),
            (Format::Toml, "zebra = 1\napple = 2\nmango = 3\n"),
            (Format::Xml, "<r><zebra>1</zebra><apple>2</apple><mango>3</mango></r>"),
        ];
        for (format, text) in cases {
            let value = parse(text, format).unwrap();
            let out = dump(&value, format, false).unwrap();
            let at = |name: &str| out.find(name).unwrap_or_else(|| panic!("{name} in {out}"));
            assert!(
                at("zebra") < at("apple") && at("apple") < at("mango"),
                "{}: {out}",
                format.as_str()
            );
        }
    }
}

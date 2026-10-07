//! Parsing and serialising the document formats drift understands.
//!
//! Every format is converted to and from a [`serde_json::Value`], which is what
//! [`crate::diff`] and [`crate::patch`] operate on.

use crate::DriftError;
use serde_json::Value;

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

pub fn parse(text: &str, format: Format) -> Result<Value, DriftError> {
    let error = |e: &dyn std::fmt::Display| DriftError::Parse(format!("{}: {e}", format.as_str()));
    match format {
        Format::Json => serde_json::from_str(text).map_err(|e| error(&e)),
        Format::Yaml => serde_yaml::from_str(text).map_err(|e| error(&e)),
        Format::Toml => {
            toml::from_str(text).map(toml_to_json).map_err(|e: toml::de::Error| error(&e))
        }
        Format::Xml => xml_to_json(text).map_err(|e| error(&*e)),
    }
}

pub fn dump(value: &Value, format: Format, compact: bool) -> Result<String, DriftError> {
    let error = |e: &dyn std::fmt::Display| DriftError::Parse(format!("{}: {e}", format.as_str()));
    match format {
        Format::Json if compact => serde_json::to_string(value).map_err(|e| error(&e)),
        Format::Json => serde_json::to_string_pretty(value).map_err(|e| error(&e)),
        Format::Yaml => serde_yaml::to_string(value).map_err(|e| error(&e)),
        Format::Toml => toml::to_string_pretty(&json_to_toml(value).map_err(|e| error(&*e))?)
            .map_err(|e| error(&e)),
        Format::Xml => json_to_xml(value).map_err(|e| error(&*e)),
    }
}

fn toml_to_json(value: toml::Value) -> Value {
    match value {
        toml::Value::String(v) => Value::String(v),
        toml::Value::Integer(v) => Value::Number(v.into()),
        // JSON has no NaN or infinity; keep them as strings rather than losing them.
        toml::Value::Float(v) => serde_json::Number::from_f64(v).map_or_else(
            || {
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
        toml::Value::Datetime(v) => Value::String(v.to_string()),
        toml::Value::Array(v) => Value::Array(v.into_iter().map(toml_to_json).collect()),
        toml::Value::Table(v) => {
            Value::Object(v.into_iter().map(|(k, v)| (k, toml_to_json(v))).collect())
        }
    }
}

fn json_to_toml(value: &Value) -> Result<toml::Value, Box<dyn std::error::Error>> {
    Ok(match value {
        Value::Object(map) => toml::Value::Table(
            map.iter()
                .map(|(key, value)| Ok((key.clone(), json_to_toml(value)?)))
                .collect::<Result<_, Box<dyn std::error::Error>>>()?,
        ),
        Value::Array(values) => {
            toml::Value::Array(values.iter().map(json_to_toml).collect::<Result<_, _>>()?)
        }
        Value::String(value) => toml::Value::String(value.clone()),
        Value::Number(value) => value
            .as_i64()
            .map(toml::Value::Integer)
            .or_else(|| value.as_f64().map(toml::Value::Float))
            .ok_or("invalid TOML number")?,
        Value::Bool(value) => toml::Value::Boolean(*value),
        Value::Null => return Err("null cannot be represented in TOML".into()),
    })
}

fn xml_to_json(text: &str) -> Result<Value, Box<dyn std::error::Error>> {
    let doc = roxmltree::Document::parse(text)?;
    /// Name as written in the source, with its namespace prefix.
    fn qualified(node: roxmltree::Node, local: &str, uri: Option<&str>) -> String {
        match uri.and_then(|uri| node.lookup_prefix(uri)).filter(|prefix| !prefix.is_empty()) {
            Some(prefix) => format!("{prefix}:{local}"),
            None => local.to_string(),
        }
    }
    fn element(node: roxmltree::Node) -> Value {
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
            let value = element(child);
            let tag = child.tag_name();
            let key = qualified(child, tag.name(), tag.namespace());
            if let Some(existing) = object.get_mut(&key) {
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
    Ok(Value::Object([(name, element(root))].into_iter().collect()))
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
        assert_eq!(xml_to_json(&xml).unwrap(), value);
    }

    #[test]
    fn xml_namespaces_are_preserved() {
        let xml =
            r#"<a:root xmlns:a="urn:a" xmlns="urn:d"><a:item a:id="1">x</a:item><plain/></a:root>"#;
        let value = xml_to_json(xml).unwrap();
        let root = &value["a:root"];
        assert_eq!(root["@xmlns:a"], "urn:a");
        assert_eq!(root["@xmlns"], "urn:d");
        assert_eq!(root["a:item"]["@a:id"], "1");
        assert_eq!(root["a:item"]["#text"], "x");
        // Round trip keeps the document equivalent.
        assert_eq!(xml_to_json(&json_to_xml(&value).unwrap()).unwrap(), value);
    }

    #[test]
    fn xml_mixed_content_keeps_all_text() {
        let value = xml_to_json("<p>Hello <b>big</b> world</p>").unwrap();
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
        let value = xml_to_json(r#"<a xml:lang="en">x</a>"#).unwrap();
        assert_eq!(value["a"]["@xml:lang"], "en");
    }
}

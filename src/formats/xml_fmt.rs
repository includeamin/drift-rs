//! XML support: attributes become `@name`, text becomes `#text`.

use super::ParseOptions;
use serde_json::Value;

pub(super) fn parse(text: &str, options: &ParseOptions) -> Result<Value, String> {
    xml_to_json(text, options).map_err(|e| e.to_string())
}

pub(super) fn dump(value: &Value) -> Result<String, String> {
    json_to_xml(value).map_err(|e| e.to_string())
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

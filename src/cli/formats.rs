use serde_json::Value;
use std::{
    fs,
    io::{self, Read},
    path::Path,
};

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum Format {
    Json,
    Yaml,
    Toml,
    Xml,
}

pub fn resolve(explicit: Option<Format>, path: &str) -> Format {
    explicit.unwrap_or_else(|| match Path::new(path).extension().and_then(|e| e.to_str()) {
        Some("yaml") | Some("yml") => Format::Yaml,
        Some("toml") => Format::Toml,
        Some("xml") => Format::Xml,
        _ => Format::Json,
    })
}

pub fn load(path: &str, format: Format) -> Result<Value, Box<dyn std::error::Error>> {
    let text = if path == "-" {
        let mut text = String::new();
        io::stdin().read_to_string(&mut text)?;
        text
    } else {
        fs::read_to_string(path)?
    };
    Ok(match format {
        Format::Json => serde_json::from_str(&text)?,
        Format::Yaml => serde_yaml::from_str(&text)?,
        Format::Toml => toml_to_json(toml::from_str(&text)?),
        Format::Xml => xml_to_json(&text)?,
    })
}

pub fn dump(
    value: &Value,
    format: Format,
    compact: bool,
) -> Result<String, Box<dyn std::error::Error>> {
    Ok(match format {
        Format::Json => {
            if compact {
                serde_json::to_string(value)?
            } else {
                serde_json::to_string_pretty(value)?
            }
        }
        Format::Yaml => serde_yaml::to_string(value)?,
        Format::Toml => toml::to_string_pretty(&json_to_toml(value)?)?,
        Format::Xml => json_to_xml(value)?,
    })
}

fn toml_to_json(value: toml::Value) -> Value {
    match value {
        toml::Value::String(v) => Value::String(v),
        toml::Value::Integer(v) => Value::Number(v.into()),
        toml::Value::Float(v) => serde_json::Number::from_f64(v).map_or(Value::Null, Value::Number),
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
    fn element(node: roxmltree::Node) -> Value {
        let mut object = serde_json::Map::new();
        for attr in node.attributes() {
            object.insert(format!("@{}", attr.name()), Value::String(attr.value().into()));
        }
        for child in node.children().filter(|node| node.is_element()) {
            let value = element(child);
            let key = child.tag_name().name().to_string();
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
        let text = node.text().unwrap_or("").trim();
        if object.is_empty() {
            Value::String(text.into())
        } else {
            if !text.is_empty() {
                object.insert("#text".into(), Value::String(text.into()));
            }
            Value::Object(object)
        }
    }
    let root = doc.root_element();
    Ok(Value::Object([(root.tag_name().name().into(), element(root))].into_iter().collect()))
}

fn json_to_xml(value: &Value) -> Result<String, Box<dyn std::error::Error>> {
    fn element(tag: &str, value: &Value, output: &mut String) {
        output.push('<');
        output.push_str(tag);
        if let Value::Object(map) = value {
            for (key, value) in map {
                if let Some(attr) = key.strip_prefix('@') {
                    output.push_str(&format!(
                        " {}=\"{}\"",
                        attr,
                        value.to_string().replace('"', "&quot;")
                    ));
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
                        output.push_str(value.as_str().unwrap_or(""));
                    } else if let Value::Array(values) = value {
                        for value in values {
                            element(key, value, output);
                        }
                    } else {
                        element(key, value, output);
                    }
                }
            }
            Value::String(value) => output.push_str(value),
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

use clap::ValueEnum;
use drift::formats;
use serde_json::Value;
use std::{
    fs,
    io::{self, Read},
};

#[derive(Clone, Copy, ValueEnum)]
pub enum Format {
    Json,
    Yaml,
    Toml,
    Xml,
}

impl From<Format> for formats::Format {
    fn from(format: Format) -> Self {
        match format {
            Format::Json => Self::Json,
            Format::Yaml => Self::Yaml,
            Format::Toml => Self::Toml,
            Format::Xml => Self::Xml,
        }
    }
}

pub fn resolve(explicit: Option<Format>, path: &str) -> formats::Format {
    explicit
        .map(Into::into)
        .or_else(|| formats::Format::from_extension(path))
        .unwrap_or(formats::Format::Json)
}

pub fn load(
    path: &str,
    format: formats::Format,
    xml_arrays: bool,
) -> Result<(Value, formats::Hints), Box<dyn std::error::Error>> {
    let text = if path == "-" {
        let mut text = String::new();
        io::stdin().read_to_string(&mut text)?;
        text
    } else {
        fs::read_to_string(path)?
    };
    Ok(formats::parse_with(&text, format, &formats::ParseOptions { xml_arrays })?)
}

pub fn dump(
    value: &Value,
    format: formats::Format,
    compact: bool,
    hints: &formats::Hints,
) -> Result<String, Box<dyn std::error::Error>> {
    Ok(formats::dump_with(value, format, compact, hints)?)
}

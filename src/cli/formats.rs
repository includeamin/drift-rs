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

/// The text of a file, or of standard input for `-`.
pub fn read(path: &str) -> Result<String, Box<dyn std::error::Error>> {
    Ok(if path == "-" {
        let mut text = String::new();
        io::stdin().read_to_string(&mut text)?;
        text
    } else {
        fs::read_to_string(path)?
    })
}

pub fn load(
    path: &str,
    format: formats::Format,
    xml_arrays: bool,
) -> Result<(Value, formats::Hints), Box<dyn std::error::Error>> {
    Ok(formats::parse_with(&read(path)?, format, &formats::ParseOptions { xml_arrays })?)
}

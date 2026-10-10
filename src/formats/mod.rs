//! Parsing and serialising the document formats drift understands.
//!
//! Every format is converted to and from a [`serde_json::Value`], which is what
//! [`crate::diff`] and [`crate::patch`] operate on.

use crate::DriftError;
use serde_json::Value;
use std::collections::BTreeSet;

/// A document format.
///
/// Every variant exists whatever the crate features; reading or writing a
/// format whose feature is off returns an error naming the feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// JSON. Always available.
    Json,
    /// YAML, needs the `yaml` feature.
    Yaml,
    /// TOML, needs the `toml` feature.
    Toml,
    /// XML, needs the `xml` feature.
    Xml,
}

impl Format {
    /// Lower-case name: `"json"`, `"yaml"`, `"toml"` or `"xml"`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Yaml => "yaml",
            Self::Toml => "toml",
            Self::Xml => "xml",
        }
    }

    /// The format called `name` (case-insensitive; `yml` means YAML).
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
// Only the TOML module reads these.
#[cfg_attr(not(feature = "toml"), allow(dead_code))]
#[derive(Debug, Clone, Default)]
pub struct Hints {
    datetimes: BTreeSet<String>,
    special_floats: BTreeSet<String>,
}

/// Parses `text` as `format` into a JSON value tree.
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
        Format::Yaml => yaml(text).map_err(|e| error(&e))?,
        Format::Toml => toml(text, &mut hints).map_err(|e| error(&e))?,
        Format::Xml => xml(text, options).map_err(|e| error(&e))?,
    };
    Ok((value, hints))
}

/// Writes `value` as `format`. `compact` only affects JSON.
///
/// TOML cannot represent `null` and needs a table at the root; XML needs
/// exactly one root element. Both return an error otherwise.
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
        Format::Yaml => yaml_dump(value).map_err(|e| error(&e)),
        Format::Toml => toml_dump(value, hints).map_err(|e| error(&e)),
        Format::Xml => xml_dump(value).map_err(|e| error(&e)),
    }
}

/// Writes `new` as `format`, keeping as much as it can of how `original` was
/// written.
///
/// `original` is the text `new` was derived from, normally by reading it and
/// patching the result. For TOML the original is edited in place: comments,
/// blank lines, key order and number or string styles survive wherever the
/// value did not change, and a changed value keeps the comment after it. The
/// result is checked to read back as `new`; if it does not, or the original
/// cannot be edited in place, the document is written out fresh as [`dump_with`]
/// does, which is always correct but drops the comments.
///
/// Every other format is written fresh. JSON has no comments to keep; YAML and
/// XML do, but are not preserved yet.
///
/// ```
/// # #[cfg(feature = "toml")]
/// # {
/// use drift::formats::{dump_like, parse, Format};
///
/// let original = "port = 80  # http\nhost = \"a\"\n";
/// let mut value = parse(original, Format::Toml)?;
/// value["port"] = 8080.into();
/// assert_eq!(
///     dump_like(original, &value, Format::Toml, false)?,
///     "port = 8080  # http\nhost = \"a\"\n"
/// );
/// # }
/// # Ok::<(), drift::DriftError>(())
/// ```
pub fn dump_like(
    original: &str,
    new: &Value,
    format: Format,
    compact: bool,
) -> Result<String, DriftError> {
    if let Some(text) = edit_in_place(original, new, format) {
        return Ok(text);
    }
    let hints = parse_with(original, format, &ParseOptions::default())
        .map(|(_, hints)| hints)
        .unwrap_or_default();
    dump_with(new, format, compact, &hints)
}

/// `original` edited to hold `new`, when the format supports it and the result
/// reads back as `new`.
fn edit_in_place(original: &str, new: &Value, format: Format) -> Option<String> {
    #[cfg(feature = "toml")]
    if format == Format::Toml {
        let text = toml_preserve::edit(original, new).ok()?;
        return (parse(&text, Format::Toml).ok().as_ref() == Some(new)).then_some(text);
    }
    let _ = (original, new, format);
    None
}

// One adapter per optional format: the real one with its feature on, otherwise
// an error that says which feature to enable.
macro_rules! format_adapters {
    ($feature:literal, $module:ident, $name:literal;
     $parse:ident($($parg:ident: $pty:ty),*), $dump:ident($($darg:ident: $dty:ty),*)) => {
        #[cfg(feature = $feature)]
        fn $parse(text: &str $(, $parg: $pty)*) -> Result<Value, String> {
            $module::parse(text $(, $parg)*)
        }
        #[cfg(feature = $feature)]
        fn $dump(value: &Value $(, $darg: $dty)*) -> Result<String, String> {
            $module::dump(value $(, $darg)*)
        }
        #[cfg(not(feature = $feature))]
        fn $parse(_text: &str $(, $parg: $pty)*) -> Result<Value, String> {
            $(let _ = $parg;)*
            Err(disabled($name, $feature))
        }
        #[cfg(not(feature = $feature))]
        fn $dump(_value: &Value $(, $darg: $dty)*) -> Result<String, String> {
            $(let _ = $darg;)*
            Err(disabled($name, $feature))
        }
    };
}

// Unused when every format is enabled.
#[allow(dead_code)]
fn disabled(name: &str, feature: &str) -> String {
    format!("{name} support is not compiled in; enable the `{feature}` feature")
}

#[cfg(feature = "toml")]
mod toml_fmt;
#[cfg(feature = "toml")]
mod toml_preserve;
#[cfg(feature = "xml")]
mod xml_fmt;
#[cfg(feature = "yaml")]
mod yaml_fmt;

format_adapters!("yaml", yaml_fmt, "YAML"; yaml(), yaml_dump());
format_adapters!("toml", toml_fmt, "TOML"; toml(hints: &mut Hints), toml_dump(hints: &Hints));
format_adapters!("xml", xml_fmt, "XML"; xml(options: &ParseOptions), xml_dump());

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "xml")]
    use serde_json::json;

    #[test]
    fn extension_and_name_detection() {
        assert_eq!(Format::from_extension("a/b.yml"), Some(Format::Yaml));
        assert_eq!(Format::from_extension("cfg.TOML"), Some(Format::Toml));
        assert_eq!(Format::from_extension("noext"), None);
        assert_eq!(Format::from_name("XML"), Some(Format::Xml));
    }

    #[test]
    #[cfg(all(feature = "yaml", feature = "toml", feature = "xml"))]
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
    #[cfg(feature = "xml")]
    fn xml_attributes_are_not_json_quoted() {
        let xml = dump(&json!({"a": {"@id": "x", "#text": "hi"}}), Format::Xml, false).unwrap();
        assert_eq!(xml, "<a id=\"x\">hi</a>");
    }

    #[test]
    #[cfg(feature = "xml")]
    fn xml_special_characters_are_escaped_and_round_trip() {
        let value = json!({"a": {"@t": "\"q\" & <r>", "#text": "1 < 2 & 3"}});
        let xml = dump(&value, Format::Xml, false).unwrap();
        assert!(!xml.contains("< 2"));
        assert_eq!(parse(&xml, Format::Xml).unwrap(), value);
    }

    #[test]
    #[cfg(feature = "xml")]
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
        assert_eq!(parse(&dump(&value, Format::Xml, false).unwrap(), Format::Xml).unwrap(), value);
    }

    #[test]
    #[cfg(feature = "xml")]
    fn xml_mixed_content_keeps_all_text() {
        let value = parse("<p>Hello <b>big</b> world</p>", Format::Xml).unwrap();
        assert_eq!(value["p"]["#text"], "Hello world");
        assert_eq!(value["p"]["b"], "big");
    }

    #[test]
    #[cfg(feature = "toml")]
    fn toml_non_finite_floats_are_not_dropped() {
        let value = parse("a = nan\nb = inf\nc = -inf\nd = 1.5\n", Format::Toml).unwrap();
        assert_eq!(value["a"], "nan");
        assert_eq!(value["b"], "inf");
        assert_eq!(value["c"], "-inf");
        assert_eq!(value["d"], 1.5);
    }

    #[test]
    #[cfg(feature = "xml")]
    fn xml_lang_attribute_keeps_its_prefix() {
        let value = parse(r#"<a xml:lang="en">x</a>"#, Format::Xml).unwrap();
        assert_eq!(value["a"]["@xml:lang"], "en");
    }

    #[test]
    #[cfg(feature = "toml")]
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
    #[cfg(feature = "toml")]
    fn quoted_strings_that_look_like_dates_stay_strings() {
        let (value, hints) =
            parse_with("a = \"2024-05-06\"\n", Format::Toml, &ParseOptions::default()).unwrap();
        let out = dump_with(&value, Format::Toml, false, &hints).unwrap();
        assert!(toml::from_str::<toml::Value>(&out).unwrap()["a"].is_str(), "{out}");
    }

    #[test]
    #[cfg(feature = "xml")]
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
    #[cfg(all(feature = "yaml", feature = "toml", feature = "xml"))]
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

    #[test]
    #[cfg(not(feature = "yaml"))]
    fn disabled_yaml_says_which_feature_to_enable() {
        let message = parse("a: 1", Format::Yaml).unwrap_err().to_string();
        assert!(message.contains("`yaml` feature"), "{message}");
    }

    #[test]
    #[cfg(not(feature = "toml"))]
    fn disabled_toml_says_which_feature_to_enable() {
        let message = dump(&serde_json::json!({}), Format::Toml, false).unwrap_err().to_string();
        assert!(message.contains("`toml` feature"), "{message}");
    }

    #[test]
    #[cfg(not(feature = "xml"))]
    fn disabled_xml_says_which_feature_to_enable() {
        let message = parse("<a/>", Format::Xml).unwrap_err().to_string();
        assert!(message.contains("`xml` feature"), "{message}");
    }

    #[test]
    #[cfg(feature = "xml")]
    fn deeply_nested_xml_is_an_error_not_a_crash() {
        let nested = |depth: usize| format!("{}x{}", "<a>".repeat(depth), "</a>".repeat(depth));
        // The same limit as JSON and YAML: 128 levels are fine, 129 are not.
        assert!(parse(&nested(128), Format::Xml).is_ok());
        let message = parse(&nested(129), Format::Xml).unwrap_err().to_string();
        assert!(message.contains("recursion limit"), "{message}");
        // Far past what the stack could hold; must fail cleanly.
        assert!(parse(&nested(200_000), Format::Xml).is_err());
    }

    #[test]
    #[cfg(feature = "toml")]
    fn dump_like_keeps_a_toml_documents_comments_and_layout() {
        let original = "# top\nname = \"x\"  # who\nport = 80\n\n[db]\nurl = 'u'  # where\n";
        let mut value = parse(original, Format::Toml).unwrap();
        value["port"] = serde_json::json!(81);
        let out = dump_like(original, &value, Format::Toml, false).unwrap();
        assert_eq!(out, original.replace("port = 80", "port = 81"));
    }

    #[test]
    #[cfg(feature = "toml")]
    fn dump_like_falls_back_to_a_plain_write_for_what_cannot_be_edited_in_place() {
        // A root that is not an object cannot be edited in place; the error is the plain writer's.
        let error = dump_like("a = 1\n", &serde_json::json!([1]), Format::Toml, false);
        assert!(error.is_err());
        // Text that is not valid TOML is written out fresh rather than failing.
        let out =
            dump_like("not = = toml", &serde_json::json!({"a": 1}), Format::Toml, false).unwrap();
        assert_eq!(parse(&out, Format::Toml).unwrap(), serde_json::json!({"a": 1}));
    }

    #[test]
    fn dump_like_is_a_plain_write_for_formats_that_keep_no_layout() {
        let value = serde_json::json!({"b": 1, "a": [1, 2]});
        for format in [Format::Json, Format::Yaml, Format::Xml] {
            if format == Format::Xml {
                continue; // needs a single root element
            }
            #[cfg(not(feature = "yaml"))]
            if format == Format::Yaml {
                continue;
            }
            assert_eq!(
                dump_like("ignored", &value, format, false).unwrap(),
                dump(&value, format, false).unwrap()
            );
        }
    }
}

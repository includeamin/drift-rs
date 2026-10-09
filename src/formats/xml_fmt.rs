//! XML support: attributes become `@name`, text becomes `#text`.

use super::ParseOptions;
use serde_json::Value;

/// Deepest element nesting accepted; the same limit serde_json and serde_yaml
/// apply, so a hostile document cannot overflow the stack.
const MAX_DEPTH: usize = 128;

pub(super) fn parse(text: &str, options: &ParseOptions) -> Result<Value, String> {
    xml_to_json(text, options).map_err(|e| e.to_string())
}

pub(super) fn dump(value: &Value) -> Result<String, String> {
    json_to_xml(value).map_err(|e| e.to_string())
}

fn xml_to_json(text: &str, options: &ParseOptions) -> Result<Value, Box<dyn std::error::Error>> {
    check_depth(text)?;
    let doc = roxmltree::Document::parse(text)?;
    /// Name as written in the source, with its namespace prefix.
    fn qualified(node: roxmltree::Node, local: &str, uri: Option<&str>) -> String {
        match uri.and_then(|uri| node.lookup_prefix(uri)).filter(|prefix| !prefix.is_empty()) {
            Some(prefix) => format!("{prefix}:{local}"),
            None => local.to_string(),
        }
    }
    fn element(node: roxmltree::Node, always_array: bool, depth: usize) -> Result<Value, String> {
        if depth > MAX_DEPTH {
            return Err("recursion limit exceeded".into());
        }
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
            let value = element(child, always_array, depth + 1)?;
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
        Ok(if object.is_empty() {
            Value::String(text)
        } else {
            if !text.is_empty() {
                object.insert("#text".into(), Value::String(text));
            }
            Value::Object(object)
        })
    }
    let root = doc.root_element();
    let tag = root.tag_name();
    let name = qualified(root, tag.name(), tag.namespace());
    Ok(Value::Object([(name, element(root, options.xml_arrays, 1)?)].into_iter().collect()))
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

/// Rejects input nested deeper than [`MAX_DEPTH`] before the XML parser sees it.
///
/// `roxmltree` has no depth limit and overflows the stack on deeply nested
/// input (about 10,000 levels, which is only ~60 KB of `<a>`), so the check has
/// to come first, and it must not recurse.
fn check_depth(text: &str) -> Result<(), String> {
    if scan(text, MAX_DEPTH) > MAX_DEPTH {
        return Err("recursion limit exceeded".into());
    }
    Ok(())
}

/// The deepest element nesting in `text`, found by a single non-recursive pass
/// over its markup. Stops early, returning a value above `limit`, once the
/// nesting exceeds it.
///
/// It only has to be right for well-formed XML: anything else is rejected by
/// the parser afterwards. Comments, CDATA, processing instructions and
/// DOCTYPE declarations are skipped, and `>` inside a quoted attribute value
/// does not end a tag.
fn scan(text: &str, limit: usize) -> usize {
    let bytes = text.as_bytes();
    let find = |from: usize, pattern: &[u8]| {
        bytes.get(from..)?.windows(pattern.len()).position(|w| w == pattern).map(|at| from + at)
    };
    // Index of the `>` that ends a tag or declaration starting at `from`,
    // skipping quoted strings and, for a DOCTYPE, its `[ ... ]` subset.
    let end_of_markup = |from: usize, brackets: bool| {
        let (mut quote, mut open) = (0u8, 0usize);
        for (at, &byte) in bytes.iter().enumerate().skip(from) {
            if quote != 0 {
                if byte == quote {
                    quote = 0;
                }
                continue;
            }
            match byte {
                b'"' | b'\'' => quote = byte,
                b'[' if brackets => open += 1,
                b']' if brackets => open = open.saturating_sub(1),
                b'>' if open == 0 => return Some(at),
                _ => {}
            }
        }
        None
    };

    let (mut at, mut depth, mut deepest) = (0, 0usize, 0usize);
    while let Some(offset) = bytes.get(at..).and_then(|rest| rest.iter().position(|&b| b == b'<')) {
        at += offset;
        let rest = &bytes[at..];
        let next = if rest.starts_with(b"<!--") {
            find(at + 4, b"-->").map(|end| end + 3)
        } else if rest.starts_with(b"<![CDATA[") {
            find(at + 9, b"]]>").map(|end| end + 3)
        } else if rest.starts_with(b"<?") {
            find(at + 2, b"?>").map(|end| end + 2)
        } else if rest.starts_with(b"<!") {
            end_of_markup(at + 2, true).map(|end| end + 1)
        } else if rest.starts_with(b"</") {
            depth = depth.saturating_sub(1);
            end_of_markup(at + 2, false).map(|end| end + 1)
        } else {
            end_of_markup(at + 1, false).map(|end| {
                if bytes[end - 1] == b'/' {
                    deepest = deepest.max(depth + 1);
                } else {
                    depth += 1;
                    deepest = deepest.max(depth);
                }
                end + 1
            })
        };
        if deepest > limit {
            return deepest;
        }
        match next {
            Some(next) => at = next,
            None => break,
        }
    }
    deepest
}

#[cfg(test)]
mod tests {
    use super::*;

    fn depth(text: &str) -> usize {
        scan(text, usize::MAX)
    }

    #[test]
    fn counts_element_nesting() {
        assert_eq!(depth("<a/>"), 1);
        assert_eq!(depth("<a></a>"), 1);
        assert_eq!(depth("<a><b><c/></b><d/></a>"), 3);
        assert_eq!(depth("<a>text<b>more</b></a>"), 2);
        assert_eq!(depth(""), 0);
    }

    #[test]
    fn markup_inside_comments_cdata_and_instructions_does_not_count() {
        assert_eq!(depth("<a><!-- <b><c><d> --></a>"), 1);
        assert_eq!(depth("<a><![CDATA[ <b><c> ]]></a>"), 1);
        assert_eq!(depth("<?xml version=\"1.0\"?><?pi <x><y> ?><a/>"), 1);
        assert_eq!(depth("<!-- --> <!-- <a> --><a><b/></a>"), 2);
    }

    #[test]
    fn quoted_attribute_values_may_contain_angle_brackets_and_slashes() {
        assert_eq!(depth(r#"<a x=">" y='/>'><b t="<c>"/></a>"#), 2);
        assert_eq!(depth(r#"<a x="a/>b"></a>"#), 1);
        // A `/>` inside the quotes must not make the tag look self-closing.
        assert_eq!(depth(r#"<a x="/>"><b><c/></b></a>"#), 3);
        assert_eq!(depth("<a x='\"'><b y=\"'\"/></a>"), 2);
    }

    #[test]
    fn doctype_with_an_internal_subset_is_skipped() {
        let xml = r#"<!DOCTYPE r [ <!ENTITY e "<x><y>"> <!ELEMENT r ANY> ]><r><s/></r>"#;
        assert_eq!(depth(xml), 2);
    }

    #[test]
    fn tags_split_over_lines_and_with_whitespace_before_the_bracket() {
        assert_eq!(depth("<a\n  x='1'\n>\n<b\n/></a\n>"), 2);
        assert_eq!(depth("<a ><b /></a >"), 2);
    }

    #[test]
    fn rejects_input_nested_too_deep_without_recursing() {
        let nested = |n: usize| format!("{}{}", "<a>".repeat(n), "</a>".repeat(n));
        assert!(check_depth(&nested(MAX_DEPTH)).is_ok());
        assert_eq!(check_depth(&nested(MAX_DEPTH + 1)).unwrap_err(), "recursion limit exceeded");
        // 100 MB of nesting would overflow any recursive parser; this is a flat loop.
        assert!(check_depth(&"<a>".repeat(5_000_000)).is_err());
    }

    #[test]
    fn matches_what_the_real_parser_sees_on_generated_documents() {
        // xorshift: deterministic, no extra dependencies.
        let mut state = 0xD1B54A32D192ED03u64;
        let mut next = move |n: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % n
        };
        fn build(out: &mut String, depth: usize, next: &mut dyn FnMut(u64) -> u64) {
            let attr =
                ["", " k=\">\"", " k='/>'", " k=\"a>b\"", " a='1' b=\"2\""][next(5) as usize];
            let selfclose = next(3) == 0 || depth == 0;
            out.push_str(&format!("<e{attr}"));
            if selfclose {
                out.push_str(if next(2) == 0 { "/>" } else { " />" });
                return;
            }
            out.push('>');
            for _ in 0..next(4) {
                match next(5) {
                    0 => out.push_str("<!-- <x><y> -->"),
                    1 => out.push_str("<![CDATA[ <p><q> ]]>"),
                    2 => out.push_str("text &amp; more"),
                    _ => build(out, depth - 1, next),
                }
            }
            out.push_str("</e>");
        }
        for _ in 0..500 {
            let mut xml = String::new();
            build(&mut xml, next(8) as usize, &mut next);
            let doc = roxmltree::Document::parse(&xml).unwrap_or_else(|e| panic!("{e}: {xml}"));
            let real = doc
                .descendants()
                .filter(|n| n.is_element())
                .map(|n| n.ancestors().filter(|a| a.is_element()).count())
                .max()
                .unwrap_or(0);
            assert_eq!(depth(&xml), real, "{xml}");
        }
    }
}

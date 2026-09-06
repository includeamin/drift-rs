//! Byte-offset index for the members of a top-level JSON object.
//!
//! Scanning records where each member's value begins and ends without building
//! any [`Value`], which makes the file randomly accessible: a later pass can
//! seek straight to one member and parse or stream just that range.

use crate::DriftError;
use std::io::BufRead;

/// What a member's value starts with, used to pick a diff strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Array,
    Object,
    Scalar,
}

/// A top-level object member and the byte range of its value.
#[derive(Debug, Clone)]
pub(crate) struct Member {
    pub key: String,
    pub start: u64,
    pub end: u64,
    pub kind: Kind,
}

impl Member {
    pub fn len(&self) -> u64 {
        self.end - self.start
    }
}

fn unexpected_eof() -> DriftError {
    DriftError::Parse("unexpected end of input while scanning".into())
}

fn unexpected(byte: u8, at: u64) -> DriftError {
    DriftError::Parse(format!("unexpected byte {:?} at offset {}", byte as char, at))
}

/// Byte cursor that tracks its absolute offset.
struct Scanner<R: BufRead> {
    reader: R,
    pos: u64,
    peeked: Option<u8>,
}

impl<R: BufRead> Scanner<R> {
    fn new(reader: R) -> Self {
        Scanner { reader, pos: 0, peeked: None }
    }

    fn peek(&mut self) -> Result<Option<u8>, DriftError> {
        if self.peeked.is_none() {
            let buffer = self.reader.fill_buf().map_err(|e| DriftError::Io(e.to_string()))?;
            if buffer.is_empty() {
                return Ok(None);
            }
            let byte = buffer[0];
            self.reader.consume(1);
            self.peeked = Some(byte);
        }
        Ok(self.peeked)
    }

    fn next(&mut self) -> Result<Option<u8>, DriftError> {
        let byte = self.peek()?;
        if byte.is_some() {
            self.peeked = None;
            self.pos += 1;
        }
        Ok(byte)
    }

    fn expect(&mut self, expected: u8) -> Result<(), DriftError> {
        let at = self.pos;
        match self.next()? {
            Some(byte) if byte == expected => Ok(()),
            Some(byte) => Err(unexpected(byte, at)),
            None => Err(unexpected_eof()),
        }
    }

    fn skip_whitespace(&mut self) -> Result<(), DriftError> {
        while let Some(byte) = self.peek()? {
            if !byte.is_ascii_whitespace() {
                break;
            }
            self.next()?;
        }
        Ok(())
    }

    /// Consumes a string body, assuming the opening quote is already consumed.
    ///
    /// A backslash always escapes exactly one byte, which is what makes `\\`
    /// terminate correctly and `\"` not terminate at all. `\uXXXX` needs no
    /// special handling: the hex digits are ordinary bytes.
    fn skip_string_body(&mut self) -> Result<(), DriftError> {
        loop {
            match self.next()?.ok_or_else(unexpected_eof)? {
                b'\\' => {
                    self.next()?.ok_or_else(unexpected_eof)?;
                }
                b'"' => return Ok(()),
                _ => {}
            }
        }
    }

    /// Reads a complete JSON string token, quotes included.
    fn read_string_raw(&mut self) -> Result<Vec<u8>, DriftError> {
        self.expect(b'"')?;
        let mut raw = vec![b'"'];
        loop {
            let byte = self.next()?.ok_or_else(unexpected_eof)?;
            raw.push(byte);
            match byte {
                b'\\' => raw.push(self.next()?.ok_or_else(unexpected_eof)?),
                b'"' => return Ok(raw),
                _ => {}
            }
        }
    }

    /// Consumes exactly one JSON value, reporting what kind it was.
    fn skip_value(&mut self) -> Result<Kind, DriftError> {
        match self.peek()?.ok_or_else(unexpected_eof)? {
            open @ (b'{' | b'[') => {
                let kind = if open == b'{' { Kind::Object } else { Kind::Array };
                let mut depth = 0usize;
                loop {
                    match self.next()?.ok_or_else(unexpected_eof)? {
                        b'"' => self.skip_string_body()?,
                        b'{' | b'[' => depth += 1,
                        b'}' | b']' => {
                            depth -= 1;
                            if depth == 0 {
                                return Ok(kind);
                            }
                        }
                        _ => {}
                    }
                }
            }
            b'"' => {
                self.next()?;
                self.skip_string_body()?;
                Ok(Kind::Scalar)
            }
            _ => {
                while let Some(byte) = self.peek()? {
                    if matches!(byte, b',' | b'}' | b']') || byte.is_ascii_whitespace() {
                        break;
                    }
                    self.next()?;
                }
                Ok(Kind::Scalar)
            }
        }
    }
}

/// Indexes the members of a top-level JSON object.
///
/// Later members win on duplicate keys, matching `serde_json`.
pub(crate) fn scan_object_members<R: BufRead>(reader: R) -> Result<Vec<Member>, DriftError> {
    let mut scanner = Scanner::new(reader);
    let mut members = Vec::new();

    scanner.skip_whitespace()?;
    scanner.expect(b'{')?;
    scanner.skip_whitespace()?;

    if scanner.peek()? == Some(b'}') {
        scanner.next()?;
        return Ok(members);
    }

    loop {
        scanner.skip_whitespace()?;
        let raw_key = scanner.read_string_raw()?;
        let key: String = serde_json::from_slice(&raw_key)
            .map_err(|error| DriftError::Parse(error.to_string()))?;

        scanner.skip_whitespace()?;
        scanner.expect(b':')?;
        scanner.skip_whitespace()?;

        let start = scanner.pos;
        let kind = scanner.skip_value()?;
        let end = scanner.pos;
        members.push(Member { key, start, end, kind });

        scanner.skip_whitespace()?;
        let at = scanner.pos;
        match scanner.next()?.ok_or_else(unexpected_eof)? {
            b',' => continue,
            b'}' => return Ok(members),
            byte => return Err(unexpected(byte, at)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn scan(text: &str) -> Vec<Member> {
        scan_object_members(text.as_bytes()).unwrap()
    }

    /// Every recorded range must reparse to the value `serde_json` sees.
    fn assert_ranges_match(text: &str) {
        let whole: Value = serde_json::from_str(text).unwrap();
        let object = whole.as_object().unwrap();
        let members = scan(text);

        assert_eq!(members.len(), object.len(), "member count mismatch for {text}");

        for member in &members {
            let slice = &text.as_bytes()[member.start as usize..member.end as usize];
            let parsed: Value = serde_json::from_slice(slice)
                .unwrap_or_else(|e| panic!("range for {:?} did not parse: {e}", member.key));
            assert_eq!(
                &parsed,
                object.get(&member.key).unwrap(),
                "value mismatch for {:?}",
                member.key
            );
        }
    }

    #[test]
    fn indexes_simple_members() {
        assert_ranges_match(r#"{"a":1,"b":"two","c":true,"d":null}"#);
    }

    #[test]
    fn indexes_nested_containers() {
        assert_ranges_match(r#"{"arr":[1,[2,3],{"x":4}],"obj":{"y":{"z":[5]}}}"#);
    }

    #[test]
    fn handles_whitespace_everywhere() {
        assert_ranges_match("{\n  \"a\" : [ 1 , 2 ] ,\n  \"b\" :\t{ \"c\" : 3 }\n}");
    }

    #[test]
    fn handles_braces_inside_strings() {
        assert_ranges_match(r#"{"a":"{[}]","b":"not,a,separator","c":[ "}" , "]" ]}"#);
    }

    #[test]
    fn handles_escaped_quotes_and_backslashes() {
        // "\\" ends the string; "\"" does not.
        assert_ranges_match(r#"{"a":"back\\slash","b":"quote\"inside","c":"trailing\\"}"#);
    }

    #[test]
    fn handles_unicode_escapes() {
        assert_ranges_match(r#"{"a":"\u0041\u007b","b":"\ud83d\ude00"}"#);
    }

    #[test]
    fn handles_escaped_keys() {
        let members = scan(r#"{"a\/b":1,"c\\d":2,"e\"f":3,"\u0041":4}"#);
        let keys: Vec<&str> = members.iter().map(|m| m.key.as_str()).collect();
        assert_eq!(keys, vec!["a/b", "c\\d", "e\"f", "A"]);
    }

    #[test]
    fn handles_numbers_with_exponents() {
        assert_ranges_match(r#"{"a":-1.5e10,"b":0,"c":1e-3}"#);
    }

    #[test]
    fn handles_empty_object() {
        assert_eq!(scan("{}").len(), 0);
        assert_eq!(scan("  {  }  ").len(), 0);
    }

    #[test]
    fn handles_empty_containers_as_values() {
        assert_ranges_match(r#"{"a":{},"b":[],"c":""}"#);
    }

    #[test]
    fn records_kinds() {
        let members = scan(r#"{"a":[1],"b":{"x":1},"c":3,"d":"s"}"#);
        let kinds: Vec<Kind> = members.iter().map(|m| m.kind).collect();
        assert_eq!(kinds, vec![Kind::Array, Kind::Object, Kind::Scalar, Kind::Scalar]);
    }

    #[test]
    fn later_duplicate_keys_are_kept_by_callers() {
        // The scanner reports both; callers collect into a map so the last wins,
        // which is what serde_json does.
        let members = scan(r#"{"a":1,"a":2}"#);
        assert_eq!(members.len(), 2);
        let map: std::collections::BTreeMap<_, _> =
            members.into_iter().map(|m| (m.key.clone(), m)).collect();
        let text = r#"{"a":1,"a":2}"#;
        let member = &map["a"];
        let slice = &text.as_bytes()[member.start as usize..member.end as usize];
        assert_eq!(serde_json::from_slice::<Value>(slice).unwrap(), json!(2));
    }

    #[test]
    fn rejects_non_object_roots() {
        assert!(scan_object_members("[1,2]".as_bytes()).is_err());
        assert!(scan_object_members("null".as_bytes()).is_err());
    }

    #[test]
    fn rejects_truncated_input() {
        assert!(scan_object_members(r#"{"a":[1,2"#.as_bytes()).is_err());
        assert!(scan_object_members(r#"{"a""#.as_bytes()).is_err());
        assert!(scan_object_members(r#"{"a":"unterminated"#.as_bytes()).is_err());
    }

    /// Generated documents with adversarial strings, checked against serde_json.
    #[test]
    fn generated_documents_match_serde_json() {
        let payloads = [
            r#""plain""#,
            r#""with \"quotes\" and \\ backslash""#,
            r#""braces {} brackets [] comma ,""#,
            r#""trailing backslash \\""#,
            r#""\u0000\u001f\uffff""#,
            "[]",
            "{}",
            "[1,2,[3,[4,[5]]]]",
            r#"{"n":{"n":{"n":[{"deep":true}]}}}"#,
            "-0.0",
            "1e100",
            "true",
            "false",
            "null",
            r#"[{"a":"}"},{"b":"]"}]"#,
        ];

        for (i, first) in payloads.iter().enumerate() {
            for (j, second) in payloads.iter().enumerate() {
                let text = format!(r#"{{"k{i}":{first},"k{j}_b":{second}}}"#);
                assert_ranges_match(&text);
            }
        }
    }
}

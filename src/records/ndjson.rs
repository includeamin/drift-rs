//! Reading NDJSON: one JSON value per line, cut up at line starts so a file can be
//! indexed on several threads.

use super::{fingerprint, hash_bytes, hash_key, open, trim, Failure, Fault, KeySpec, Part, Result};
use crate::DriftError;
use serde::de::{DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::Value;
use std::cell::{Cell, RefCell};
use std::fmt;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

/// Reads the key parts out of one line without building the rest of the row: the
/// other fields are skipped as they are read, which allocates nothing.
pub(super) fn read_key(line: &[u8], spec: &KeySpec) -> std::result::Result<Vec<Value>, Fault> {
    let tokens: Vec<&[String]> = spec.parts.iter().map(|p| p.tokens.as_slice()).collect();
    let found = RefCell::new(vec![None; tokens.len()]);
    let object = Cell::new(false);
    let active: Vec<usize> = (0..tokens.len()).collect();
    let mut deserializer = serde_json::Deserializer::from_slice(line);
    let seed =
        KeySeed { parts: &tokens, depth: 0, active: &active, found: &found, object: &object };
    seed.deserialize(&mut deserializer).map_err(Fault::Json)?;
    deserializer.end().map_err(Fault::Json)?;
    if !object.get() {
        return Err(Fault::NotObject);
    }
    found
        .into_inner()
        .into_iter()
        .enumerate()
        .map(|(at, part)| match part {
            None => Err(Fault::NoKey(at)),
            Some(key @ (Value::String(_) | Value::Number(_) | Value::Bool(_))) => Ok(key),
            Some(_) => Err(Fault::BadKey(at)),
        })
        .collect()
}

/// Walks one value, keeping the parts of the key found at `parts[..][depth..]`
/// (for the parts in `active`) and skipping everything else.
struct KeySeed<'a> {
    parts: &'a [&'a [String]],
    depth: usize,
    active: &'a [usize],
    found: &'a RefCell<Vec<Option<Value>>>,
    /// Set when the value being walked is an object.
    object: &'a Cell<bool>,
}

impl<'de> DeserializeSeed<'de> for KeySeed<'_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for KeySeed<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<(), A::Error> {
        self.object.set(true);
        while let Some(name) = map.next_key::<std::borrow::Cow<str>>()? {
            let hit = self.active.iter().copied().find(|&p| self.parts[p][self.depth] == name);
            match hit {
                None => {
                    map.next_value::<IgnoredAny>()?;
                }
                Some(part) if self.parts[part].len() == self.depth + 1 => {
                    self.found.borrow_mut()[part] = Some(map.next_value::<Value>()?);
                }
                Some(_) => {
                    // Parts continuing under this name (none ends here: no part
                    // lies inside another).
                    let deeper: Vec<usize> = self
                        .active
                        .iter()
                        .copied()
                        .filter(|&p| self.parts[p][self.depth] == name)
                        .collect();
                    let inner = Cell::new(false);
                    map.next_value_seed(KeySeed {
                        parts: self.parts,
                        depth: self.depth + 1,
                        active: &deeper,
                        found: self.found,
                        object: &inner,
                    })?;
                }
            }
        }
        Ok(())
    }

    // Anything that is not an object holds no key; arrays still have to be read
    // to their end.
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<(), A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(())
    }
    fn visit_bool<E>(self, _: bool) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_i64<E>(self, _: i64) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_u64<E>(self, _: u64) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_f64<E>(self, _: f64) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_str<E>(self, _: &str) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_unit<E>(self) -> std::result::Result<(), E> {
        Ok(())
    }
}

/// Cuts a file of `len` bytes into up to `parts` stretches that each start at the
/// beginning of a line.
pub(super) fn chunk_bounds(path: &Path, len: u64, parts: usize) -> Result<Vec<(u64, u64)>> {
    let mut starts = vec![0u64];
    if parts > 1 && len > 0 {
        let mut reader = BufReader::new(open(path)?);
        for k in 1..parts as u64 {
            let target = (len * k / parts as u64).max(*starts.last().expect("starts is not empty"));
            let mut skipped = Vec::new();
            let moved = reader
                .seek(SeekFrom::Start(target))
                .and_then(|_| reader.read_until(b'\n', &mut skipped))
                .map_err(|e| DriftError::Io(format!("{}: {e}", path.display())))?;
            let start = target + moved as u64;
            if start < len && start > *starts.last().expect("starts is not empty") {
                starts.push(start);
            }
        }
    }
    let mut bounds: Vec<(u64, u64)> = starts.windows(2).map(|w| (w[0], w[1])).collect();
    bounds.push((*starts.last().expect("starts is not empty"), len));
    Ok(bounds)
}

/// Indexes the lines that start in `start..stop`.
pub(super) fn index_part(
    path: &Path,
    start: u64,
    stop: u64,
    spec: &KeySpec,
) -> std::result::Result<Part, Failure> {
    let io = |e: std::io::Error| Failure {
        offset: start,
        fault: Fault::Json(serde::de::Error::custom(format!("{}: {e}", path.display()))),
    };
    let mut file = File::open(path).map_err(io)?;
    file.seek(SeekFrom::Start(start)).map_err(io)?;
    let mut reader = BufReader::with_capacity(1 << 20, file.take(stop - start));
    let mut part = Part { keys: Vec::new(), hashes: Vec::new(), offsets: Vec::new() };
    let (mut buffer, mut offset) = (Vec::new(), start);
    loop {
        buffer.clear();
        let read = reader.read_until(b'\n', &mut buffer).map_err(io)?;
        if read == 0 {
            return Ok(part);
        }
        let line_start = offset;
        offset += read as u64;
        let text = trim(&buffer);
        if text.is_empty() {
            continue;
        }
        let key = if spec.whole_row {
            // The whole row is the key, so it has to be read in full.
            let row: Value = serde_json::from_slice(text)
                .map_err(|e| Failure { offset: line_start, fault: Fault::Json(e) })?;
            fingerprint(&row)
        } else {
            let parts =
                read_key(text, spec).map_err(|fault| Failure { offset: line_start, fault })?;
            hash_key(&parts)
        };
        part.keys.push(key);
        part.hashes.push(hash_bytes(text));
        part.offsets.push(line_start);
    }
}

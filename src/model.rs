use serde::de::Error as _;
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// The kind of an RFC 6902 operation.
///
/// [`diff`](crate::diff) produces `Add`, `Remove` and `Replace`, and `Move`
/// when arrays are matched by key; [`patch`](crate::patch) applies all six.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    /// Insert a value, or set an object member.
    Add,
    /// Delete a value.
    Remove,
    /// Overwrite an existing value.
    Replace,
    /// Move a value from `from_path` to `path`.
    Move,
    /// Copy a value from `from_path` to `path`.
    Copy,
    /// Fail the patch unless the value at `path` equals `value`.
    Test,
}

impl Operation {
    /// The operation called `name` in the RFC 6902 JSON form, if there is one.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "add" => Some(Self::Add),
            "remove" => Some(Self::Remove),
            "replace" => Some(Self::Replace),
            "move" => Some(Self::Move),
            "copy" => Some(Self::Copy),
            "test" => Some(Self::Test),
            _ => None,
        }
    }

    /// The name used in the RFC 6902 JSON form: `"add"`, `"remove"` and so on.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Remove => "remove",
            Self::Replace => "replace",
            Self::Move => "move",
            Self::Copy => "copy",
            Self::Test => "test",
        }
    }
}

/// One RFC 6902 operation: what to do, where, and with which value.
///
/// With serde it is the standard JSON form, so a patch is just `Vec<Delta>`:
///
/// ```
/// use drift::{Delta, Operation};
///
/// let patch: Vec<Delta> = serde_json::from_str(
///     r#"[{"op": "replace", "path": "/name", "value": "Alex"}]"#,
/// )?;
/// assert_eq!(patch[0].op, Operation::Replace);
/// assert_eq!(
///     serde_json::to_string(&patch)?,
///     r#"[{"op":"replace","path":"/name","value":"Alex"}]"#,
/// );
/// # Ok::<(), serde_json::Error>(())
/// ```
///
/// Reading is strict about what RFC 6902 requires: `op` and `path` always,
/// `value` for `add`, `replace` and `test`, and `from` for `move` and `copy`.
/// Other members are ignored. `"value": null` is a value, not a missing one.
#[derive(Debug, Clone, PartialEq)]
pub struct Delta {
    /// What to do.
    pub op: Operation,
    /// JSON Pointer to the target, `""` for the whole document.
    pub path: String,
    /// The value for `Add`, `Replace` and `Test`; `None` otherwise.
    pub value: Option<Value>,
    /// The source pointer for `Move` and `Copy`; `None` otherwise.
    pub from_path: Option<String>,
}

impl Delta {
    /// An operation without a value, such as `Remove`.
    pub fn new(op: Operation, path: impl Into<String>) -> Self {
        Self { op, path: path.into(), value: None, from_path: None }
    }

    /// An operation that carries a value, such as `Add` or `Replace`.
    pub fn with_value(op: Operation, path: impl Into<String>, value: Value) -> Self {
        Self { op, path: path.into(), value: Some(value), from_path: None }
    }
}

impl Serialize for Operation {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl Serialize for Delta {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let carries_value =
            matches!(self.op, Operation::Add | Operation::Replace | Operation::Test);
        let carries_from = matches!(self.op, Operation::Move | Operation::Copy);
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("op", &self.op)?;
        if let Some(from) = self.from_path.as_ref().filter(|_| carries_from) {
            map.serialize_entry("from", from)?;
        }
        map.serialize_entry("path", &self.path)?;
        if let Some(value) = self.value.as_ref().filter(|_| carries_value) {
            map.serialize_entry("value", value)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for Delta {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let Value::Object(mut members) = Value::deserialize(deserializer)? else {
            return Err(D::Error::custom("an operation must be a JSON object"));
        };
        let mut string = |name: &str| match members.remove(name) {
            None => Err(D::Error::custom(format!("missing member `{name}`"))),
            Some(Value::String(text)) => Ok(text),
            Some(_) => Err(D::Error::custom(format!("`{name}` must be a string"))),
        };
        let op_name = string("op")?;
        let op = Operation::from_name(&op_name)
            .ok_or_else(|| D::Error::custom(format!("unknown operation `{op_name}`")))?;
        let path = string("path")?;
        let from_path = match op {
            Operation::Move | Operation::Copy => Some(string("from")?),
            _ => None,
        };
        let value = match op {
            Operation::Add | Operation::Replace | Operation::Test => {
                Some(members.remove("value").ok_or_else(|| {
                    D::Error::custom(format!("operation `{op_name}` needs a `value`"))
                })?)
            }
            _ => None,
        };
        Ok(Delta { op, path, value, from_path })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(text: &str) -> Result<Vec<Delta>, serde_json::Error> {
        serde_json::from_str(text)
    }

    #[test]
    fn serialises_to_the_rfc_6902_shape() {
        let ops = vec![
            Delta::with_value(Operation::Add, "/a", json!(1)),
            Delta::new(Operation::Remove, "/b"),
            Delta::with_value(Operation::Replace, "/c", json!({"x": null})),
            Delta {
                op: Operation::Move,
                path: "/d".into(),
                value: None,
                from_path: Some("/e".into()),
            },
            Delta {
                op: Operation::Copy,
                path: "/f".into(),
                value: None,
                from_path: Some("/g".into()),
            },
            Delta::with_value(Operation::Test, "/h", json!("v")),
        ];
        assert_eq!(
            serde_json::to_value(&ops).unwrap(),
            json!([
                {"op": "add", "path": "/a", "value": 1},
                {"op": "remove", "path": "/b"},
                {"op": "replace", "path": "/c", "value": {"x": null}},
                {"op": "move", "from": "/e", "path": "/d"},
                {"op": "copy", "from": "/g", "path": "/f"},
                {"op": "test", "path": "/h", "value": "v"},
            ])
        );
    }

    #[test]
    fn round_trips_every_operation() {
        let ops = vec![
            Delta::with_value(Operation::Add, "", json!([1, 2])),
            Delta::new(Operation::Remove, "/a/0"),
            Delta::with_value(Operation::Replace, "/a~1b", json!(false)),
            Delta {
                op: Operation::Move,
                path: "/x".into(),
                value: None,
                from_path: Some("/y".into()),
            },
            Delta {
                op: Operation::Copy,
                path: "/x".into(),
                value: None,
                from_path: Some("/y".into()),
            },
            Delta::with_value(Operation::Test, "/t", json!(1.5)),
        ];
        let text = serde_json::to_string(&ops).unwrap();
        assert_eq!(parse(&text).unwrap(), ops);
    }

    #[test]
    fn a_null_value_is_not_a_missing_value() {
        let ops = parse(r#"[{"op": "add", "path": "/a", "value": null}]"#).unwrap();
        assert_eq!(ops[0].value, Some(Value::Null));
        assert_eq!(
            serde_json::to_string(&ops).unwrap(),
            r#"[{"op":"add","path":"/a","value":null}]"#
        );
    }

    #[test]
    fn unknown_members_are_ignored() {
        let ops =
            parse(r#"[{"op": "remove", "path": "/a", "comment": "why", "value": 1}]"#).unwrap();
        assert_eq!(ops, vec![Delta::new(Operation::Remove, "/a")]);
    }

    #[test]
    fn rejects_what_rfc_6902_requires() {
        for (text, expect) in [
            (r#"[{"path": "/a"}]"#, "op"),
            (r#"[{"op": "frobnicate", "path": "/a"}]"#, "frobnicate"),
            (r#"[{"op": "remove"}]"#, "path"),
            (r#"[{"op": "add", "path": "/a"}]"#, "value"),
            (r#"[{"op": "replace", "path": "/a"}]"#, "value"),
            (r#"[{"op": "test", "path": "/a"}]"#, "value"),
            (r#"[{"op": "move", "path": "/a"}]"#, "from"),
            (r#"[{"op": "copy", "path": "/a"}]"#, "from"),
            (r#"[{"op": "remove", "path": 5}]"#, "path"),
            (r#"["remove"]"#, "object"),
        ] {
            let error = parse(text).unwrap_err().to_string();
            assert!(error.contains(expect), "{text}: {error}");
        }
    }
}

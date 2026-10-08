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

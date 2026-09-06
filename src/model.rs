use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    Add,
    Remove,
    Replace,
    Move,
    Copy,
    Test,
}

impl Operation {
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

#[derive(Debug, Clone, PartialEq)]
pub struct Delta {
    pub op: Operation,
    pub path: String,
    pub value: Option<Value>,
    pub from_path: Option<String>,
}

impl Delta {
    pub fn new(op: Operation, path: impl Into<String>) -> Self {
        Self { op, path: path.into(), value: None, from_path: None }
    }

    pub fn with_value(op: Operation, path: impl Into<String>, value: Value) -> Self {
        Self { op, path: path.into(), value: Some(value), from_path: None }
    }
}

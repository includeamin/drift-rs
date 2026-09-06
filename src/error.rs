use thiserror::Error;

#[derive(Debug, Error)]
pub enum DriftError {
    #[error("invalid JSON Pointer: {0}")]
    Pointer(String),
    #[error("path not found: {0}")]
    Missing(String),
    #[error("invalid array index: {0}")]
    Index(String),
    #[error("cannot {operation} at {path}: {reason}")]
    Operation { operation: String, path: String, reason: String },
    #[error("JSON Patch test failed at {0:?}")]
    Test(String),
    #[error("invalid regular expression: {0}")]
    Regex(String),
}

use thiserror::Error;

/// What can go wrong reading, diffing or patching a document.
#[derive(Debug, Error)]
pub enum DriftError {
    /// A string is not a valid JSON Pointer (RFC 6901), for example a bad `~` escape.
    #[error("invalid JSON Pointer: {0}")]
    Pointer(String),
    /// A path or key a patch refers to does not exist in the document.
    #[error("path not found: {0}")]
    Missing(String),
    /// An array index is malformed (leading zero, not a number) or out of range.
    #[error("invalid array index: {0}")]
    Index(String),
    /// An operation cannot be applied where it points, for example adding to a scalar.
    #[error("cannot {operation} at {path}: {reason}")]
    Operation {
        /// The operation that failed, such as `add` or `remove`.
        operation: String,
        /// The path it was applied to.
        path: String,
        /// Why it could not be applied.
        reason: String,
    },
    /// A `test` operation found a different value than it expected, at this path.
    #[error("JSON Patch test failed at {0:?}")]
    Test(String),
    /// A filter pattern is not a valid regular expression.
    #[error("invalid regular expression: {0}")]
    Regex(String),
    /// Reading a file failed.
    #[error("i/o error: {0}")]
    Io(String),
    /// A document could not be parsed or written in its format.
    #[error("parse error: {0}")]
    Parse(String),
}

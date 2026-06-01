//! Typed client-side A2A errors.
//!
//! Mirrors the Python/TypeScript client exception hierarchy: every JSON-RPC
//! error code maps to a specific variant so callers never need to inspect raw
//! error codes.

use thiserror::Error;

/// JSON-RPC error codes the client maps to typed errors.
pub const CODE_TASK_NOT_FOUND: i32 = -32001;
pub const CODE_TASK_NOT_CANCELABLE: i32 = -32002;
pub const CODE_INTERNAL_ERROR: i32 = -32603;

/// Base error type for all client-side A2A failures.
#[derive(Debug, Error)]
pub enum A2AClientError {
    /// Network-level failure: connection refused, timeout, DNS error, or a
    /// non-2xx HTTP status.
    #[error("A2A connection error: {0}")]
    Connection(String),

    /// Agent Card fetch failed: HTTP error or invalid JSON in the response.
    #[error("A2A discovery error: {0}")]
    Discovery(String),

    /// JSON-RPC -32001: the requested task does not exist.
    #[error("Task not found{}", task_id.as_ref().map(|t| format!(": {t}")).unwrap_or_default())]
    TaskNotFound { task_id: Option<String> },

    /// JSON-RPC -32002: the task is in a terminal (non-cancelable) state.
    #[error("Task not cancelable{}", state.as_ref().map(|s| format!(": {s}")).unwrap_or_default())]
    TaskNotCancelable { state: Option<String> },

    /// JSON-RPC -32603 (and any other code): internal server error.
    #[error("A2A server error ({code}): {message}")]
    Server { code: i32, message: String },

    /// Invalid agent URL supplied at construction time.
    #[error("Invalid A2A agent URL: {0} (must be http:// or https://)")]
    InvalidUrl(String),
}

impl A2AClientError {
    /// Build a typed error from a JSON-RPC `error` object (`{code, message}`).
    pub fn from_jsonrpc(code: i32, message: impl Into<String>) -> Self {
        let message = message.into();
        match code {
            CODE_TASK_NOT_FOUND => A2AClientError::TaskNotFound { task_id: None },
            CODE_TASK_NOT_CANCELABLE => A2AClientError::TaskNotCancelable { state: None },
            other => A2AClientError::Server {
                code: other,
                message,
            },
        }
    }
}

/// Convenience result alias for client operations.
pub type ClientResult<T> = Result<T, A2AClientError>;

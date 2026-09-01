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
/// Governance refusal codes (srs FR-ERR-003 / FR-ERR-009 / FR-ERR-010). Without
/// typed variants for these a refusal would surface as an untyped
/// [`A2AClientError::Server`], which is exactly the "some server-side problem,
/// retry it" reading the server side of this change exists to stop.
pub const CODE_ACCESS_DENIED: i32 = -32040;
pub const CODE_APPROVAL_DENIED: i32 = -32041;
pub const CODE_APPROVAL_TIMEOUT: i32 = -32042;

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

    /// JSON-RPC -32040: the ACL refused this caller. Terminal — retrying the
    /// same call with the same identity cannot succeed.
    #[error("Access denied: {message}")]
    AccessDenied { message: String },

    /// JSON-RPC -32041: a human explicitly refused this call. Terminal.
    #[error("Approval denied: {message}")]
    ApprovalDenied { message: String },

    /// JSON-RPC -32042: the approval expired unanswered. Unlike the other two
    /// refusals, a fresh submission may legitimately be approved.
    #[error("Approval timed out: {message}")]
    ApprovalTimeout { message: String },

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
            CODE_ACCESS_DENIED => A2AClientError::AccessDenied { message },
            CODE_APPROVAL_DENIED => A2AClientError::ApprovalDenied { message },
            CODE_APPROVAL_TIMEOUT => A2AClientError::ApprovalTimeout { message },
            other => A2AClientError::Server {
                code: other,
                message,
            },
        }
    }
}

impl A2AClientError {
    /// Whether this is a governance refusal — the ACL, or a human, said no.
    ///
    /// Distinguishing these from [`A2AClientError::Server`] is the point of the
    /// typed variants: a refusal is not a transient failure, and an agent that
    /// backs off and retries one will be refused identically for as long as it
    /// keeps trying. [`A2AClientError::ApprovalTimeout`] is the one where a
    /// *fresh* submission may still succeed.
    pub fn is_governance_refusal(&self) -> bool {
        matches!(
            self,
            A2AClientError::AccessDenied { .. }
                | A2AClientError::ApprovalDenied { .. }
                | A2AClientError::ApprovalTimeout { .. }
        )
    }
}

/// Convenience result alias for client operations.
pub type ClientResult<T> = Result<T, A2AClientError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn governance_codes_map_to_typed_variants_not_the_server_catch_all() {
        for (code, expected_governance) in [
            (CODE_ACCESS_DENIED, true),
            (CODE_APPROVAL_DENIED, true),
            (CODE_APPROVAL_TIMEOUT, true),
            (CODE_INTERNAL_ERROR, false),
            (CODE_TASK_NOT_FOUND, false),
        ] {
            let err = A2AClientError::from_jsonrpc(code, "whatever the server said");
            assert_eq!(err.is_governance_refusal(), expected_governance, "{code}");
        }
    }

    #[test]
    fn an_access_denial_is_not_reported_as_task_not_found() {
        // The whole point of moving off -32001: these two must not collapse
        // into one client-side variant.
        assert!(matches!(
            A2AClientError::from_jsonrpc(CODE_ACCESS_DENIED, "Access denied"),
            A2AClientError::AccessDenied { .. }
        ));
        assert!(matches!(
            A2AClientError::from_jsonrpc(CODE_TASK_NOT_FOUND, "Task not found"),
            A2AClientError::TaskNotFound { .. }
        ));
    }
}

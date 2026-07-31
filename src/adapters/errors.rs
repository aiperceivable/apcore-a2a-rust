//! ErrorMapper — translates apcore errors to A2A JSON-RPC error responses.

use apcore::error_formatter::{ErrorFormatter as ApcoreErrorFormatter, ErrorFormatterRegistry};
use apcore::errors::{ErrorCode as ApcoreErrorCode, ModuleError};
use serde::{Deserialize, Serialize};
use serde_json::Value;

// JSON-RPC error codes per A2A specification
const CODE_METHOD_NOT_FOUND: i32 = -32601;
const CODE_INVALID_PARAMS: i32 = -32602;
const CODE_INTERNAL_ERROR: i32 = -32603;
const CODE_TASK_NOT_FOUND: i32 = -32001;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
}

pub struct ErrorMapper;

impl ErrorMapper {
    pub fn to_jsonrpc_error(error: &ModuleError) -> JsonRpcError {
        // Log the full, unsanitized error server-side for diagnostics before any
        // sanitization (matches Python's `logger.error(..., exc_info=True)`).
        tracing::error!(error = ?error, "apcore error mapped to A2A JSON-RPC error");

        let code = error.code;

        match code {
            ApcoreErrorCode::ModuleNotFound => JsonRpcError {
                code: CODE_METHOD_NOT_FOUND,
                message: sanitize_message(&error.message),
            },
            ApcoreErrorCode::SchemaValidationError => JsonRpcError {
                code: CODE_INVALID_PARAMS,
                message: sanitize_message(&error.message),
            },
            ApcoreErrorCode::ACLDenied => JsonRpcError {
                code: CODE_TASK_NOT_FOUND,
                message: "Task not found".to_string(),
            },
            ApcoreErrorCode::ModuleTimeout => JsonRpcError {
                code: CODE_INTERNAL_ERROR,
                message: "Execution timeout".to_string(),
            },
            ApcoreErrorCode::ExecutionCancelled => JsonRpcError {
                code: CODE_INTERNAL_ERROR,
                message: "Execution cancelled".to_string(),
            },
            ApcoreErrorCode::CircuitBreakerOpen | ApcoreErrorCode::TaskLimitExceeded => {
                JsonRpcError {
                    code: CODE_INTERNAL_ERROR,
                    message: "Service temporarily unavailable".to_string(),
                }
            }
            ApcoreErrorCode::CallDepthExceeded
            | ApcoreErrorCode::CircularCall
            | ApcoreErrorCode::CallFrequencyExceeded => JsonRpcError {
                code: CODE_INTERNAL_ERROR,
                message: "Safety limit exceeded".to_string(),
            },
            ApcoreErrorCode::ModuleDisabled => JsonRpcError {
                code: CODE_INTERNAL_ERROR,
                message: "Module is currently disabled".to_string(),
            },
            ApcoreErrorCode::ConfigNamespaceDuplicate
            | ApcoreErrorCode::ConfigMountError
            | ApcoreErrorCode::ConfigBindError => JsonRpcError {
                code: CODE_INTERNAL_ERROR,
                message: "Configuration error".to_string(),
            },
            ApcoreErrorCode::GeneralInvalidInput => JsonRpcError {
                code: CODE_INVALID_PARAMS,
                message: format!("Invalid input: {}", sanitize_message(&error.message)),
            },
            _ => JsonRpcError {
                code: CODE_INTERNAL_ERROR,
                message: "Internal server error".to_string(),
            },
        }
    }
}

/// Whether [`ErrorMapper::to_jsonrpc_error`] lets this code's own message reach
/// the caller (sanitized), or replaces it with a fixed per-class string.
///
/// This is the partition that decides whether a message may be *widened* — with
/// `ai_guidance`, or anything else. It is deliberately not `user_fixable`, which
/// is a different partition: six codes carry `user_fixable = Some(true)` while
/// falling into `to_jsonrpc_error`'s catch-all (`VERSION_CONSTRAINT_INVALID`,
/// the three `BINDING_*` codes, `DEPENDENCY_NOT_FOUND`,
/// `DEPENDENCY_VERSION_MISMATCH`), and appending guidance to those extended the
/// fixed "Internal server error" string with internal detail. `user_fixable` is
/// also settable per-error by the module author, which would have let any module
/// widen the fixed string at will.
///
/// `error_mapper_message_policy_matches_to_jsonrpc_error` locks this to the
/// match in [`ErrorMapper::to_jsonrpc_error`] across every apcore error code, so
/// the two cannot drift.
pub(crate) fn carries_caller_detail(code: ApcoreErrorCode) -> bool {
    matches!(
        code,
        ApcoreErrorCode::ModuleNotFound
            | ApcoreErrorCode::SchemaValidationError
            | ApcoreErrorCode::GeneralInvalidInput
    )
}

/// Strip paths, tracebacks and excess whitespace from text bound for a caller.
/// Crate-visible so the task-status surface (`server::handlers`) applies exactly
/// the same redaction as the JSON-RPC surface.
pub(crate) fn sanitize_message(message: &str) -> String {
    // Match Unix absolute paths (single or multi-component) and ~ paths.
    let path_re = regex::Regex::new(r"~?/\S*").unwrap();
    let cleaned = path_re.replace_all(message, "");
    // Strip traceback lines (kept in sync with Python/TypeScript bindings).
    let tb_re = regex::Regex::new(r#"(?m)^.*(?:Traceback|File "|line \d+).*$"#).unwrap();
    let cleaned = tb_re.replace_all(&cleaned, "");
    // Collapse internal whitespace.
    let ws_re = regex::Regex::new(r"\s+").unwrap();
    let cleaned = ws_re.replace_all(&cleaned, " ");
    let cleaned = cleaned.trim();
    // Char-boundary-safe truncation: slicing by byte index (`cleaned[..500]`)
    // panics when byte 500 lands inside a multibyte UTF-8 char.
    cleaned.chars().take(500).collect::<String>()
}

// A2A Error Formatter for apcore ErrorFormatterRegistry

pub struct A2aErrorFormatter;

impl ApcoreErrorFormatter for A2aErrorFormatter {
    fn format(&self, error: &ModuleError, _context: Option<&dyn std::any::Any>) -> Value {
        serde_json::to_value(ErrorMapper::to_jsonrpc_error(error)).unwrap_or_else(|_| {
            // Never fall back to the raw apcore error (it may leak unsanitized
            // detail); return a hardcoded sanitized generic error instead.
            serde_json::json!({ "code": -32603, "message": "Internal server error" })
        })
    }
}

/// Register the A2A error formatter with apcore's ErrorFormatterRegistry.
pub fn register_a2a_error_formatter() {
    if !ErrorFormatterRegistry::is_registered("a2a") {
        let _ = ErrorFormatterRegistry::register("a2a", Box::new(A2aErrorFormatter));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_error(code: ApcoreErrorCode, message: &str) -> ModuleError {
        ModuleError::new(code, message)
    }

    #[test]
    fn test_module_not_found() {
        let err = make_error(ApcoreErrorCode::ModuleNotFound, "module foo not found");
        let resp = ErrorMapper::to_jsonrpc_error(&err);
        assert_eq!(resp.code, CODE_METHOD_NOT_FOUND);
        assert!(resp.message.contains("module foo not found"));
    }

    #[test]
    fn test_acl_denied_masked() {
        let err = make_error(ApcoreErrorCode::ACLDenied, "secret info");
        let resp = ErrorMapper::to_jsonrpc_error(&err);
        assert_eq!(resp.code, CODE_TASK_NOT_FOUND);
        assert_eq!(resp.message, "Task not found");
    }

    #[test]
    fn test_safety_limit() {
        let err = make_error(ApcoreErrorCode::CallDepthExceeded, "depth exceeded");
        let resp = ErrorMapper::to_jsonrpc_error(&err);
        assert_eq!(resp.code, CODE_INTERNAL_ERROR);
        assert_eq!(resp.message, "Safety limit exceeded");
    }

    #[test]
    fn test_module_disabled() {
        let err = make_error(ApcoreErrorCode::ModuleDisabled, "disabled");
        let resp = ErrorMapper::to_jsonrpc_error(&err);
        assert_eq!(resp.code, CODE_INTERNAL_ERROR);
        assert_eq!(resp.message, "Module is currently disabled");
    }

    #[test]
    fn test_config_error() {
        let err = make_error(ApcoreErrorCode::ConfigNamespaceDuplicate, "dup");
        let resp = ErrorMapper::to_jsonrpc_error(&err);
        assert_eq!(resp.code, CODE_INTERNAL_ERROR);
        assert_eq!(resp.message, "Configuration error");
    }

    #[test]
    fn test_unknown_error_internal() {
        let err = make_error(ApcoreErrorCode::GeneralInternalError, "boom");
        let resp = ErrorMapper::to_jsonrpc_error(&err);
        assert_eq!(resp.code, CODE_INTERNAL_ERROR);
        assert_eq!(resp.message, "Internal server error");
    }

    #[test]
    fn test_sanitize_strips_paths() {
        let msg = sanitize_message("error at /home/user/secret/file.py line 42");
        assert!(!msg.contains("/home"));
    }

    #[test]
    fn test_sanitize_strips_tracebacks() {
        let msg = sanitize_message(
            "boom\nTraceback (most recent call last):\nFile \"x.rs\", line 42, in foo\nactual error",
        );
        assert!(!msg.contains("Traceback"));
        assert!(!msg.contains("File \""));
        assert!(!msg.contains("line 42"));
        assert!(msg.contains("boom"));
        assert!(msg.contains("actual error"));
    }

    #[test]
    fn test_sanitize_collapses_whitespace() {
        let msg = sanitize_message("a\n\n   b\t\tc");
        assert_eq!(msg, "a b c");
    }

    #[test]
    fn test_execution_cancelled() {
        let err = make_error(ApcoreErrorCode::ExecutionCancelled, "cancelled");
        let resp = ErrorMapper::to_jsonrpc_error(&err);
        assert_eq!(resp.code, CODE_INTERNAL_ERROR);
        assert_eq!(resp.message, "Execution cancelled");
    }

    #[test]
    fn test_circuit_breaker_open() {
        let err = make_error(ApcoreErrorCode::CircuitBreakerOpen, "open");
        let resp = ErrorMapper::to_jsonrpc_error(&err);
        assert_eq!(resp.code, CODE_INTERNAL_ERROR);
        assert_eq!(resp.message, "Service temporarily unavailable");
    }

    #[test]
    fn test_task_limit_exceeded() {
        let err = make_error(ApcoreErrorCode::TaskLimitExceeded, "too many");
        let resp = ErrorMapper::to_jsonrpc_error(&err);
        assert_eq!(resp.code, CODE_INTERNAL_ERROR);
        assert_eq!(resp.message, "Service temporarily unavailable");
    }

    #[test]
    fn test_sanitize_truncation_multibyte_no_panic() {
        // 600 multibyte chars (3 bytes each in UTF-8) => 1800 bytes, with a
        // char straddling byte boundary 500. A naive `cleaned[..500]` would panic.
        let input: String = "\u{3042}".repeat(600);
        let out = sanitize_message(&input);
        assert!(out.chars().count() <= 500);
        assert_eq!(out.chars().count(), 500);
    }

    #[test]
    fn test_register_a2a_error_formatter_idempotent() {
        register_a2a_error_formatter();
        register_a2a_error_formatter();
    }

    #[test]
    fn error_mapper_message_policy_matches_to_jsonrpc_error() {
        // `carries_caller_detail` is what gates message widening (see
        // `server::handlers::failure_text`), so it must name exactly the codes
        // whose own message `to_jsonrpc_error` actually forwards. Asserted over
        // every apcore error code with a sentinel that survives sanitization,
        // so adding a code or an arm cannot silently desync the two.
        const SENTINEL: &str = "canary-2f8a";
        for &code in ApcoreErrorCode::ALL {
            let mapped = ErrorMapper::to_jsonrpc_error(&make_error(code, SENTINEL));
            assert_eq!(
                mapped.message.contains(SENTINEL),
                carries_caller_detail(code),
                "{code:?}: to_jsonrpc_error forwards the message = {}, \
                 carries_caller_detail = {}",
                mapped.message.contains(SENTINEL),
                carries_caller_detail(code),
            );
        }
    }
}

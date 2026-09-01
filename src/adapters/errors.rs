//! ErrorMapper — translates apcore errors to A2A JSON-RPC error responses.

use apcore::error_formatter::{ErrorFormatter as ApcoreErrorFormatter, ErrorFormatterRegistry};
use apcore::errors::{ErrorCode as ApcoreErrorCode, ModuleError};
use serde::{Deserialize, Serialize};
use serde_json::Value;

// JSON-RPC error codes per A2A specification
const CODE_METHOD_NOT_FOUND: i32 = -32601;
const CODE_INVALID_PARAMS: i32 = -32602;
const CODE_INTERNAL_ERROR: i32 = -32603;
/// A2A 1.0 `TaskNotFoundError`. Reserved for an unknown task id or one owned by
/// another principal — deliberately indistinguishable from each other, and no
/// longer produced for an authorization refusal (see `CODE_ACCESS_DENIED`).
/// Kept here so `task_not_found_code_is_no_longer_spent_on_authorization` can
/// assert no apcore error code reaches it.
#[cfg(test)]
const CODE_TASK_NOT_FOUND: i32 = -32001;

// Governance refusal codes (srs FR-ERR-003 / FR-ERR-009 / FR-ERR-010).
//
// A2A 1.0 reserves -32001..-32009; JSON-RPC 2.0 leaves -32000..-32099 to the
// implementation. These three sit above A2A's reserved block, with room for it
// to grow, and are the "JSON-RPC custom error" the A2A spec §13.2 names as the
// example for this binding.
//
// apcore distinguishes these three refusals from each other and from every
// other failure. Collapsing them onto -32001 (which means "unknown or non-owned
// task id") or -32603 (which every agent reads as "retry me") tells the caller
// a *different* failure happened, one whose correct response is the opposite of
// the real one.
const CODE_ACCESS_DENIED: i32 = -32040;
const CODE_APPROVAL_DENIED: i32 = -32041;
const CODE_APPROVAL_TIMEOUT: i32 = -32042;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
}

pub struct ErrorMapper;

impl ErrorMapper {
    /// Map an apcore error with the default refusal policy (fixed per-class
    /// strings for the three governance codes; srs FR-ERR-011 default).
    pub fn to_jsonrpc_error(error: &ModuleError) -> JsonRpcError {
        Self::to_jsonrpc_error_with(error, false)
    }

    /// Map an apcore error, optionally forwarding apcore's own reason for a
    /// governance refusal (srs FR-ERR-011, `disclose_refusal_reason`).
    ///
    /// The flag only ever widens the *message*. The code is the same either
    /// way: what a refusal *is* does not depend on how much a deployment
    /// chooses to say about it.
    pub fn to_jsonrpc_error_with(
        error: &ModuleError,
        disclose_refusal_reason: bool,
    ) -> JsonRpcError {
        // Log the full, unsanitized error server-side for diagnostics before any
        // sanitization (matches Python's `logger.error(..., exc_info=True)`).
        tracing::error!(error = ?error, "apcore error mapped to A2A JSON-RPC error");

        let code = error.code;

        match code {
            ApcoreErrorCode::ModuleNotFound => JsonRpcError {
                code: CODE_METHOD_NOT_FOUND,
                message: sanitize_message(&error.message),
            },
            // apcore raises SCHEMA_VALIDATION_ERROR for output and config
            // validation too, neither of which the caller can do anything
            // about — see `is_server_side_schema_error`.
            ApcoreErrorCode::SchemaValidationError
                if is_server_side_schema_error(&error.message) =>
            {
                JsonRpcError {
                    code: CODE_INTERNAL_ERROR,
                    message: "Internal server error".to_string(),
                }
            }
            ApcoreErrorCode::SchemaValidationError => JsonRpcError {
                code: CODE_INVALID_PARAMS,
                message: sanitize_message(&error.message),
            },
            // The A2A spec §13.2 MUST NOT forbids revealing *the existence of
            // a resource*, not the *class* of failure. A fixed "Access denied"
            // naming no caller, target or rule discloses nothing — a caller
            // that named a skill already held that id — while still telling an
            // agent to stop rather than retry.
            ApcoreErrorCode::ACLDenied => JsonRpcError {
                code: CODE_ACCESS_DENIED,
                message: refusal_message("Access denied", error, disclose_refusal_reason),
            },
            // A human explicitly refused. On the -32603 catch-all this read as
            // the canonical *retryable* failure, which does not merely permit a
            // retry loop but invites one.
            ApcoreErrorCode::ApprovalDenied => JsonRpcError {
                code: CODE_APPROVAL_DENIED,
                message: refusal_message("Approval denied", error, disclose_refusal_reason),
            },
            // Distinct from ApprovalDenied (nobody refused, nobody answered) and
            // from ModuleTimeout (an execution deadline, not a governance
            // outcome): a fresh submission may legitimately be approved.
            ApcoreErrorCode::ApprovalTimeout => JsonRpcError {
                code: CODE_APPROVAL_TIMEOUT,
                message: refusal_message("Approval timed out", error, disclose_refusal_reason),
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
/// The three governance codes (`ACLDenied`, `ApprovalDenied`, `ApprovalTimeout`)
/// are in this partition only when `disclose_refusal_reason` is set — the same
/// flag `to_jsonrpc_error_with` branches on, so the two surfaces agree under
/// either setting.
///
/// `error_mapper_message_policy_matches_to_jsonrpc_error` locks this to the
/// match in [`ErrorMapper::to_jsonrpc_error_with`] across every apcore error
/// code and both flag values, so the two cannot drift.
pub(crate) fn carries_caller_detail(error: &ModuleError, disclose_refusal_reason: bool) -> bool {
    match error.code {
        ApcoreErrorCode::ModuleNotFound | ApcoreErrorCode::GeneralInvalidInput => true,
        ApcoreErrorCode::SchemaValidationError => !is_server_side_schema_error(&error.message),
        // The three governance codes move into and out of this partition with
        // the flag, so the task-status surface forwards exactly what the
        // JSON-RPC surface does under either setting (srs FR-ERR-011 criterion 4).
        ApcoreErrorCode::ACLDenied
        | ApcoreErrorCode::ApprovalDenied
        | ApcoreErrorCode::ApprovalTimeout => disclose_refusal_reason,
        _ => false,
    }
}

/// The caller-facing message for a governance refusal.
///
/// Default: the fixed per-class string, which names no caller, target, approver
/// or rule. With `disclose_refusal_reason` (srs FR-ERR-011): apcore's own
/// message, through the same sanitizer every other forwarded message goes
/// through. An empty or whitespace-only apcore message falls back to the fixed
/// string rather than sending the caller nothing.
fn refusal_message(fixed: &str, error: &ModuleError, disclose_refusal_reason: bool) -> String {
    if !disclose_refusal_reason {
        return fixed.to_string();
    }
    let disclosed = sanitize_message(&error.message);
    if disclosed.trim().is_empty() {
        fixed.to_string()
    } else {
        disclosed
    }
}

/// Whether a `SCHEMA_VALIDATION_ERROR` is about something the *server* produced
/// rather than something the caller sent.
///
/// apcore raises the one code for all three directions —
/// `executor::validate_against_schema(value, schema, direction)` is called with
/// `"Input"`, `"Output"` (`executor.rs`, on the module's own result) and
/// `"Config"` (`config.rs`). Reporting an output- or config-validation failure
/// as `-32602 Invalid params` tells the caller to fix a request that was
/// correct, and its `ai_guidance` points at a `details.errors` field an A2A
/// caller never receives. Those are server-side defects and belong behind the
/// fixed internal string.
///
/// The direction label apcore puts at the front of the message is the only
/// signal that exists, so this matches apcore's two exact wordings rather than
/// a loose prefix. Anything unrecognized keeps the caller-facing detail —
/// including a module that raises the code itself with its own wording, whose
/// message srs FR-ERR-002 requires the caller to see. Failing to recognize a
/// server-side error therefore preserves today's behaviour; it never masks a
/// caller-fixable one by mistake.
fn is_server_side_schema_error(message: &str) -> bool {
    ["Output", "Config"].iter().any(|direction| {
        message == format!("{direction} validation failed")
            || message.starts_with(&format!("{direction} schema is invalid: "))
    })
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
    fn acl_denial_reports_access_denied_not_task_not_found() {
        let err = make_error(
            ApcoreErrorCode::ACLDenied,
            "Access denied: caller 'svc-db-writer' cannot access module 'admin.users.delete'",
        );
        let resp = ErrorMapper::to_jsonrpc_error(&err);
        assert_eq!(resp.code, CODE_ACCESS_DENIED);
        assert_eq!(resp.message, "Access denied");
        // The class is conveyed; the detail is not.
        assert!(!resp.message.contains("svc-db-writer"));
        assert!(!resp.message.contains("admin.users.delete"));
    }

    #[test]
    fn approval_denial_leaves_the_retryable_catch_all() {
        let err = make_error(
            ApcoreErrorCode::ApprovalDenied,
            "Approval denied by alice@example.com for approval 7f3c1e",
        );
        let resp = ErrorMapper::to_jsonrpc_error(&err);
        assert_eq!(resp.code, CODE_APPROVAL_DENIED);
        assert_eq!(resp.message, "Approval denied");
        assert!(!resp.message.contains("alice@example.com"));
        assert!(!resp.message.contains("7f3c1e"));
    }

    #[test]
    fn approval_timeout_is_distinct_from_execution_timeout() {
        let err = make_error(
            ApcoreErrorCode::ApprovalTimeout,
            "Approval 7f3c1e timed out after 300s waiting on alice@example.com",
        );
        let resp = ErrorMapper::to_jsonrpc_error(&err);
        assert_eq!(resp.code, CODE_APPROVAL_TIMEOUT);
        assert_eq!(resp.message, "Approval timed out");
        // Not ModuleTimeout's string: an execution deadline and an unanswered
        // approval call for different next moves.
        assert_ne!(resp.message, "Execution timeout");
        assert!(!resp.message.contains("alice@example.com"));
    }

    #[test]
    fn approval_pending_is_not_swept_into_the_governance_block() {
        // ApprovalPending is a resumable pause, not a refusal. `error_to_status`
        // intercepts it before the mapper's message is ever used; re-coding it
        // here would turn that pause into a terminal failure.
        let err = make_error(
            ApcoreErrorCode::ApprovalPending,
            "Approval required: id=7f3c1e",
        );
        let resp = ErrorMapper::to_jsonrpc_error(&err);
        assert_eq!(resp.code, CODE_INTERNAL_ERROR);
        for governance in [
            CODE_ACCESS_DENIED,
            CODE_APPROVAL_DENIED,
            CODE_APPROVAL_TIMEOUT,
        ] {
            assert_ne!(resp.code, governance);
        }
    }

    #[test]
    fn task_not_found_code_is_no_longer_spent_on_authorization() {
        // -32001 must mean only "unknown or non-owned task id" now. No apcore
        // error code may produce it, or the separation is not real.
        for &code in ApcoreErrorCode::ALL {
            let err = make_error(code, "whatever");
            assert_ne!(
                ErrorMapper::to_jsonrpc_error(&err).code,
                CODE_TASK_NOT_FOUND,
                "{code:?} still maps to -32001"
            );
        }
    }

    #[test]
    fn disclose_refusal_reason_widens_the_message_but_never_the_code() {
        for (code, fixed) in [
            (ApcoreErrorCode::ACLDenied, "Access denied"),
            (ApcoreErrorCode::ApprovalDenied, "Approval denied"),
            (ApcoreErrorCode::ApprovalTimeout, "Approval timed out"),
        ] {
            let err = make_error(
                code,
                "caller 'svc-db-writer' cannot access 'admin.users.delete'",
            );
            let masked = ErrorMapper::to_jsonrpc_error_with(&err, false);
            let disclosed = ErrorMapper::to_jsonrpc_error_with(&err, true);
            assert_eq!(masked.message, fixed);
            assert_eq!(
                masked.code, disclosed.code,
                "the flag must not move the code"
            );
            assert!(disclosed.message.contains("svc-db-writer"));
        }
    }

    #[test]
    fn disclose_refusal_reason_falls_back_when_apcore_says_nothing() {
        let err = make_error(ApcoreErrorCode::ACLDenied, "   ");
        assert_eq!(
            ErrorMapper::to_jsonrpc_error_with(&err, true).message,
            "Access denied",
            "an empty apcore message must not send the caller an empty string"
        );
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
        // Both flag values: `disclose_refusal_reason` moves the three governance
        // codes across the partition, and the two must move together.
        for disclose in [false, true] {
            for &code in ApcoreErrorCode::ALL {
                let err = make_error(code, SENTINEL);
                let mapped = ErrorMapper::to_jsonrpc_error_with(&err, disclose);
                assert_eq!(
                    mapped.message.contains(SENTINEL),
                    carries_caller_detail(&err, disclose),
                    "{code:?} (disclose={disclose}): to_jsonrpc_error_with forwards \
                     the message = {}, carries_caller_detail = {}",
                    mapped.message.contains(SENTINEL),
                    carries_caller_detail(&err, disclose),
                );
            }
        }
        // The one code whose policy is not decided by the code alone.
        for message in [
            "Output validation failed",
            "Config validation failed",
            "Output schema is invalid: bad $ref",
        ] {
            let err = make_error(ApcoreErrorCode::SchemaValidationError, message);
            assert!(!carries_caller_detail(&err, false), "{message}");
            assert_eq!(
                ErrorMapper::to_jsonrpc_error(&err).message,
                "Internal server error",
                "{message}"
            );
        }
    }

    #[test]
    fn output_validation_failure_is_not_reported_as_caller_fixable() {
        // apcore raises SCHEMA_VALIDATION_ERROR for output validation too
        // (`validate_against_schema(&merged, &setup.output_schema, "Output")`),
        // so a module returning the wrong shape reached the caller as
        // `-32602 Invalid params` — telling them to fix a correct request, and
        // pointing at a `details` field an A2A caller never receives.
        let err = make_error(
            ApcoreErrorCode::SchemaValidationError,
            "Output validation failed",
        )
        .with_ai_guidance("Output failed schema validation. Check the 'errors' field in details.");
        let resp = ErrorMapper::to_jsonrpc_error(&err);
        assert_eq!(resp.code, CODE_INTERNAL_ERROR);
        assert_eq!(resp.message, "Internal server error");

        // Input validation — the caller-fixable direction — is untouched, and
        // so is a module raising the code with its own wording.
        for message in ["Input validation failed", "width: must be integer"] {
            let resp = ErrorMapper::to_jsonrpc_error(&make_error(
                ApcoreErrorCode::SchemaValidationError,
                message,
            ));
            assert_eq!(resp.code, CODE_INVALID_PARAMS, "{message}");
            assert_eq!(resp.message, message, "{message}");
        }
    }
}

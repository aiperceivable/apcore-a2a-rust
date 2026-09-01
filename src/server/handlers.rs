//! A2A 1.0 JSON-RPC + SSE request handlers.
//!
//! A single `POST /` endpoint dispatches the A2A JSON-RPC methods. `message/send`
//! runs a task to completion and returns the final `Task`; `message/stream`
//! returns an SSE stream of A2A 1.0 events (`statusUpdate` / `artifactUpdate`);
//! `tasks/get` / `tasks/cancel` / `ListTasks` manage task state. Per-task
//! `CancelToken`s back `tasks/cancel` (cooperative apcore cancellation).

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use apcore::cancel::CancelToken;
use apcore::context::Identity;
use apcore::errors::{ErrorCode, ModuleError};
use axum::{
    body::Bytes,
    extract::{FromRequestParts, State},
    http::request::Parts,
    http::{HeaderMap, StatusCode},
    response::sse::{Event, KeepAlive, Sse},
    response::{Html, IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;
use uuid::Uuid;

use crate::adapters::errors::{carries_caller_detail, sanitize_message, ErrorMapper};
use crate::server::executor::ApCoreAgentExecutor;
use crate::storage::{CallContext, ListParams, OwnerId, PushConfigStore, StoreError, TaskStore};
use crate::types::{
    Artifact, Message, StreamEvent, Task, TaskArtifactUpdateEvent, TaskState, TaskStatus,
    TaskStatusUpdateEvent,
};

/// Shared application state for the A2A server routes.
#[derive(Clone)]
/// Fields are `pub(crate)` and there is no public constructor: the only
/// supported way to build a server is `build_app` / `build_app_with_auth` /
/// `serve` / `async_serve`, or `A2AServerFactory::create`. Keeping the struct
/// unconstructable from outside means adding a field later is not a breaking
/// change — three releases in a row had already broken hand-built `AppState`s.
pub struct AppState {
    pub(crate) executor: Arc<ApCoreAgentExecutor>,
    pub(crate) task_store: Arc<dyn TaskStore>,
    /// Known skill (module) ids from the registry, for request-time validation.
    pub(crate) skill_ids: Arc<HashSet<String>>,
    /// Per-skill input schema, so an inbound `TextPart` can be parsed against
    /// the schema the module actually declares.
    pub(crate) input_schemas: Arc<HashMap<String, Value>>,
    /// The **public** card served at `/.well-known/*`, already filtered for the
    /// anonymous principal at build time (srs FR-AGC-003). Filtering once rather
    /// than per request is what keeps an auth-exempt route from letting any
    /// anonymous client drive one governance audit write per skill per request.
    pub(crate) agent_card: Arc<Value>,
    /// The **extended** card (srs FR-AGC-004), filtered per authenticated
    /// identity. `None` when no `Authenticator` is configured, which is also
    /// when `capabilities.extendedAgentCard` is `false`.
    pub(crate) extended_card: Option<Arc<FilteredCard>>,
    /// Agent card enriched with per-skill `_inputSchemas` for the Explorer UI.
    /// Filtered per caller like the extended card: the Explorer sits behind the
    /// auth middleware whenever one is configured.
    pub(crate) explorer_card: Arc<FilteredCard>,
    pub(crate) cancel_tokens: Arc<Mutex<HashMap<String, CancelToken>>>,
    /// Per-task push-notification webhook configs
    /// (`tasks/pushNotificationConfig/*`), scoped to their owner by the store.
    pub(crate) push_config_store: Arc<dyn PushConfigStore>,
    pub(crate) http: reqwest::Client,
}

/// Embedded Explorer UI (served at the explorer prefix).
const EXPLORER_HTML: &str = include_str!("../explorer/index.html");

/// Serve the Explorer single-page UI.
pub async fn explorer_html() -> Html<&'static str> {
    Html(EXPLORER_HTML)
}

/// Serve the public Agent Card (srs FR-AGC-003).
///
/// Already filtered for the anonymous principal at build time, so this is a
/// memory read — no `ACL::check`, no audit write, no per-caller cache lookup.
pub async fn public_agent_card(State(state): State<AppState>) -> Json<Value> {
    Json(state.agent_card.as_ref().clone())
}

/// Serve the extended Agent Card for the authenticated caller (srs FR-AGC-004).
///
/// Returns `None` when no `Authenticator` is configured, which is exactly when
/// `capabilities.extendedAgentCard` is `false` — a binding serves every
/// capability it advertises and advertises none it does not serve
/// (srs FR-AGC-006).
pub(crate) fn extended_agent_card(state: &AppState, identity: Option<&Identity>) -> Option<Value> {
    state
        .extended_card
        .as_ref()
        .map(|card| card.for_caller(&state.executor, identity).as_ref().clone())
}

/// Serve the Explorer's agent card (with `_inputSchemas`).
pub async fn explorer_card(
    State(state): State<AppState>,
    AuthIdentity(identity): AuthIdentity,
) -> Json<Value> {
    Json(
        state
            .explorer_card
            .for_caller(&state.executor, identity.as_ref())
            .as_ref()
            .clone(),
    )
}

/// Remove from a card the skills `identity` is not allowed to invoke.
///
/// The ACL gated the call but not the advertisement, so a deny-all-but-one ACL
/// still listed every module — telling a caller about capabilities it will
/// always be refused, and disclosing the module inventory of a locked-down
/// deployment. apcore's ACL is the authority on who may invoke what, and the
/// discovery surface reflects that authority rather than ignoring it.
///
/// Called with `identity = None` once at build time to produce the **public**
/// card (srs FR-AGC-003), and per authenticated identity to produce the
/// **extended** card (srs FR-AGC-004).
///
/// No ACL configured is the common case and costs nothing: the card is
/// returned as-is.
///
/// The filter and the enforcement path must reach the *same* verdict, or the
/// card advertises skills every call will refuse, or hides skills the caller can
/// invoke. Both failures existed: the filter evaluated the authenticated
/// principal, while an inbound A2A request reaches apcore's `BuiltinACLCheck` as
/// `@external` no matter who sent it, and the filter passed `ctx: None`, which
/// makes apcore's `check_conditions` return `false` — so a rule with a
/// `conditions:` block was inert here and live there. A `deny @external` rule
/// therefore advertised the whole inventory to an authenticated caller and then
/// refused every call.
///
/// The two surfaces now agree by construction: each skill is checked exactly as
/// the pipeline checks it — against
/// `ApCoreAgentExecutor::acl_context(identity).child(skill_id)`, which is what
/// `BuiltinContextCreation` hands to `BuiltinACLCheck` — using that context's
/// own `caller_id`. Deriving the caller from `child()` rather than hardcoding
/// `@external` is what makes the agreement structural: it is the same one line
/// of apcore, so the two cannot drift apart under a pipeline change.
///
/// `caller_id` is `None` here for every inbound request, authenticated or not,
/// and `ACL::check` maps that to `@external`. That is apcore's contract, not a
/// gap: `caller_id` names the *calling module* in a nested call chain and is
/// managed exclusively by `Context::child`. The authenticated principal is
/// carried in `identity` and reaches the ACL through the context passed here,
/// so `@system` and the `identity_types` / `roles` conditions discriminate
/// callers on both surfaces.
///
/// Authorization and approval are two independent results, not one (apcore
/// PROTOCOL_SPEC §6.1.6), and this reads them apart. `ACL::check` folds them
/// into a boolean that **fails closed** on an approval requirement — correct for
/// a caller about to execute, wrong for a discovery surface, where it would
/// delete a skill from the extended card for the one reason FR-AGC-004 says to
/// keep it. `ACL::check_access` carries both axes: `is_allowed()` decides
/// visibility, and `approval_required` decides only whether the public card is
/// the right surface, which is what `hide_approval_gated` selects.
///
/// The projection argument is `None`, because a card is discovery and there is
/// no call site yet. An `arguments` condition (§6.1.7) is therefore unevaluable,
/// so a rule carrying one neither denies nor grants — but an `allow` rule's
/// `approval: required` stays *pending* and composes with whatever grants
/// (§6.1.1 rule 5). A skill gated only for some argument shapes thus reports
/// `approval_required` here: at discovery time "this may need approval" is the
/// honest answer, and it is the one that keeps such a skill off the public card.
fn acl_filtered_card(
    card: &Value,
    executor: &ApCoreAgentExecutor,
    identity: Option<&Identity>,
    hide_approval_gated: bool,
) -> Value {
    let Some(acl) = executor.acl() else {
        return card.clone();
    };
    let base = executor.acl_context(identity.cloned());

    let mut filtered = card.clone();
    if let Some(skills) = filtered.get_mut("skills").and_then(Value::as_array_mut) {
        skills.retain(|skill| {
            skill.get("id").and_then(Value::as_str).is_some_and(|id| {
                let ctx = base.child(id);
                let decision = acl.check_access(ctx.caller_id.as_deref(), id, Some(&ctx), None);
                decision.is_allowed() && !(hide_approval_gated && decision.approval_required)
            })
        });
    }
    filtered
}

/// apcore's reserved namespace for the runtime's own management modules
/// (apcore `PROTOCOL_SPEC` §6.7) — `system.health.*`, `system.usage.*`,
/// `system.manifest.*` and, under the second opt-in, `system.control.*`. apcore
/// identifies the surface by this prefix itself, in
/// `Executor::governance_state`, so matching on it conveys apcore's own
/// boundary rather than inventing one.
pub(crate) const SYSTEM_NAMESPACE: &str = "system.";

/// Whether `skill_id` is one of apcore's management modules.
pub(crate) fn is_system_skill(skill_id: &str) -> bool {
    skill_id.starts_with(SYSTEM_NAMESPACE)
}

/// Build the public Agent Card: what an **unauthenticated** caller could
/// actually invoke (srs FR-AGC-003).
///
/// Four subtractions from the full card:
///
/// 1. skills in apcore's reserved `system.*` management namespace,
/// 2. skills the ACL denies to the anonymous principal (`@external`),
/// 3. skills the ACL allows that principal but gates behind a human
///    (`approval: required`, apcore >= 0.28.0), and
/// 4. skills whose module is annotated `requires_approval`.
///
/// 3 and 4 are the two sources PROTOCOL_SPEC §6.9 composes by **union**. Since
/// apcore 0.28.0 the annotation is one source among several, so reading it alone
/// would leave a skill on the public card that an anonymous caller cannot in
/// fact just call.
///
/// 1 is the only subtraction that is **unconditional**, and the only one that
/// still holds with no ACL configured — where 2 and 3 are empty and 4 covers
/// just `system.control.*`, leaving the six read modules to publish the
/// deployment's module inventory, health and usage to any anonymous caller
/// (srs FR-AGC-003 criteria 12 and 13). `ACL::discover` yields `Ok(None)` for a
/// missing root by design, so "no ACL at all" is the default rather than an edge
/// case.
///
/// An approval gate is not a refusal — it is a prompt the caller can satisfy —
/// so those skills reappear on the extended card, which is what gives
/// `capabilities.extendedAgentCard` something to mean. `system.*` reappears
/// there too (criterion 11), filtered by the ACL like any other skill: the
/// namespace exclusion is a property of the public card, not of the skill.
///
/// This resolves exactly one identity, so it runs once at build time. A
/// per-caller filter on `/.well-known/` would be strictly more accurate and
/// unaffordable: that route is auth-exempt by design, so every anonymous request
/// would drive `skills.len()` calls into the consumer's ACL audit sink.
pub(crate) fn public_card(
    card: &Value,
    executor: &ApCoreAgentExecutor,
    approval_gated: &HashSet<String>,
) -> Value {
    // The management namespace goes first and unconditionally, before the ACL is
    // consulted at all: skipping the check for these ids also keeps `system.*`
    // out of the audit trail of a decision whose answer cannot change the
    // outcome.
    let mut without_system = card.clone();
    if let Some(skills) = without_system
        .get_mut("skills")
        .and_then(Value::as_array_mut)
    {
        skills.retain(|skill| {
            !skill
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(is_system_skill)
        });
    }

    let mut filtered = acl_filtered_card(&without_system, executor, None, true);
    if let Some(skills) = filtered.get_mut("skills").and_then(Value::as_array_mut) {
        skills.retain(|skill| {
            !skill
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| approval_gated.contains(id))
        });
    }
    filtered
}

/// How many per-caller filtered copies of one card are cached before the cache
/// is dropped and rebuilt.
const MAX_CACHED_FILTERED_CARDS: usize = 1024;

/// An Agent Card together with its per-caller ACL-filtered copies.
///
/// The filter is memoized because `ACL::check` calls the consumer's audit sink
/// — a synchronous `Fn(&AuditEntry)` that may write a file or a socket — once
/// per skill. `/.well-known/agent-card.json` is auth-exempt, so filtering on
/// every request let any anonymous client generate `skills.len()` governance
/// audit entries per request at arbitrary rate, each recording `decision:
/// "deny"` and indistinguishable from a real enforcement decision, while
/// blocking a tokio worker for the duration of the sink.
///
/// Memoizing is sound because the ACL cannot change for the life of the
/// process: it is installed on the `Executor` before `build_app` runs and is
/// then reachable only through an `Arc<ACL>`, which has no interior
/// mutability. The card itself is likewise built once.
///
/// The audit sink is still driven once per (caller, card) pair; nothing in
/// apcore's public `ACL` API can suppress it (`default_effect` is private, so
/// an audit-free twin cannot be reconstructed from `rules()`, and there is no
/// `clear_audit_logger`). Bounding the cache bounds the exposure instead.
pub struct FilteredCard {
    card: Arc<Value>,
    cache: Mutex<HashMap<String, Arc<Value>>>,
}

impl FilteredCard {
    pub fn new(card: Arc<Value>) -> Self {
        Self {
            card,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// The unfiltered card, as built at startup.
    pub fn unfiltered(&self) -> &Arc<Value> {
        &self.card
    }

    /// This card as `identity` may see it, computing and caching the filtered
    /// copy on first request from that principal.
    pub fn for_caller(
        &self,
        executor: &ApCoreAgentExecutor,
        identity: Option<&Identity>,
    ) -> Arc<Value> {
        let key = OwnerId::from_identity(identity).as_str().to_string();
        if let Some(cached) = self.cache.lock().unwrap().get(&key) {
            return cached.clone();
        }
        let filtered = Arc::new(acl_filtered_card(&self.card, executor, identity, false));
        let mut cache = self.cache.lock().unwrap();
        // A flat cap rather than an eviction order: entries are equivalent and
        // cheap to rebuild, so dropping the map is a fine way to stay bounded.
        if cache.len() >= MAX_CACHED_FILTERED_CARDS {
            cache.clear();
        }
        cache.insert(key, filtered.clone());
        filtered
    }
}

/// Extracts the authenticated apcore `Identity` from request extensions, if the
/// auth middleware inserted one. Always succeeds (identity is optional).
pub struct AuthIdentity(pub Option<Identity>);

impl<S: Send + Sync> FromRequestParts<S> for AuthIdentity {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(AuthIdentity(parts.extensions.get::<Identity>().cloned()))
    }
}

fn rpc_result(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_error(id: &Value, code: i32, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Render one stream event as an SSE frame.
///
/// The `data:` payload is a full JSON-RPC response carrying the event as its
/// `result`, which is what both upstream A2A servers put on the wire
/// (a2a-python `jsonrpc_dispatcher.py` `_wrap_stream` ->
/// `JSONRPC20Response(result=..., _id=request_id).data`; a2a-js
/// `jsonrpc_transport_handler.ts` `yield { jsonrpc: '2.0', id: requestId,
/// result: StreamResponse.toJSON(event) }`). A bare payload cannot be parsed by
/// an off-the-shelf `a2a-python` / `a2a-js` client.
///
/// No `kind` discriminator and no `final` flag: A2A 1.0 discriminates by the
/// `oneof` wrapper key and dropped `final` in favour of the terminal
/// `TaskState`. Both the spec repo and both upstream SDKs agree on those two.
///
/// The absent SSE `id:` line is a **deliberate deviation from the spec repo**,
/// not a case of the same agreement. `docs/spec/srs.md` ("SSE event format"),
/// `docs/features/streaming.md` (`_format_sse_event(event, event_id)`) and
/// `docs/spec/tech-design.md` ("Assign monotonic id") all mandate a monotonic
/// `id:`. Neither upstream server emits one — a2a-python's `_wrap_stream` and
/// a2a-js's `jsonrpc_transport_handler.ts` yield `data:` only — so emitting it
/// would put this server alone on the wire, and it buys nothing today because
/// `tasks/resubscribe` / `Last-Event-ID` replay is not implemented. Revisit
/// together with resubscribe support, or amend the spec.
///
/// The three refusals are recorded separately because the earlier comment
/// presented all three as backed by both the spec and both SDKs, which is true
/// of two of them.
fn sse_event(id: &Value, event: &StreamEvent) -> Event {
    let payload = serde_json::to_value(event).unwrap_or(Value::Null);
    Event::default()
        .json_data(rpc_result(id, payload))
        .unwrap_or_default()
}

const CODE_PARSE_ERROR: i32 = -32700;
const CODE_INVALID_REQUEST: i32 = -32600;
const CODE_METHOD_NOT_FOUND: i32 = -32601;
const CODE_INVALID_PARAMS: i32 = -32602;
const CODE_TASK_NOT_FOUND: i32 = -32001;
const CODE_TASK_NOT_CANCELABLE: i32 = -32002;
const CODE_INTERNAL_ERROR: i32 = -32603;
/// A2A 1.0 `ExtendedAgentCardNotConfiguredError`. Returned when a client calls
/// `GetExtendedAgentCard` on a server with no `Authenticator` — the same server
/// whose card reports `capabilities.extendedAgentCard = false`.
const CODE_EXTENDED_CARD_NOT_CONFIGURED: i32 = -32007;

/// Extract `metadata.skillId` from the JSON-RPC params.
fn skill_id_of(params: &Value) -> Option<String> {
    params
        .get("metadata")
        .and_then(|m| m.get("skillId"))
        .or_else(|| {
            params
                .get("message")
                .and_then(|m| m.get("metadata"))
                .and_then(|m| m.get("skillId"))
        })
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Map an apcore execution error to the resulting terminal task status.
///
/// This is the surface that matters most on `message/send`: that response is a
/// JSON-RPC `result`, so the error *code* never reaches the caller at all —
/// the state and this message are the entire payload it receives.
fn error_to_status(err: &ModuleError, disclose_refusal_reason: bool) -> TaskStatus {
    match err.code {
        ErrorCode::ExecutionCancelled => TaskStatus::with_message(
            TaskState::Canceled,
            Message::agent_text("Execution cancelled"),
        ),
        ErrorCode::ApprovalPending => {
            TaskStatus::with_message(TaskState::InputRequired, Message::agent_text(&err.message))
        }
        ErrorCode::ModuleTimeout => TaskStatus::with_message(
            TaskState::Failed,
            Message::agent_text("Execution timed out"),
        ),
        // A2A 1.0 defines `rejected` as a terminal state, and a policy refusal
        // is what it describes (srs FR-ERR-012). `failed` said only "something
        // broke", which for a refusal is both wrong and an invitation to retry.
        ErrorCode::ACLDenied | ErrorCode::ApprovalDenied | ErrorCode::ApprovalTimeout => {
            TaskStatus::with_message(
                TaskState::Rejected,
                Message::agent_text(failure_text(err, disclose_refusal_reason)),
            )
        }
        _ => TaskStatus::with_message(
            TaskState::Failed,
            Message::agent_text(failure_text(err, disclose_refusal_reason)),
        ),
    }
}

/// Caller-facing text for a FAILED task status.
///
/// Delegates to [`ErrorMapper`], the crate's single error-redaction policy, so
/// the task-status surface classifies exactly like the JSON-RPC surface instead
/// of collapsing every code to one string:
///
/// - internal / unrecognized errors keep the fixed `"Internal server error"`
///   (srs FR-ERR-004 / FR-ERR-008, locked by `error_mapping.json` and by
///   `streaming_events.json`'s `error_midstream_skips_marker`);
/// - `ACL_DENIED` stays masked as `"Task not found"` (srs FR-ERR-003);
/// - caller-fixable classes (schema validation, invalid input, unknown module)
///   carry their sanitized detail, which srs FR-ERR-002 requires precisely so a
///   caller "can correct their input without guessing".
///
/// For those detail-carrying classes the error's `ai_guidance` is appended when
/// apcore supplied one: it exists to tell an agent what to do next, and an A2A
/// caller sees only this status message. It is withheld for every other class,
/// where the message is a fixed per-class string that must stay fixed.
///
/// The gate is [`carries_caller_detail`] — the same partition [`ErrorMapper`]
/// itself branches on. Gating on `err.user_fixable` instead was a different
/// partition: six codes are `user_fixable = Some(true)` yet fall into
/// `ErrorMapper`'s catch-all, so e.g. a `DEPENDENCY_NOT_FOUND` failure sent the
/// caller `"Internal server error (module 'x' requires 'y' >= 2.1; …)"` — the
/// deliberately-opaque string extended with dependency-graph detail that
/// `sanitize_message` does not strip (it removes only paths and
/// traceback-shaped lines, not module ids, versions, env-var names or
/// hostnames). `user_fixable` is settable per-error by the module author, so
/// any module could widen the fixed string further.
fn failure_text(err: &ModuleError, disclose_refusal_reason: bool) -> String {
    let message = ErrorMapper::to_jsonrpc_error_with(err, disclose_refusal_reason).message;
    match err.ai_guidance.as_deref() {
        Some(guidance)
            if carries_caller_detail(err, disclose_refusal_reason)
                && !guidance.trim().is_empty() =>
        {
            format!("{message} ({})", sanitize_message(guidance))
        }
        _ => message,
    }
}

/// Reject a request whose `Content-Type` is present but not JSON (HTTP 415,
/// per the spec's transport error table). A missing header is tolerated, as in
/// the upstream A2A Python dispatcher, which has no content-type guard at all.
fn unsupported_media_type(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|value| {
            let media_type = value.split(';').next().unwrap_or("").trim();
            !media_type.eq_ignore_ascii_case("application/json")
                && !media_type.ends_with("+json")
                && !media_type.is_empty()
        })
}

/// Validate the JSON-RPC 2.0 envelope, returning the error response body when
/// it is malformed.
///
/// A missing `id` is deliberately not an error: that is a notification, which
/// is valid JSON-RPC 2.0, and both upstream A2A servers answer it (with
/// `"id": null`) rather than rejecting it.
fn envelope_error(req: &Value) -> Option<Value> {
    if req.is_array() {
        // Neither upstream server implements batching; a2a-python rejects it
        // explicitly with this code and message, which is the JSON-RPC-correct
        // answer (a2a-js only reaches -32602 by falling through its validator).
        return Some(rpc_error(
            &Value::Null,
            CODE_INVALID_REQUEST,
            "Batch requests are not supported",
        ));
    }
    if !req.is_object() {
        return Some(rpc_error(
            &Value::Null,
            CODE_INVALID_REQUEST,
            "Invalid Request",
        ));
    }
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    match req.get("jsonrpc").and_then(Value::as_str) {
        Some("2.0") => {}
        Some(_) => {
            return Some(rpc_error(
                &id,
                CODE_INVALID_REQUEST,
                "Invalid Request: jsonrpc must be '2.0'",
            ))
        }
        None => return Some(rpc_error(&id, CODE_INVALID_REQUEST, "Invalid Request")),
    }
    if req.get("method").and_then(Value::as_str).is_none() {
        return Some(rpc_error(&id, CODE_INVALID_REQUEST, "Invalid Request"));
    }
    None
}

/// JSON-RPC dispatch entry point (`POST /`).
///
/// Takes the raw body rather than `Json<Value>` so unparseable input becomes a
/// JSON-RPC `-32700` response instead of axum's `text/plain` 400, which no
/// JSON-RPC client can read.
pub async fn jsonrpc_handler(
    State(state): State<AppState>,
    AuthIdentity(identity): AuthIdentity,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if unsupported_media_type(&headers) {
        return (StatusCode::UNSUPPORTED_MEDIA_TYPE, "Unsupported media type").into_response();
    }
    let req: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => {
            return Json(rpc_error(&Value::Null, CODE_PARSE_ERROR, "Parse error")).into_response()
        }
    };
    if let Some(error) = envelope_error(&req) {
        return Json(error).into_response();
    }

    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let ctx = CallContext::from_identity(identity.as_ref());

    match method {
        "message/send" => match handle_send(&state, identity, &ctx, &params).await {
            Ok(task) => Json(rpc_result(&id, task)).into_response(),
            Err((code, msg)) => Json(rpc_error(&id, code, &msg)).into_response(),
        },
        "message/stream" => handle_stream(&state, identity, &ctx, &params, id).await,
        "tasks/get" => match handle_get(&state, &ctx, &params).await {
            Ok(task) => Json(rpc_result(&id, task)).into_response(),
            Err((code, msg)) => Json(rpc_error(&id, code, &msg)).into_response(),
        },
        "tasks/cancel" => match handle_cancel(&state, &ctx, &params).await {
            Ok(task) => Json(rpc_result(&id, task)).into_response(),
            Err((code, msg)) => Json(rpc_error(&id, code, &msg)).into_response(),
        },
        "ListTasks" => match handle_list(&state, &ctx).await {
            Ok(tasks) => Json(rpc_result(&id, json!({ "tasks": tasks }))).into_response(),
            Err((code, msg)) => Json(rpc_error(&id, code, &msg)).into_response(),
        },
        // srs FR-AGC-004 / FR-AGC-006. Previously this crate advertised
        // `capabilities.extendedAgentCard` and dispatched nothing, so a client
        // that read the flag and called the method — which A2A §3.2.x entitles
        // it to do — got method-not-found.
        "GetExtendedAgentCard" => match extended_agent_card(&state, identity.as_ref()) {
            Some(card) => Json(rpc_result(&id, card)).into_response(),
            None => Json(rpc_error(
                &id,
                CODE_EXTENDED_CARD_NOT_CONFIGURED,
                "Extended agent card is not configured",
            ))
            .into_response(),
        },
        "tasks/pushNotificationConfig/set" => match handle_push_set(&state, &ctx, &params).await {
            Ok(v) => Json(rpc_result(&id, v)).into_response(),
            Err((code, msg)) => Json(rpc_error(&id, code, &msg)).into_response(),
        },
        "tasks/pushNotificationConfig/get" => match handle_push_get(&state, &ctx, &params).await {
            Ok(v) => Json(rpc_result(&id, v)).into_response(),
            Err((code, msg)) => Json(rpc_error(&id, code, &msg)).into_response(),
        },
        "tasks/pushNotificationConfig/delete" => {
            match handle_push_delete(&state, &ctx, &params).await {
                Ok(v) => Json(rpc_result(&id, v)).into_response(),
                Err((code, msg)) => Json(rpc_error(&id, code, &msg)).into_response(),
            }
        }
        _ => Json(rpc_error(&id, CODE_METHOD_NOT_FOUND, "Method not found")).into_response(),
    }
}

/// Outcome of a failed request parse.
///
/// A missing/invalid `message` envelope is a protocol error (`Rpc`) with no task.
/// A missing `metadata.skillId` or an unconvertible Part set is a task-level
/// failure (`Task`) that must surface as a FAILED task — matching Python/TS and
/// Rust's own unknown-skill path (which already emits a FAILED task).
enum ParseFailure {
    Rpc(i32, String),
    Task {
        task_id: String,
        context_id: String,
        message: String,
    },
}

/// Parse the inbound message + skillId + inputs shared by send/stream.
fn parse_request(
    state: &AppState,
    params: &Value,
) -> Result<(String, String, String, Value), ParseFailure> {
    // The message envelope is required at the protocol level: without it there is
    // no task context to fail (Python/TS reach the executor only with a message).
    // A present-but-unreadable message reports what actually failed to parse: a
    // single "Missing or invalid parameter: message" named the wrong field for
    // every malformed part, role or id inside it.
    let raw_message = params.get("message").cloned().ok_or(ParseFailure::Rpc(
        CODE_INVALID_PARAMS,
        "Missing required parameter: message".to_string(),
    ))?;
    let message: Message = serde_json::from_value(raw_message).map_err(|e| {
        ParseFailure::Rpc(
            CODE_INVALID_PARAMS,
            format!(
                "Invalid parameter: message ({})",
                sanitize_message(&e.to_string())
            ),
        )
    })?;

    let task_id = Uuid::new_v4().to_string();
    let context_id = message
        .context_id
        .clone()
        .unwrap_or_else(|| task_id.clone());

    // From here, failures are task-level: a missing skillId or unconvertible parts
    // produce a FAILED task (not a JSON-RPC error), matching Python/TS.
    let skill_id = skill_id_of(params).ok_or_else(|| ParseFailure::Task {
        task_id: task_id.clone(),
        context_id: context_id.clone(),
        message: "Missing required parameter: metadata.skillId".to_string(),
    })?;

    // The module's own input schema decides how a TextPart is read: against an
    // object-typed schema its text is parsed as JSON, otherwise it is taken as a
    // raw string. Passing `None` here made every TextPart a bare string, so the
    // `application/json` input mode the Agent Card advertises only ever worked
    // via a DataPart. Python and TS both look the descriptor up and pass it
    // (apcore-a2a-python server/executor.py:129, apcore-a2a-typescript
    // src/server/executor.ts:128).
    let input_schema = state.input_schemas.get(&skill_id);
    let inputs = state
        .executor
        .part_converter()
        .parts_to_input(&message.parts, input_schema)
        .map_err(|e| ParseFailure::Task {
            task_id: task_id.clone(),
            context_id: context_id.clone(),
            message: e,
        })?;

    Ok((skill_id, task_id, context_id, inputs))
}

/// Build, persist, and webhook-notify a terminal FAILED task; returns its JSON.
/// Shared by the missing-skillId / unparseable-parts / unknown-skill paths so all
/// task-level failures produce an identical FAILED task shape (Python/TS parity).
async fn fail_task(
    state: &AppState,
    task_id: &str,
    context_id: &str,
    ctx: &CallContext,
    message: impl Into<String>,
) -> Value {
    let status = TaskStatus::with_message(TaskState::Failed, Message::agent_text(message));
    let task = Task {
        id: task_id.to_string(),
        context_id: context_id.to_string(),
        status: status.clone(),
        artifacts: vec![],
        history: vec![],
    };
    let task_json = serde_json::to_value(&task).unwrap();
    save_task(state, task_id, task_json.clone(), ctx).await;
    notify_push(
        state,
        task_id,
        ctx,
        &StreamEvent::StatusUpdate(TaskStatusUpdateEvent {
            task_id: task_id.to_string(),
            context_id: context_id.to_string(),
            status,
        }),
    )
    .await;
    task_json
}

/// Emit a task-level failure as an SSE stream (SUBMITTED then terminal FAILED),
/// mirroring how a normal stream surfaces task state. Used when a streamed request
/// has a missing skillId or unconvertible parts (Python/TS parity).
async fn failed_task_stream(
    state: &AppState,
    task_id: String,
    context_id: String,
    ctx: &CallContext,
    id: Value,
    message: String,
) -> Response {
    let status = TaskStatus::with_message(TaskState::Failed, Message::agent_text(message));
    let task = Task {
        id: task_id.clone(),
        context_id: context_id.clone(),
        status: status.clone(),
        artifacts: vec![],
        history: vec![],
    };
    save_task(state, &task_id, serde_json::to_value(&task).unwrap(), ctx).await;
    let terminal = StreamEvent::StatusUpdate(TaskStatusUpdateEvent {
        task_id: task_id.clone(),
        context_id: context_id.clone(),
        status,
    });
    notify_push(state, &task_id, ctx, &terminal).await;
    let events = vec![
        StreamEvent::StatusUpdate(TaskStatusUpdateEvent {
            task_id: task_id.clone(),
            context_id: context_id.clone(),
            status: TaskStatus::new(TaskState::Submitted),
        }),
        terminal,
    ];
    let event_stream =
        tokio_stream::iter(events).map(move |ev| Ok::<Event, Infallible>(sse_event(&id, &ev)));
    Sse::new(event_stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

fn submitted_task(task_id: &str, context_id: &str) -> Task {
    Task {
        id: task_id.to_string(),
        context_id: context_id.to_string(),
        status: TaskStatus::new(TaskState::Submitted),
        artifacts: vec![],
        history: vec![],
    }
}

/// `message/send` — run to completion, return the final Task.
async fn handle_send(
    state: &AppState,
    identity: Option<Identity>,
    ctx: &CallContext,
    params: &Value,
) -> Result<Value, (i32, String)> {
    let (skill_id, task_id, context_id, inputs) = match parse_request(state, params) {
        Ok(v) => v,
        Err(ParseFailure::Rpc(code, msg)) => return Err((code, msg)),
        Err(ParseFailure::Task {
            task_id,
            context_id,
            message,
        }) => return Ok(fail_task(state, &task_id, &context_id, ctx, message).await),
    };

    // Reject unknown skills with a FAILED task (Python/TS parity).
    if !state.skill_ids.contains(&skill_id) {
        return Ok(fail_task(
            state,
            &task_id,
            &context_id,
            ctx,
            format!("Skill not found: {skill_id}"),
        )
        .await);
    }

    let mut task = submitted_task(&task_id, &context_id);

    let cancel = CancelToken::new();
    register_cancel(state, &task_id, cancel.clone());
    // Unregister the cancel token on every exit path (success, error, panic);
    // a Drop guard mirrors handle_stream and avoids leaking the token.
    let _cancel_guard = CancelGuard {
        state: state.clone(),
        task_id: task_id.clone(),
    };

    save_task(state, &task_id, serde_json::to_value(&task).unwrap(), ctx).await;

    let result = state
        .executor
        .call(&skill_id, inputs, identity, cancel)
        .await;

    match result {
        Ok(output) => {
            let artifact = state
                .executor
                .part_converter()
                .output_to_parts(&output, &task_id);
            task.artifacts = vec![artifact];
            task.status = TaskStatus::new(TaskState::Completed);
        }
        Err(err) => {
            task.status = error_to_status(&err, state.executor.disclose_refusal_reason());
        }
    }
    let task_json = serde_json::to_value(&task).unwrap();
    state
        .task_store
        .save(&task_id, task_json.clone(), ctx)
        .await
        .map_err(|e| store_failure(&format!("task store write for {task_id}"), e))?;

    // Deliver the terminal status to any registered webhook.
    notify_push(
        state,
        &task_id,
        ctx,
        &StreamEvent::StatusUpdate(TaskStatusUpdateEvent {
            task_id: task_id.clone(),
            context_id: context_id.clone(),
            status: task.status.clone(),
        }),
    )
    .await;
    Ok(task_json)
}

/// `message/stream` — SSE stream of A2A 1.0 events.
async fn handle_stream(
    state: &AppState,
    identity: Option<Identity>,
    ctx: &CallContext,
    params: &Value,
    id: Value,
) -> Response {
    let (skill_id, task_id, context_id, inputs) = match parse_request(state, params) {
        Ok(v) => v,
        Err(ParseFailure::Rpc(code, msg)) => {
            return Json(rpc_error(&id, code, &msg)).into_response()
        }
        Err(ParseFailure::Task {
            task_id,
            context_id,
            message,
        }) => return failed_task_stream(state, task_id, context_id, ctx, id, message).await,
    };

    let cancel = CancelToken::new();
    register_cancel(state, &task_id, cancel.clone());

    let (tx, rx) = mpsc::channel::<StreamEvent>(16);
    let state2 = state.clone();
    // The spawned task outlives this call, so it needs an owned context to
    // scope its own stores writes and webhook lookup.
    let ctx2 = ctx.clone();
    let skill_known = state.skill_ids.contains(&skill_id);

    tokio::spawn(async move {
        // Unregister the cancel token on every exit path (success, error, panic).
        let _cancel_guard = CancelGuard {
            state: state2.clone(),
            task_id: task_id.clone(),
        };
        let send = |ev: StreamEvent| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(ev).await;
            }
        };

        // submitted → working
        send(StreamEvent::StatusUpdate(TaskStatusUpdateEvent {
            task_id: task_id.clone(),
            context_id: context_id.clone(),
            status: TaskStatus::new(TaskState::Submitted),
        }))
        .await;

        // Reject unknown skills with a terminal FAILED status (Python/TS parity).
        if !skill_known {
            let status = TaskStatus::with_message(
                TaskState::Failed,
                Message::agent_text(format!("Skill not found: {skill_id}")),
            );
            let task = Task {
                id: task_id.clone(),
                context_id: context_id.clone(),
                status: status.clone(),
                artifacts: vec![],
                history: vec![],
            };
            save_task(
                &state2,
                &task_id,
                serde_json::to_value(&task).unwrap(),
                &ctx2,
            )
            .await;
            let terminal_event = StreamEvent::StatusUpdate(TaskStatusUpdateEvent {
                task_id: task_id.clone(),
                context_id: context_id.clone(),
                status,
            });
            notify_push(&state2, &task_id, &ctx2, &terminal_event).await;
            send(terminal_event).await;
            return;
        }

        send(StreamEvent::StatusUpdate(TaskStatusUpdateEvent {
            task_id: task_id.clone(),
            context_id: context_id.clone(),
            status: TaskStatus::new(TaskState::Working),
        }))
        .await;

        let mut chunks = state2
            .executor
            .stream_channel(skill_id, inputs, identity, cancel);
        let mut idx: usize = 0;
        let mut error: Option<ModuleError> = None;

        while let Some(item) = chunks.next().await {
            match item {
                Ok(chunk) => {
                    // task_id is always a fresh uuid here, so the artifact id is
                    // stable (`art-{task_id}`) across chunks.
                    let artifact = state2
                        .executor
                        .part_converter()
                        .output_to_parts(&chunk, &task_id);
                    send(StreamEvent::ArtifactUpdate(TaskArtifactUpdateEvent {
                        task_id: task_id.clone(),
                        context_id: context_id.clone(),
                        artifact,
                        append: idx > 0,
                        last_chunk: false,
                    }))
                    .await;
                    idx += 1;
                }
                Err(e) => {
                    error = Some(e);
                    break;
                }
            }
        }

        // On successful completion, emit a terminal empty-artifact marker
        // (last_chunk=true, art-{task_id}) before the COMPLETED status — required
        // by the A2A streaming contract (streaming.md "Final chunk: lastChunk=True")
        // and matching Python/TS. Errors skip the marker and go straight to FAILED.
        if error.is_none() {
            send(StreamEvent::ArtifactUpdate(TaskArtifactUpdateEvent {
                task_id: task_id.clone(),
                context_id: context_id.clone(),
                artifact: Artifact::new(format!("art-{task_id}"), vec![]),
                append: idx > 0,
                last_chunk: true,
            }))
            .await;
        }

        let final_status = match &error {
            None => TaskStatus::new(TaskState::Completed),
            Some(e) => error_to_status(e, state2.executor.disclose_refusal_reason()),
        };
        // Persist terminal task.
        let task = Task {
            id: task_id.clone(),
            context_id: context_id.clone(),
            status: final_status.clone(),
            artifacts: vec![],
            history: vec![],
        };
        save_task(
            &state2,
            &task_id,
            serde_json::to_value(&task).unwrap(),
            &ctx2,
        )
        .await;
        let terminal_event = StreamEvent::StatusUpdate(TaskStatusUpdateEvent {
            task_id: task_id.clone(),
            context_id: context_id.clone(),
            status: final_status,
        });
        // Deliver the terminal status to any registered webhook.
        notify_push(&state2, &task_id, &ctx2, &terminal_event).await;
        send(terminal_event).await;
        // `_cancel_guard` unregisters the cancel token on drop (all exit paths).
    });

    let event_stream =
        ReceiverStream::new(rx).map(move |ev| Ok::<Event, Infallible>(sse_event(&id, &ev)));

    Sse::new(event_stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// `tasks/get` — return a stored task by id, if the caller owns it.
async fn handle_get(
    state: &AppState,
    ctx: &CallContext,
    params: &Value,
) -> Result<Value, (i32, String)> {
    let task_id = params.get("id").and_then(Value::as_str).ok_or((
        CODE_INVALID_PARAMS,
        "Missing required parameter: id".to_string(),
    ))?;
    // Another principal's task reads back as absent — the store scopes by
    // `ctx.owner()`. Masked as "not found" rather than "forbidden", matching
    // how ACL denials are reported (srs FR-ERR-003): a caller must not learn
    // that another principal's task id exists.
    match state.task_store.get(task_id, ctx).await {
        Ok(Some(task)) => Ok(task),
        Ok(None) => Err((CODE_TASK_NOT_FOUND, "Task not found".to_string())),
        Err(e) => Err(store_failure(&format!("task store read for {task_id}"), e)),
    }
}

/// `ListTasks` — return the calling principal's tasks.
///
/// Named `ListTasks` because that is what A2A 1.0 calls it; 0.3 had no
/// task-listing method. The `tasks/list` spelling this used until 0.5.0 was
/// neither, so no upstream SDK ever routed it.
///
/// Unscoped, this returned every caller's tasks including the full stdout of
/// tasks other callers submitted. The upstream A2A stores scope by owner
/// (`a2a-python`'s `OwnerResolver`, `a2a-js`'s per-context bucket); the spec
/// repo is silent on it, so this follows upstream.
async fn handle_list(state: &AppState, ctx: &CallContext) -> Result<Vec<Value>, (i32, String)> {
    state
        .task_store
        .list(&ListParams::new(), ctx)
        .await
        .map_err(|e| store_failure("task store list", e))
}

/// `tasks/cancel` — signal cooperative cancellation and mark the task canceled.
///
/// Guarded per srs FR-TSK-005 and the upstream A2A reference handler
/// (`a2a-python` `DefaultRequestHandler.on_cancel_task`): look the task up
/// first, `-32001` when it does not exist, `-32002` when it is already
/// terminal, and cancel only from a non-terminal state. Writing
/// unconditionally both fabricated tasks for unknown ids — contradicting
/// `tasks/get`, which reports the same id as missing — and replaced a
/// COMPLETED task's artifacts, destroying the result it had already produced.
async fn handle_cancel(
    state: &AppState,
    ctx: &CallContext,
    params: &Value,
) -> Result<Value, (i32, String)> {
    let task_id = params.get("id").and_then(Value::as_str).ok_or((
        CODE_INVALID_PARAMS,
        "Missing required parameter: id".to_string(),
    ))?;

    // Another principal's task reads back as absent, so it is reported as
    // missing rather than as forbidden.
    let stored = match state.task_store.get(task_id, ctx).await {
        Ok(Some(stored)) => stored,
        Ok(None) => return Err((CODE_TASK_NOT_FOUND, "Task not found".to_string())),
        Err(e) => return Err(store_failure(&format!("task store read for {task_id}"), e)),
    };

    // A task whose stored JSON cannot be read back is unusable, not cancelable.
    let mut task: Task = serde_json::from_value(stored)
        .map_err(|_| (CODE_TASK_NOT_FOUND, "Task not found".to_string()))?;

    if task.status.state.is_terminal() {
        let state_name = serde_json::to_value(task.status.state)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        return Err((
            CODE_TASK_NOT_CANCELABLE,
            format!("Task is not cancelable: current state is {state_name}"),
        ));
    }

    if let Some(token) = state.cancel_tokens.lock().unwrap().remove(task_id) {
        token.cancel();
    }

    // Update in place: artifacts and history a non-terminal task already
    // accumulated belong to it and survive the cancellation.
    task.status = TaskStatus::with_message(
        TaskState::Canceled,
        Message::agent_text("Canceled by client"),
    );
    let context_id = task.context_id.clone();
    let task_json = serde_json::to_value(&task).unwrap();
    // Cancelling *is* this write, so unlike the send/stream paths a failure
    // here is not "work happened but could not be recorded" — it is the
    // operation failing. Answering CANCELED anyway would leave the caller
    // believing a task is cancelled while the store still reads WORKING. The
    // cancel token is already signalled, so retrying is the right response.
    state
        .task_store
        .save(task_id, task_json.clone(), ctx)
        .await
        .map_err(|e| store_failure(&format!("task store write for {task_id}"), e))?;

    // Deliver the terminal CANCELED status to any registered webhook.
    notify_push(
        state,
        task_id,
        ctx,
        &StreamEvent::StatusUpdate(TaskStatusUpdateEvent {
            task_id: task_id.to_string(),
            context_id,
            status: task.status.clone(),
        }),
    )
    .await;
    Ok(task_json)
}

/// Read a task-addressed request's `id` and confirm the caller owns that task.
///
/// The six task-addressed methods must all be scoped, not just the three that
/// read task state: a principal holding another's task id could otherwise point
/// its terminal `statusUpdate` at an attacker-controlled webhook, or silently
/// suppress the owner's notifications by deleting their config. Masked as
/// "not found" like the rest (srs FR-ERR-003) so ids cannot be probed. Upstream
/// scopes these three the same way — `a2a-python`'s
/// `on_set/get_task_push_notification_config` both load the task from the
/// context-scoped `task_store` first and raise `TaskNotFoundError`.
async fn owned_task_id<'a>(
    state: &AppState,
    ctx: &CallContext,
    params: &'a Value,
) -> Result<&'a str, (i32, String)> {
    let task_id = params.get("id").and_then(Value::as_str).ok_or((
        CODE_INVALID_PARAMS,
        "Missing required parameter: id".to_string(),
    ))?;
    match state.task_store.get(task_id, ctx).await {
        Ok(Some(_)) => Ok(task_id),
        Ok(None) => Err((CODE_TASK_NOT_FOUND, "Task not found".to_string())),
        Err(e) => Err(store_failure(&format!("task store read for {task_id}"), e)),
    }
}

/// `tasks/pushNotificationConfig/set` — register a webhook for a task.
async fn handle_push_set(
    state: &AppState,
    ctx: &CallContext,
    params: &Value,
) -> Result<Value, (i32, String)> {
    let task_id = owned_task_id(state, ctx, params).await?;
    let cfg = params.get("pushNotificationConfig").cloned().ok_or((
        CODE_INVALID_PARAMS,
        "Missing pushNotificationConfig".to_string(),
    ))?;
    if cfg.get("url").and_then(Value::as_str).is_none() {
        return Err((
            CODE_INVALID_PARAMS,
            "pushNotificationConfig.url is required".to_string(),
        ));
    }
    state
        .push_config_store
        .save(task_id, cfg.clone(), ctx)
        .await
        .map_err(|e| store_failure(&format!("push config store write for {task_id}"), e))?;
    Ok(json!({ "taskId": task_id, "pushNotificationConfig": cfg }))
}

/// `tasks/pushNotificationConfig/get` — return a task's webhook config.
async fn handle_push_get(
    state: &AppState,
    ctx: &CallContext,
    params: &Value,
) -> Result<Value, (i32, String)> {
    let task_id = owned_task_id(state, ctx, params).await?;
    match state.push_config_store.get(task_id, ctx).await {
        Ok(Some(cfg)) => Ok(json!({ "taskId": task_id, "pushNotificationConfig": cfg })),
        Ok(None) => Err((
            CODE_TASK_NOT_FOUND,
            "Push notification config not found".to_string(),
        )),
        Err(e) => Err(store_failure(
            &format!("push config store read for {task_id}"),
            e,
        )),
    }
}

/// `tasks/pushNotificationConfig/delete` — remove a task's webhook config.
async fn handle_push_delete(
    state: &AppState,
    ctx: &CallContext,
    params: &Value,
) -> Result<Value, (i32, String)> {
    let task_id = owned_task_id(state, ctx, params).await?;
    // Reporting success here would tell a caller its webhook is revoked while
    // deliveries keep going to that URL — the one failure mode this method must
    // not have, since revoking a leaked webhook is a reason to call it.
    state
        .push_config_store
        .delete(task_id, ctx)
        .await
        .map_err(|e| store_failure(&format!("push config store delete for {task_id}"), e))?;
    Ok(Value::Null)
}

/// Deliver an event to a task's registered webhook (if any), retrying with
/// exponential backoff. Fire-and-forget: spawned so it never blocks the caller.
async fn notify_push(state: &AppState, task_id: &str, ctx: &CallContext, event: &StreamEvent) {
    let cfg = match state.push_config_store.get(task_id, ctx).await {
        Ok(Some(c)) => c,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!("push config store read failed for {task_id}: {e}");
            return;
        }
    };
    let url = match cfg.get("url").and_then(Value::as_str) {
        Some(u) => u.to_string(),
        None => return,
    };
    let token = cfg.get("token").and_then(Value::as_str).map(str::to_string);
    let body = serde_json::to_value(event).unwrap_or(Value::Null);
    let http = state.http.clone();

    tokio::spawn(async move {
        // 3 attempts, exponential backoff 1s / 2s / 4s (matches Python/TS).
        let delays = [1u64, 2, 4];
        for (attempt, delay) in delays.iter().enumerate() {
            let mut req = http.post(&url).json(&body);
            if let Some(t) = &token {
                req = req.header("Authorization", format!("Bearer {t}"));
            }
            match req.send().await {
                Ok(resp) if resp.status().is_success() => return,
                _ => {
                    if attempt + 1 < delays.len() {
                        tokio::time::sleep(std::time::Duration::from_secs(*delay)).await;
                    }
                }
            }
        }
        tracing::warn!("push notification delivery to {url} failed after retries");
    });
}

/// Classify a store backend failure as an internal error, never as a missing
/// task.
///
/// Collapsing it to `-32001` reports "your task does not exist" when the truth
/// is that the backend is unreachable, and the two call for opposite responses:
/// an A2A agent reading *not found* re-submits work that is actually still
/// there, while *internal error* is correctly not caller-fixable. This is the
/// same rule the task-status surface follows (srs FR-ERR-004 / FR-ERR-008) —
/// internal failures keep the fixed string and stay distinct from anything the
/// caller could act on. The classification also has to be the same for reads
/// and writes: `set` reporting `-32603` while `get` reported `-32001` for one
/// and the same outage gave callers two contradictory signals.
fn store_failure(what: &str, e: StoreError) -> (i32, String) {
    tracing::warn!("{what}: {e}");
    (CODE_INTERNAL_ERROR, "Internal server error".to_string())
}

/// Persist a task, logging rather than propagating a store failure.
///
/// Every call site here has already run the task to a known state, so a failed
/// write cannot change what is returned to the caller — turning it into a
/// JSON-RPC error would report "failed" for work that actually succeeded. It
/// must not stay silent either: previously each of these was a bare `let _ =`,
/// so a store outage produced tasks that `message/send` reported as COMPLETED
/// and `tasks/get` then reported as missing, with nothing logged in between.
async fn save_task(state: &AppState, task_id: &str, task: Value, ctx: &CallContext) {
    if let Err(e) = state.task_store.save(task_id, task, ctx).await {
        tracing::warn!("task store write failed for {task_id}: {e}");
    }
}

fn register_cancel(state: &AppState, task_id: &str, token: CancelToken) {
    state
        .cancel_tokens
        .lock()
        .unwrap()
        .insert(task_id.to_string(), token);
}

fn unregister_cancel(state: &AppState, task_id: &str) {
    state.cancel_tokens.lock().unwrap().remove(task_id);
}

/// Drop guard that unregisters a task's cancel token on all exit paths
/// (normal return, early return, or panic), preventing token leaks.
struct CancelGuard {
    state: AppState,
    task_id: String,
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        unregister_cancel(&self.state, &self.task_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Part;

    /// Extract the status message's text (if any).
    fn status_text(status: &TaskStatus) -> Option<String> {
        status.message.as_ref().and_then(|m| {
            m.parts.iter().find_map(|p| match p {
                Part::Text { text } => Some(text.clone()),
                _ => None,
            })
        })
    }

    #[test]
    fn internal_error_yields_fixed_internal_server_error_message() {
        // Regression (A-D-015): an internal or unrecognized error must emit the
        // fixed "Internal server error" message, never leaking the raw error
        // text (srs FR-ERR-004 / FR-ERR-008, Python/TS parity).
        let err = ModuleError::new(
            ErrorCode::GeneralInternalError,
            "super secret internal detail leaking through",
        );
        let status = error_to_status(&err, false);
        assert_eq!(status.state, TaskState::Failed);
        assert_eq!(
            status_text(&status).as_deref(),
            Some("Internal server error")
        );
    }

    #[test]
    fn acl_denial_is_rejected_not_failed_and_says_access_denied() {
        // srs FR-ERR-003 / FR-ERR-012: the class of refusal reaches the caller,
        // the detail does not. This is the surface that matters on
        // `message/send`, where the JSON-RPC code never reaches the caller at
        // all — the state and this message are the whole payload.
        let err = ModuleError::new(
            ErrorCode::ACLDenied,
            "caller alice denied module admin.wipe",
        );
        let status = error_to_status(&err, false);
        assert_eq!(status.state, TaskState::Rejected);
        let text = status_text(&status).unwrap();
        assert_eq!(text, "Access denied");
        // "Task not found" sent an agent back to retry the one thing that was
        // fine. It must not come back.
        assert_ne!(text, "Task not found");
        assert!(!text.contains("alice"));
        assert!(!text.contains("admin.wipe"));
    }

    #[test]
    fn approval_refusals_are_rejected_not_failed() {
        for (code, expected) in [
            (ErrorCode::ApprovalDenied, "Approval denied"),
            (ErrorCode::ApprovalTimeout, "Approval timed out"),
        ] {
            let err = ModuleError::new(code, "approval 7f3c1e for alice@example.com");
            let status = error_to_status(&err, false);
            assert_eq!(status.state, TaskState::Rejected, "{code:?}");
            let text = status_text(&status).unwrap();
            assert_eq!(text, expected);
            // Not the retryable catch-all these used to land on.
            assert_ne!(text, "Internal server error");
            assert!(!text.contains("alice@example.com"), "{code:?}");
            assert!(!text.contains("7f3c1e"), "{code:?}");
        }
    }

    #[test]
    fn approval_pending_stays_a_resumable_pause() {
        // The one approval code that must NOT move: it carries the approval_id
        // the caller resumes with, and `rejected` is terminal.
        let err = ModuleError::new(
            ErrorCode::ApprovalPending,
            "Approval required: approval_id=7f3c1e",
        );
        let status = error_to_status(&err, false);
        assert_eq!(status.state, TaskState::InputRequired);
        assert_eq!(
            status_text(&status).as_deref(),
            Some("Approval required: approval_id=7f3c1e"),
            "the message must reach the caller verbatim"
        );
    }

    #[test]
    fn disclose_refusal_reason_reaches_the_task_status_surface() {
        // The flag must move both surfaces together, or the JSON-RPC error and
        // the task status would disagree about the same refusal.
        let err = ModuleError::new(
            ErrorCode::ACLDenied,
            "caller alice denied module admin.wipe",
        );
        let status = error_to_status(&err, true);
        assert_eq!(
            status.state,
            TaskState::Rejected,
            "the flag must not move the state"
        );
        let text = status_text(&status).unwrap();
        assert!(text.contains("alice"));
        assert!(text.contains("admin.wipe"));
    }

    #[test]
    fn invalid_input_error_reaches_the_caller() {
        // srs FR-ERR-002/FR-ERR-006: a caller-fixable failure must carry enough
        // detail for the caller to correct the call. Collapsing it to the generic
        // string leaves an agent unable to tell a bad argument from a crash.
        let err = ModuleError::new(
            ErrorCode::GeneralInvalidInput,
            "Parameters '1' and 'l' cannot be used together",
        );
        let status = error_to_status(&err, false);
        assert_eq!(status.state, TaskState::Failed);
        let text = status_text(&status).unwrap();
        assert!(
            text.contains("'1' and 'l' cannot be used together"),
            "{text}"
        );
    }

    #[test]
    fn schema_validation_error_names_the_field() {
        let err = ModuleError::new(ErrorCode::SchemaValidationError, "width: must be integer");
        let text = status_text(&error_to_status(&err, false)).unwrap();
        assert!(text.contains("width"), "{text}");
    }

    #[test]
    fn ai_guidance_is_appended_for_caller_fixable_errors() {
        // ai_guidance exists to tell an agent what to do next; the A2A caller
        // sees only this status message, so it is appended there.
        let err = ModuleError::new(ErrorCode::GeneralInvalidInput, "bad flag combination")
            .with_ai_guidance("send either -1 or -l, not both");
        let text = status_text(&error_to_status(&err, false)).unwrap();
        assert!(text.contains("send either -1 or -l, not both"), "{text}");
    }

    #[test]
    fn ai_guidance_is_withheld_for_internal_errors() {
        // The fixed per-class strings must stay fixed: guidance on an internal
        // error would both widen the message and risk leaking internal detail.
        //
        // `DEPENDENCY_NOT_FOUND` is the case that actually discriminates.
        // `user_fixable_for_code` marks it `Some(true)` while `ErrorMapper`
        // maps it through the catch-all to "Internal server error", so the
        // previous `user_fixable`-based gate appended the guidance verbatim —
        // internal dependency-graph detail (module ids, versions, env-var
        // names, hostnames), none of which `sanitize_message` strips.
        let err = ModuleError::new(ErrorCode::DependencyNotFound, "boom").with_ai_guidance(
            "module 'billing.charge' requires 'vault.secrets' >= 2.1; set VAULT_ADDR",
        );
        assert_eq!(err.user_fixable, Some(true), "precondition for this test");
        assert_eq!(
            status_text(&error_to_status(&err, false)).as_deref(),
            Some("Internal server error")
        );

        // And the original case, where the default `user_fixable` happened to
        // agree, still holds.
        let internal = ModuleError::new(ErrorCode::GeneralInternalError, "boom")
            .with_ai_guidance("inspect /var/log/secret.log");
        assert_eq!(
            status_text(&error_to_status(&internal, false)).as_deref(),
            Some("Internal server error")
        );
    }

    #[test]
    fn ai_guidance_is_withheld_when_a_module_declares_a_masked_error_user_fixable() {
        // `user_fixable` is author-settable, so gating on it let any module
        // widen a fixed per-class string. An ACL denial must stay exactly
        // "Access denied" (srs FR-ERR-003) whatever the module claims — the
        // guidance here names the very module the refusal must not disclose.
        let err = ModuleError::new(ErrorCode::ACLDenied, "caller alice denied admin.wipe")
            .with_user_fixable(true)
            .with_ai_guidance("ask an admin to grant you the 'admin.wipe' role");
        assert_eq!(
            status_text(&error_to_status(&err, false)).as_deref(),
            Some("Access denied")
        );
    }

    #[test]
    fn timeout_error_keeps_specific_message() {
        // The specific arms are unchanged: timeout still maps to its own message.
        let err = ModuleError::new(ErrorCode::ModuleTimeout, "ignored");
        let status = error_to_status(&err, false);
        assert_eq!(status.state, TaskState::Failed);
        assert_eq!(status_text(&status).as_deref(), Some("Execution timed out"));
    }
}

//! A2A 1.0 JSON-RPC + SSE request handlers.
//!
//! A single `POST /` endpoint dispatches the A2A JSON-RPC methods. `message/send`
//! runs a task to completion and returns the final `Task`; `message/stream`
//! returns an SSE stream of A2A 1.0 events (`statusUpdate` / `artifactUpdate`);
//! `tasks/get` / `tasks/cancel` / `tasks/list` manage task state. Per-task
//! `CancelToken`s back `tasks/cancel` (cooperative apcore cancellation).

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use apcore::cancel::CancelToken;
use apcore::context::Identity;
use apcore::errors::{ErrorCode, ModuleError};
use axum::{
    extract::{FromRequestParts, State},
    http::request::Parts,
    response::sse::{Event, KeepAlive, Sse},
    response::{Html, IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;
use uuid::Uuid;

use crate::adapters::errors::{sanitize_message, ErrorMapper};
use crate::server::executor::ApCoreAgentExecutor;
use crate::storage::TaskStore;
use crate::types::{
    Artifact, Message, StreamEvent, Task, TaskArtifactUpdateEvent, TaskState, TaskStatus,
    TaskStatusUpdateEvent,
};

/// Shared application state for the A2A server routes.
#[derive(Clone)]
pub struct AppState {
    pub executor: Arc<ApCoreAgentExecutor>,
    pub task_store: Arc<dyn TaskStore>,
    /// Known skill (module) ids from the registry, for request-time validation.
    pub skill_ids: Arc<HashSet<String>>,
    /// Per-skill input schema, so an inbound `TextPart` can be parsed against
    /// the schema the module actually declares.
    pub input_schemas: Arc<HashMap<String, Value>>,
    pub agent_card: Arc<Value>,
    /// Agent card enriched with per-skill `_inputSchemas` for the Explorer UI.
    pub explorer_card: Arc<Value>,
    pub cancel_tokens: Arc<Mutex<HashMap<String, CancelToken>>>,
    /// Per-task push-notification webhook configs (`tasks/pushNotificationConfig/*`).
    pub push_configs: Arc<Mutex<HashMap<String, Value>>>,
    /// Owner (authenticated principal) of each task, keyed by task id. Scopes
    /// `tasks/list` / `tasks/get` / `tasks/cancel` so one caller cannot read or
    /// cancel another's tasks.
    pub task_owners: Arc<Mutex<HashMap<String, String>>>,
    pub http: reqwest::Client,
}

/// Owner key for a request's authenticated principal.
///
/// Mirrors the upstream A2A stores, which scope task storage by an owner
/// resolved from the call context (`a2a-python`'s `OwnerResolver`, whose
/// default is `context.user.user_name`; `a2a-js`'s per-context bucket). When no
/// authenticator is configured every request resolves to the same empty owner,
/// exactly as upstream's `UnauthenticatedUser.user_name` does — a single-tenant
/// deployment keeps its current behaviour, and configuring auth is what turns
/// scoping on.
fn owner_key(identity: Option<&Identity>) -> String {
    identity.map(|i| i.id().to_string()).unwrap_or_default()
}

/// Embedded Explorer UI (served at the explorer prefix).
const EXPLORER_HTML: &str = include_str!("../explorer/index.html");

/// Serve the Explorer single-page UI.
pub async fn explorer_html() -> Html<&'static str> {
    Html(EXPLORER_HTML)
}

/// Serve the Explorer's agent card (with `_inputSchemas`).
pub async fn explorer_card(State(state): State<AppState>) -> Json<Value> {
    Json((*state.explorer_card).clone())
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

const CODE_METHOD_NOT_FOUND: i32 = -32601;
const CODE_INVALID_PARAMS: i32 = -32602;
const CODE_TASK_NOT_FOUND: i32 = -32001;
const CODE_TASK_NOT_CANCELABLE: i32 = -32002;

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
fn error_to_status(err: &ModuleError) -> TaskStatus {
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
        _ => TaskStatus::with_message(TaskState::Failed, Message::agent_text(failure_text(err))),
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
/// For those caller-fixable classes the error's `ai_guidance` is appended when
/// apcore supplied one: it exists to tell an agent what to do next, and an A2A
/// caller sees only this status message. It is withheld for every other class,
/// where the message is a fixed per-class string that must stay fixed.
fn failure_text(err: &ModuleError) -> String {
    let message = ErrorMapper::to_jsonrpc_error(err).message;
    match (err.user_fixable, err.ai_guidance.as_deref()) {
        (Some(true), Some(guidance)) if !guidance.trim().is_empty() => {
            format!("{message} ({})", sanitize_message(guidance))
        }
        _ => message,
    }
}

/// JSON-RPC dispatch entry point (`POST /`).
pub async fn jsonrpc_handler(
    State(state): State<AppState>,
    AuthIdentity(identity): AuthIdentity,
    Json(req): Json<Value>,
) -> Response {
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let owner = owner_key(identity.as_ref());

    match method {
        "message/send" => match handle_send(&state, identity, &owner, &params).await {
            Ok(task) => Json(rpc_result(&id, task)).into_response(),
            Err((code, msg)) => Json(rpc_error(&id, code, &msg)).into_response(),
        },
        "message/stream" => handle_stream(&state, identity, &owner, &params, id).await,
        "tasks/get" => match handle_get(&state, &owner, &params).await {
            Ok(task) => Json(rpc_result(&id, task)).into_response(),
            Err((code, msg)) => Json(rpc_error(&id, code, &msg)).into_response(),
        },
        "tasks/cancel" => match handle_cancel(&state, &owner, &params).await {
            Ok(task) => Json(rpc_result(&id, task)).into_response(),
            Err((code, msg)) => Json(rpc_error(&id, code, &msg)).into_response(),
        },
        "tasks/list" => {
            let tasks = handle_list(&state, &owner).await;
            Json(rpc_result(&id, json!({ "tasks": tasks }))).into_response()
        }
        "tasks/pushNotificationConfig/set" => match handle_push_set(&state, &params) {
            Ok(v) => Json(rpc_result(&id, v)).into_response(),
            Err((code, msg)) => Json(rpc_error(&id, code, &msg)).into_response(),
        },
        "tasks/pushNotificationConfig/get" => match handle_push_get(&state, &params) {
            Ok(v) => Json(rpc_result(&id, v)).into_response(),
            Err((code, msg)) => Json(rpc_error(&id, code, &msg)).into_response(),
        },
        "tasks/pushNotificationConfig/delete" => match handle_push_delete(&state, &params) {
            Ok(v) => Json(rpc_result(&id, v)).into_response(),
            Err((code, msg)) => Json(rpc_error(&id, code, &msg)).into_response(),
        },
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
    let message: Message = params
        .get("message")
        .cloned()
        .and_then(|m| serde_json::from_value(m).ok())
        .ok_or(ParseFailure::Rpc(
            CODE_INVALID_PARAMS,
            "Missing or invalid parameter: message".to_string(),
        ))?;

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
    owner: &str,
    message: impl Into<String>,
) -> Value {
    register_owner(state, task_id, owner);
    let status = TaskStatus::with_message(TaskState::Failed, Message::agent_text(message));
    let task = Task {
        id: task_id.to_string(),
        context_id: context_id.to_string(),
        status: status.clone(),
        artifacts: vec![],
        history: vec![],
    };
    let task_json = serde_json::to_value(&task).unwrap();
    let _ = state.task_store.save(task_id, task_json.clone()).await;
    notify_push(
        state,
        task_id,
        &StreamEvent::StatusUpdate(TaskStatusUpdateEvent {
            task_id: task_id.to_string(),
            context_id: context_id.to_string(),
            status,
        }),
    );
    task_json
}

/// Emit a task-level failure as an SSE stream (SUBMITTED then terminal FAILED),
/// mirroring how a normal stream surfaces task state. Used when a streamed request
/// has a missing skillId or unconvertible parts (Python/TS parity).
async fn failed_task_stream(
    state: &AppState,
    task_id: String,
    context_id: String,
    owner: &str,
    message: String,
) -> Response {
    register_owner(state, &task_id, owner);
    let status = TaskStatus::with_message(TaskState::Failed, Message::agent_text(message));
    let task = Task {
        id: task_id.clone(),
        context_id: context_id.clone(),
        status: status.clone(),
        artifacts: vec![],
        history: vec![],
    };
    let _ = state
        .task_store
        .save(&task_id, serde_json::to_value(&task).unwrap())
        .await;
    let terminal = StreamEvent::StatusUpdate(TaskStatusUpdateEvent {
        task_id: task_id.clone(),
        context_id: context_id.clone(),
        status,
    });
    notify_push(state, &task_id, &terminal);
    let events = vec![
        StreamEvent::StatusUpdate(TaskStatusUpdateEvent {
            task_id: task_id.clone(),
            context_id: context_id.clone(),
            status: TaskStatus::new(TaskState::Submitted),
        }),
        terminal,
    ];
    let event_stream = tokio_stream::iter(events)
        .map(|ev| Ok::<Event, Infallible>(Event::default().json_data(&ev).unwrap_or_default()));
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
    owner: &str,
    params: &Value,
) -> Result<Value, (i32, String)> {
    let (skill_id, task_id, context_id, inputs) = match parse_request(state, params) {
        Ok(v) => v,
        Err(ParseFailure::Rpc(code, msg)) => return Err((code, msg)),
        Err(ParseFailure::Task {
            task_id,
            context_id,
            message,
        }) => return Ok(fail_task(state, &task_id, &context_id, owner, message).await),
    };

    // Reject unknown skills with a FAILED task (Python/TS parity).
    if !state.skill_ids.contains(&skill_id) {
        return Ok(fail_task(
            state,
            &task_id,
            &context_id,
            owner,
            format!("Skill not found: {skill_id}"),
        )
        .await);
    }

    register_owner(state, &task_id, owner);
    let mut task = submitted_task(&task_id, &context_id);

    let cancel = CancelToken::new();
    register_cancel(state, &task_id, cancel.clone());
    // Unregister the cancel token on every exit path (success, error, panic);
    // a Drop guard mirrors handle_stream and avoids leaking the token.
    let _cancel_guard = CancelGuard {
        state: state.clone(),
        task_id: task_id.clone(),
    };

    let _ = state
        .task_store
        .save(&task_id, serde_json::to_value(&task).unwrap())
        .await;

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
            task.status = error_to_status(&err);
        }
    }
    let task_json = serde_json::to_value(&task).unwrap();
    let _ = state.task_store.save(&task_id, task_json.clone()).await;

    // Deliver the terminal status to any registered webhook.
    notify_push(
        state,
        &task_id,
        &StreamEvent::StatusUpdate(TaskStatusUpdateEvent {
            task_id: task_id.clone(),
            context_id: context_id.clone(),
            status: task.status.clone(),
        }),
    );
    Ok(task_json)
}

/// `message/stream` — SSE stream of A2A 1.0 events.
async fn handle_stream(
    state: &AppState,
    identity: Option<Identity>,
    owner: &str,
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
        }) => return failed_task_stream(state, task_id, context_id, owner, message).await,
    };

    register_owner(state, &task_id, owner);
    let cancel = CancelToken::new();
    register_cancel(state, &task_id, cancel.clone());

    let (tx, rx) = mpsc::channel::<StreamEvent>(16);
    let state2 = state.clone();
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
            let _ = state2
                .task_store
                .save(&task_id, serde_json::to_value(&task).unwrap())
                .await;
            let terminal_event = StreamEvent::StatusUpdate(TaskStatusUpdateEvent {
                task_id: task_id.clone(),
                context_id: context_id.clone(),
                status,
            });
            notify_push(&state2, &task_id, &terminal_event);
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
            Some(e) => error_to_status(e),
        };
        // Persist terminal task.
        let task = Task {
            id: task_id.clone(),
            context_id: context_id.clone(),
            status: final_status.clone(),
            artifacts: vec![],
            history: vec![],
        };
        let _ = state2
            .task_store
            .save(&task_id, serde_json::to_value(&task).unwrap())
            .await;
        let terminal_event = StreamEvent::StatusUpdate(TaskStatusUpdateEvent {
            task_id: task_id.clone(),
            context_id: context_id.clone(),
            status: final_status,
        });
        // Deliver the terminal status to any registered webhook.
        notify_push(&state2, &task_id, &terminal_event);
        send(terminal_event).await;
        // `_cancel_guard` unregisters the cancel token on drop (all exit paths).
    });

    let event_stream = ReceiverStream::new(rx)
        .map(|ev| Ok::<Event, Infallible>(Event::default().json_data(&ev).unwrap_or_default()));

    Sse::new(event_stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// `tasks/get` — return a stored task by id, if the caller owns it.
async fn handle_get(state: &AppState, owner: &str, params: &Value) -> Result<Value, (i32, String)> {
    let task_id = params.get("id").and_then(Value::as_str).ok_or((
        CODE_INVALID_PARAMS,
        "Missing required parameter: id".to_string(),
    ))?;
    if !is_owned_by(state, task_id, owner) {
        // Masked as "not found" rather than "forbidden", matching how ACL
        // denials are reported (srs FR-ERR-003): a caller must not learn that
        // another principal's task id exists.
        return Err((CODE_TASK_NOT_FOUND, "Task not found".to_string()));
    }
    match state.task_store.get(task_id).await {
        Ok(Some(task)) => Ok(task),
        _ => Err((CODE_TASK_NOT_FOUND, "Task not found".to_string())),
    }
}

/// `tasks/list` — return the calling principal's tasks.
///
/// Unscoped, this returned every caller's tasks including the full stdout of
/// tasks other callers submitted. The upstream A2A stores scope by owner
/// (`a2a-python`'s `OwnerResolver`, `a2a-js`'s per-context bucket); the spec
/// repo is silent on it, so this follows upstream.
async fn handle_list(state: &AppState, owner: &str) -> Vec<Value> {
    let owned: HashSet<String> = state
        .task_owners
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, task_owner)| task_owner.as_str() == owner)
        .map(|(task_id, _)| task_id.clone())
        .collect();

    state
        .task_store
        .list()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|task| {
            task.get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| owned.contains(id))
        })
        .collect()
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
    owner: &str,
    params: &Value,
) -> Result<Value, (i32, String)> {
    let task_id = params.get("id").and_then(Value::as_str).ok_or((
        CODE_INVALID_PARAMS,
        "Missing required parameter: id".to_string(),
    ))?;

    // Another principal's task is reported as missing, not as forbidden.
    if !is_owned_by(state, task_id, owner) {
        return Err((CODE_TASK_NOT_FOUND, "Task not found".to_string()));
    }

    let stored = state
        .task_store
        .get(task_id)
        .await
        .ok()
        .flatten()
        .ok_or((CODE_TASK_NOT_FOUND, "Task not found".to_string()))?;

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
    let _ = state.task_store.save(task_id, task_json.clone()).await;

    // Deliver the terminal CANCELED status to any registered webhook.
    notify_push(
        state,
        task_id,
        &StreamEvent::StatusUpdate(TaskStatusUpdateEvent {
            task_id: task_id.to_string(),
            context_id,
            status: task.status.clone(),
        }),
    );
    Ok(task_json)
}

/// `tasks/pushNotificationConfig/set` — register a webhook for a task.
fn handle_push_set(state: &AppState, params: &Value) -> Result<Value, (i32, String)> {
    let task_id = params.get("id").and_then(Value::as_str).ok_or((
        CODE_INVALID_PARAMS,
        "Missing required parameter: id".to_string(),
    ))?;
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
        .push_configs
        .lock()
        .unwrap()
        .insert(task_id.to_string(), cfg.clone());
    Ok(json!({ "taskId": task_id, "pushNotificationConfig": cfg }))
}

/// `tasks/pushNotificationConfig/get` — return a task's webhook config.
fn handle_push_get(state: &AppState, params: &Value) -> Result<Value, (i32, String)> {
    let task_id = params.get("id").and_then(Value::as_str).ok_or((
        CODE_INVALID_PARAMS,
        "Missing required parameter: id".to_string(),
    ))?;
    match state.push_configs.lock().unwrap().get(task_id) {
        Some(cfg) => Ok(json!({ "taskId": task_id, "pushNotificationConfig": cfg })),
        None => Err((
            CODE_TASK_NOT_FOUND,
            "Push notification config not found".to_string(),
        )),
    }
}

/// `tasks/pushNotificationConfig/delete` — remove a task's webhook config.
fn handle_push_delete(state: &AppState, params: &Value) -> Result<Value, (i32, String)> {
    let task_id = params.get("id").and_then(Value::as_str).ok_or((
        CODE_INVALID_PARAMS,
        "Missing required parameter: id".to_string(),
    ))?;
    state.push_configs.lock().unwrap().remove(task_id);
    Ok(Value::Null)
}

/// Deliver an event to a task's registered webhook (if any), retrying with
/// exponential backoff. Fire-and-forget: spawned so it never blocks the caller.
fn notify_push(state: &AppState, task_id: &str, event: &StreamEvent) {
    let cfg = match state.push_configs.lock().unwrap().get(task_id).cloned() {
        Some(c) => c,
        None => return,
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

/// Record which principal a task belongs to.
fn register_owner(state: &AppState, task_id: &str, owner: &str) {
    state
        .task_owners
        .lock()
        .unwrap()
        .insert(task_id.to_string(), owner.to_string());
}

/// Whether `owner` may read or cancel `task_id`.
///
/// Fails closed: a task with no recorded owner is visible to nobody. The
/// ownership map lives in process memory, so a custom [`TaskStore`] that
/// outlives the process would need to carry the owner itself — that is a
/// `TaskStore` trait change and is deliberately not made here.
fn is_owned_by(state: &AppState, task_id: &str, owner: &str) -> bool {
    state
        .task_owners
        .lock()
        .unwrap()
        .get(task_id)
        .is_some_and(|task_owner| task_owner == owner)
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
        let status = error_to_status(&err);
        assert_eq!(status.state, TaskState::Failed);
        assert_eq!(
            status_text(&status).as_deref(),
            Some("Internal server error")
        );
    }

    #[test]
    fn acl_denied_stays_masked_as_task_not_found() {
        // srs FR-ERR-003: an ACL denial must not disclose the caller, the target
        // module, or that the denial happened at all.
        let err = ModuleError::new(
            ErrorCode::ACLDenied,
            "caller alice denied module admin.wipe",
        );
        let status = error_to_status(&err);
        assert_eq!(status.state, TaskState::Failed);
        let text = status_text(&status).unwrap();
        assert_eq!(text, "Task not found");
        assert!(!text.contains("alice"));
        assert!(!text.contains("admin.wipe"));
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
        let status = error_to_status(&err);
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
        let text = status_text(&error_to_status(&err)).unwrap();
        assert!(text.contains("width"), "{text}");
    }

    #[test]
    fn ai_guidance_is_appended_for_caller_fixable_errors() {
        // ai_guidance exists to tell an agent what to do next; the A2A caller
        // sees only this status message, so it is appended there.
        let err = ModuleError::new(ErrorCode::GeneralInvalidInput, "bad flag combination")
            .with_ai_guidance("send either -1 or -l, not both");
        let text = status_text(&error_to_status(&err)).unwrap();
        assert!(text.contains("send either -1 or -l, not both"), "{text}");
    }

    #[test]
    fn ai_guidance_is_withheld_for_internal_errors() {
        // The fixed per-class strings must stay fixed: guidance on an internal
        // error would both widen the message and risk leaking internal detail.
        let err = ModuleError::new(ErrorCode::GeneralInternalError, "boom")
            .with_ai_guidance("inspect /var/log/secret.log");
        assert_eq!(
            status_text(&error_to_status(&err)).as_deref(),
            Some("Internal server error")
        );
    }

    #[test]
    fn timeout_error_keeps_specific_message() {
        // The specific arms are unchanged: timeout still maps to its own message.
        let err = ModuleError::new(ErrorCode::ModuleTimeout, "ignored");
        let status = error_to_status(&err);
        assert_eq!(status.state, TaskState::Failed);
        assert_eq!(status_text(&status).as_deref(), Some("Execution timed out"));
    }
}

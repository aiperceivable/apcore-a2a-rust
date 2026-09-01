//! End-to-end HTTP integration tests for the A2A 1.0 server.
//!
//! Drives the assembled axum `Router` via `tower::ServiceExt::oneshot` — no
//! network sockets — exercising JSON-RPC dispatch, the Agent Card endpoint, and
//! task execution against a real apcore `Registry` holding an echo module.

use std::sync::Arc;

use apcore::context::Context;
use apcore::errors::{ErrorCode, ModuleError};
use apcore::module::Module;
use apcore::registry::registry::Registry;
use apcore_a2a::{build_app, APCoreA2AConfig, BackendSource};
use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

/// Minimal echo module: returns its inputs unchanged.
struct EchoModule;

#[async_trait]
impl Module for EchoModule {
    fn input_schema(&self) -> Value {
        json!({ "type": "object" })
    }
    fn output_schema(&self) -> Value {
        json!({ "type": "object" })
    }
    fn description(&self) -> &str {
        "Echoes its inputs"
    }
    async fn execute(&self, inputs: Value, _ctx: &Context<Value>) -> Result<Value, ModuleError> {
        Ok(inputs)
    }
}

/// Module that refuses its input the way apcore's own input guards do.
struct GuardModule;

#[async_trait]
impl Module for GuardModule {
    fn input_schema(&self) -> Value {
        json!({ "type": "object" })
    }
    fn output_schema(&self) -> Value {
        json!({ "type": "object" })
    }
    fn description(&self) -> &str {
        "Always refuses its input"
    }
    async fn execute(&self, _inputs: Value, _ctx: &Context<Value>) -> Result<Value, ModuleError> {
        Err(ModuleError::new(
            ErrorCode::GeneralInvalidInput,
            "Parameters '1' and 'l' cannot be used together",
        )
        .with_ai_guidance("send one or the other"))
    }
}

/// Module that fails with an internal error carrying sensitive text.
struct InternalFailModule;

#[async_trait]
impl Module for InternalFailModule {
    fn input_schema(&self) -> Value {
        json!({ "type": "object" })
    }
    fn output_schema(&self) -> Value {
        json!({ "type": "object" })
    }
    fn description(&self) -> &str {
        "Always fails internally"
    }
    async fn execute(&self, _inputs: Value, _ctx: &Context<Value>) -> Result<Value, ModuleError> {
        Err(ModuleError::new(
            ErrorCode::GeneralInternalError,
            "Database password: P@ssw0rd123",
        ))
    }
}

/// Module that stays in flight long enough for a concurrent `tasks/cancel`.
struct SlowModule;

#[async_trait]
impl Module for SlowModule {
    fn input_schema(&self) -> Value {
        json!({ "type": "object" })
    }
    fn output_schema(&self) -> Value {
        json!({ "type": "object" })
    }
    fn description(&self) -> &str {
        "Sleeps before returning"
    }
    async fn execute(&self, inputs: Value, _ctx: &Context<Value>) -> Result<Value, ModuleError> {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        Ok(inputs)
    }
}

/// A registry with the `test.echo`, `test.guard`, `test.internal` and
/// `test.slow` modules.
fn echo_registry() -> Arc<Registry> {
    let registry = Registry::new();
    registry
        .register_module("test.echo", Box::new(EchoModule))
        .expect("register echo module");
    registry
        .register_module("test.guard", Box::new(GuardModule))
        .expect("register guard module");
    registry
        .register_module("test.internal", Box::new(InternalFailModule))
        .expect("register internal-fail module");
    registry
        .register_module("test.slow", Box::new(SlowModule))
        .expect("register slow module");
    Arc::new(registry)
}

async fn build() -> axum::Router {
    let (router, _card) = build_app(
        BackendSource::Registry(echo_registry()),
        APCoreA2AConfig::default(),
    )
    .await
    .expect("build app");
    router
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn post_rpc(router: axum::Router, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    (status, body_json(resp).await)
}

/// POST a `message/stream` request and collect the raw SSE `data:` frames.
async fn collect_sse_frames(router: axum::Router, body: Value) -> Vec<Value> {
    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    text.lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
        .collect()
}

/// Collect a stream's A2A events, unwrapping each frame's JSON-RPC envelope.
async fn collect_sse(router: axum::Router, body: Value) -> Vec<Value> {
    collect_sse_frames(router, body)
        .await
        .into_iter()
        .map(|frame| frame["result"].clone())
        .collect()
}

#[tokio::test]
async fn agent_card_is_a2a_1_0_shape() {
    let req = Request::builder()
        .uri("/.well-known/agent-card.json")
        .body(Body::empty())
        .unwrap();
    let resp = build().await.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let card = body_json(resp).await;

    // A2A 1.0: supportedInterfaces (not top-level url), 1.0 capabilities.
    assert_eq!(
        card["supportedInterfaces"][0]["protocolVersion"],
        json!("1.0")
    );
    assert_eq!(
        card["supportedInterfaces"][0]["protocolBinding"],
        json!("JSONRPC")
    );
    assert!(card["capabilities"]["extensions"].is_array());
    assert!(card["capabilities"].get("stateTransitionHistory").is_none());
    assert!(card.get("url").is_none());
    // Skill present with 1.0 securityRequirements.
    assert_eq!(card["skills"][0]["id"], json!("test.echo"));
    assert!(card["skills"][0]["securityRequirements"].is_array());
}

#[tokio::test]
async fn agent_card_version_tracks_crate_version() {
    // The card's default version comes from `apcore_a2a::VERSION`, which is
    // derived from Cargo.toml. A hand-maintained literal silently drifts and
    // makes the served card advertise a release the binary is not.
    assert_eq!(apcore_a2a::VERSION, env!("CARGO_PKG_VERSION"));

    let req = Request::builder()
        .uri("/.well-known/agent-card.json")
        .body(Body::empty())
        .unwrap();
    let resp = build().await.oneshot(req).await.unwrap();
    let card = body_json(resp).await;
    assert_eq!(card["version"], json!(env!("CARGO_PKG_VERSION")));
}

#[tokio::test]
async fn agent_card_advertises_only_acl_allowed_skills() {
    // The ACL gated the call but not the advertisement, so a deny-all-but-one
    // ACL still listed every module.
    use apcore::acl::{ACLRule, ACL};
    use apcore::config::Config;
    use apcore::executor::Executor;

    let acl = ACL::new(
        vec![ACLRule {
            callers: vec!["*".into()],
            targets: vec!["test.echo".into()],
            effect: "allow".into(),
            approval: None,
            description: None,
            conditions: None,
        }],
        "deny",
        None,
    );
    let mut executor = Executor::new(echo_registry(), Config::default());
    executor.set_acl(acl);

    let (router, _card) = build_app(
        BackendSource::Executor(Arc::new(executor)),
        APCoreA2AConfig::default(),
    )
    .await
    .expect("build app");

    let req = Request::builder()
        .uri("/.well-known/agent-card.json")
        .body(Body::empty())
        .unwrap();
    let card = body_json(router.oneshot(req).await.unwrap()).await;

    let ids: Vec<&str> = card["skills"]
        .as_array()
        .expect("skills array")
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    assert_eq!(
        ids,
        vec!["test.echo"],
        "only the ACL-allowed skill may be advertised"
    );
}

#[tokio::test]
async fn agent_card_is_unfiltered_when_no_acl_is_configured() {
    let req = Request::builder()
        .uri("/.well-known/agent-card.json")
        .body(Body::empty())
        .unwrap();
    let card = body_json(build().await.oneshot(req).await.unwrap()).await;
    assert_eq!(card["skills"].as_array().map(Vec::len), Some(4));
}

#[tokio::test]
async fn agent_json_alias_served() {
    let req = Request::builder()
        .uri("/.well-known/agent.json")
        .body(Body::empty())
        .unwrap();
    let resp = build().await.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn health_ok() {
    let req = Request::builder()
        .uri("/health")
        .body(Body::empty())
        .unwrap();
    let resp = build().await.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["status"], json!("healthy"));
}

#[tokio::test]
async fn message_send_executes_and_completes() {
    let (status, resp) = post_rpc(
        build().await,
        json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "message/send",
            "params": {
                "message": {
                    "messageId": "m1",
                    "role": "ROLE_USER",
                    "parts": [{ "data": { "hello": "world" } }]
                },
                "metadata": { "skillId": "test.echo" }
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let task = &resp["result"];
    assert_eq!(task["status"]["state"], json!("TASK_STATE_COMPLETED"));
    // Echo module returns its inputs as a data Part.
    assert_eq!(
        task["artifacts"][0]["parts"][0]["data"],
        json!({ "hello": "world" })
    );
}

/// Send a `message/send` for `skill_id` with an empty data Part.
async fn send_to(router: axum::Router, skill_id: &str) -> Value {
    let (_status, resp) = post_rpc(
        router,
        json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "message/send",
            "params": {
                "message": { "messageId": "m1", "role": "ROLE_USER", "parts": [{ "data": {} }] },
                "metadata": { "skillId": skill_id }
            }
        }),
    )
    .await;
    resp
}

/// Poll `ListTasks` until the task a concurrent `message/send` persisted as
/// SUBMITTED appears, and return its (server-generated) id.
async fn await_in_flight_task_id(router: axum::Router) -> String {
    for _ in 0..100 {
        let (_status, listed) = post_rpc(
            router.clone(),
            json!({ "jsonrpc": "2.0", "id": "list", "method": "ListTasks", "params": {} }),
        )
        .await;
        if let Some(id) = listed["result"]["tasks"][0]["id"].as_str() {
            return id.to_string();
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("in-flight task was never persisted");
}

/// Extract a task status message's text.
fn status_text(task: &Value) -> String {
    task["status"]["message"]["parts"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[tokio::test]
async fn failed_task_surfaces_caller_fixable_error_detail() {
    // A guard refusal must reach the caller intact: an agent that cannot tell a
    // bad argument from a crash cannot self-correct, which is the whole point of
    // the guard (srs FR-ERR-002 "correct their input without guessing").
    let resp = send_to(build().await, "test.guard").await;
    let task = &resp["result"];
    assert_eq!(task["status"]["state"], json!("TASK_STATE_FAILED"));
    let text = status_text(task);
    assert!(
        text.contains("'1' and 'l' cannot be used together"),
        "guard text missing from {text:?}"
    );
    assert!(
        text.contains("send one or the other"),
        "ai_guidance missing from {text:?}"
    );
}

#[tokio::test]
async fn failed_task_keeps_internal_errors_opaque() {
    // The counterpart guarantee: an internal error still collapses to the fixed
    // string, so no internal detail rides out on the status message.
    let resp = send_to(build().await, "test.internal").await;
    let task = &resp["result"];
    assert_eq!(task["status"]["state"], json!("TASK_STATE_FAILED"));
    let text = status_text(task);
    assert_eq!(text, "Internal server error");
    assert!(!text.contains("P@ssw0rd123"));
}

#[tokio::test]
async fn sse_frames_are_jsonrpc_responses_carrying_the_event() {
    // Each `data:` used to be a bare `{"statusUpdate":{...}}`, which an
    // off-the-shelf a2a-python / a2a-js client cannot parse.
    let frames = collect_sse_frames(
        build().await,
        json!({
            "jsonrpc": "2.0",
            "id": "req-7",
            "method": "message/stream",
            "params": {
                "message": { "messageId": "m1", "role": "ROLE_USER", "parts": [{ "data": { "x": 1 } }] },
                "metadata": { "skillId": "test.echo" }
            }
        }),
    )
    .await;

    assert!(!frames.is_empty());
    for frame in &frames {
        assert_eq!(frame["jsonrpc"], json!("2.0"));
        assert_eq!(frame["id"], json!("req-7"), "the request id is echoed");
        assert!(frame["result"].is_object(), "event lives under result");
    }
    // A2A 1.0 discriminates by the oneof wrapper key; `kind` and `final` were
    // removed and must not come back.
    let first = &frames[0]["result"];
    assert!(first.get("statusUpdate").is_some());
    assert!(first.get("kind").is_none());
    assert!(first["statusUpdate"].get("final").is_none());
}

#[tokio::test]
async fn message_stream_surfaces_caller_fixable_error_detail() {
    // The streaming path shares error_to_status, so it must classify identically.
    let events = collect_sse(
        build().await,
        json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "message/stream",
            "params": {
                "message": { "messageId": "m1", "role": "ROLE_USER", "parts": [{ "data": {} }] },
                "metadata": { "skillId": "test.guard" }
            }
        }),
    )
    .await;
    let terminal = events.last().expect("at least one event");
    let status = &terminal["statusUpdate"]["status"];
    assert_eq!(status["state"], json!("TASK_STATE_FAILED"));
    assert!(status["message"]["parts"][0]["text"]
        .as_str()
        .unwrap()
        .contains("cannot be used together"));
}

#[tokio::test]
async fn sdk_conventional_lowercase_role_is_accepted() {
    // `"role": "user"` is what an SDK-conventional client sends. It used to be
    // rejected with -32602 "Missing or invalid parameter: message", naming the
    // wrong field entirely — the actual requirement was the protobuf enum name.
    let (_status, resp) = post_rpc(
        build().await,
        json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "message/send",
            "params": {
                "message": { "messageId": "m1", "role": "user", "parts": [{ "data": { "a": 1 } }] },
                "metadata": { "skillId": "test.echo" }
            }
        }),
    )
    .await;
    assert!(resp.get("error").is_none(), "unexpected error: {resp:?}");
    assert_eq!(
        resp["result"]["status"]["state"],
        json!("TASK_STATE_COMPLETED")
    );
}

#[tokio::test]
async fn unreadable_message_names_the_field_that_failed() {
    // An unusable role must not be reported as a missing `message`.
    let (_status, resp) = post_rpc(
        build().await,
        json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "message/send",
            "params": {
                "message": { "messageId": "m1", "role": "wizard", "parts": [{ "data": {} }] },
                "metadata": { "skillId": "test.echo" }
            }
        }),
    )
    .await;
    assert_eq!(resp["error"]["code"], json!(-32602));
    let message = resp["error"]["message"].as_str().unwrap();
    // The rejected value and the accepted ones, rather than a claim that the
    // whole `message` parameter was missing.
    assert!(message.contains("wizard"), "{message}");
    assert!(message.contains("ROLE_USER"), "{message}");
    assert!(!message.contains("Missing"), "{message}");
}

#[tokio::test]
async fn text_part_is_parsed_against_the_module_input_schema() {
    // The card advertises `application/json` as an input mode, but the module's
    // schema was never passed to the part converter, so a JSON TextPart arrived
    // as a bare string and only a DataPart ever worked.
    let (status, resp) = post_rpc(
        build().await,
        json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "message/send",
            "params": {
                "message": {
                    "messageId": "m1",
                    "role": "ROLE_USER",
                    "parts": [{ "text": "{\"hello\": \"world\"}" }]
                },
                "metadata": { "skillId": "test.echo" }
            }
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let task = &resp["result"];
    assert_eq!(task["status"]["state"], json!("TASK_STATE_COMPLETED"));
    // The echo module returns its inputs, so a parsed object comes back as a
    // data Part — not the raw string.
    assert_eq!(
        task["artifacts"][0]["parts"][0]["data"],
        json!({ "hello": "world" })
    );
}

#[tokio::test]
async fn text_part_that_is_not_json_fails_the_task_with_a_readable_reason() {
    let resp = {
        let (_status, resp) = post_rpc(
            build().await,
            json!({
                "jsonrpc": "2.0",
                "id": "1",
                "method": "message/send",
                "params": {
                    "message": {
                        "messageId": "m1",
                        "role": "ROLE_USER",
                        "parts": [{ "text": "not json at all" }]
                    },
                    "metadata": { "skillId": "test.echo" }
                }
            }),
        )
        .await;
        resp
    };
    let task = &resp["result"];
    assert_eq!(task["status"]["state"], json!("TASK_STATE_FAILED"));
    assert!(
        status_text(task).contains("not valid JSON"),
        "{}",
        status_text(task)
    );
}

#[tokio::test]
async fn message_send_missing_skill_id_yields_failed_task() {
    // A-D-303: a missing metadata.skillId now produces a FAILED task (matching
    // Python/TS and the unknown-skill path), not a JSON-RPC -32602 error.
    let (status, resp) = post_rpc(
        build().await,
        json!({
            "jsonrpc": "2.0",
            "id": "2",
            "method": "message/send",
            "params": { "message": { "messageId": "m", "role": "ROLE_USER", "parts": [{ "text": "hi" }] } }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        resp.get("error").is_none(),
        "expected a task, not an error: {resp:?}"
    );
    let task = &resp["result"];
    assert_eq!(task["status"]["state"], json!("TASK_STATE_FAILED"));
    assert_eq!(
        task["status"]["message"]["parts"][0]["text"],
        json!("Missing required parameter: metadata.skillId")
    );
}

#[tokio::test]
async fn message_send_missing_message_is_invalid_params() {
    // A genuinely missing message envelope remains a protocol-level -32602 error
    // (no task context exists to fail) — distinct from missing skillId.
    let (_status, resp) = post_rpc(
        build().await,
        json!({
            "jsonrpc": "2.0",
            "id": "3",
            "method": "message/send",
            "params": { "metadata": { "skillId": "test.echo" } }
        }),
    )
    .await;
    assert_eq!(resp["error"]["code"], json!(-32602));
}

#[tokio::test]
async fn message_stream_emits_terminal_last_chunk_marker() {
    // A-D-301: a successful stream emits a final artifactUpdate with lastChunk=true
    // (art-{taskId}, empty parts) before the terminal COMPLETED status.
    let events = collect_sse(
        build().await,
        json!({
            "jsonrpc": "2.0",
            "id": "s1",
            "method": "message/stream",
            "params": {
                "message": { "messageId": "m", "role": "ROLE_USER", "parts": [{ "data": { "x": 1 } }] },
                "metadata": { "skillId": "test.echo" }
            }
        }),
    )
    .await;

    let last_chunk_idx = events
        .iter()
        .position(|e| e["artifactUpdate"]["lastChunk"] == json!(true));
    let completed_idx = events
        .iter()
        .position(|e| e["statusUpdate"]["status"]["state"] == json!("TASK_STATE_COMPLETED"));
    assert!(
        last_chunk_idx.is_some(),
        "expected a lastChunk=true artifactUpdate, got: {events:?}"
    );
    assert!(completed_idx.is_some(), "expected a COMPLETED statusUpdate");
    assert!(
        last_chunk_idx < completed_idx,
        "lastChunk marker must precede COMPLETED: {events:?}"
    );
    let marker = &events[last_chunk_idx.unwrap()]["artifactUpdate"];
    assert!(marker["artifact"]["artifactId"]
        .as_str()
        .unwrap()
        .starts_with("art-"));
    assert_eq!(marker["artifact"]["parts"], json!([]));
}

#[tokio::test]
async fn message_stream_missing_skill_id_yields_failed_task_events() {
    // A-D-303 (streaming): a streamed request with no skillId emits SUBMITTED then
    // a terminal FAILED status over SSE — not a JSON-RPC error.
    let events = collect_sse(
        build().await,
        json!({
            "jsonrpc": "2.0",
            "id": "s2",
            "method": "message/stream",
            "params": { "message": { "messageId": "m", "role": "ROLE_USER", "parts": [{ "text": "hi" }] } }
        }),
    )
    .await;
    let states: Vec<&str> = events
        .iter()
        .filter_map(|e| e["statusUpdate"]["status"]["state"].as_str())
        .collect();
    assert_eq!(states, vec!["TASK_STATE_SUBMITTED", "TASK_STATE_FAILED"]);
}

#[tokio::test]
async fn unknown_method_is_method_not_found() {
    let (_status, resp) = post_rpc(
        build().await,
        json!({ "jsonrpc": "2.0", "id": "3", "method": "does/not/exist", "params": {} }),
    )
    .await;
    assert_eq!(resp["error"]["code"], json!(-32601));
}

/// POST a raw body (not necessarily valid JSON) to the JSON-RPC endpoint.
async fn post_raw(router: axum::Router, body: &str) -> (StatusCode, Vec<u8>) {
    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, bytes.to_vec())
}

#[tokio::test]
async fn malformed_json_is_a_parse_error_not_a_plain_text_400() {
    // Unparseable input used to fall to axum's `Json` rejection: HTTP 400 with a
    // text/plain body, which no JSON-RPC client can read.
    let (status, body) = post_raw(build().await, "{not json").await;
    assert_eq!(status, StatusCode::OK);
    let resp: Value = serde_json::from_slice(&body).expect("a JSON-RPC body");
    assert_eq!(resp["jsonrpc"], json!("2.0"));
    assert_eq!(resp["error"]["code"], json!(-32700));
    assert_eq!(resp["id"], Value::Null);
}

#[tokio::test]
async fn wrong_jsonrpc_version_is_invalid_request() {
    let (_status, resp) = post_rpc(
        build().await,
        json!({ "jsonrpc": "1.0", "id": "1", "method": "tasks/list", "params": {} }),
    )
    .await;
    assert_eq!(resp["error"]["code"], json!(-32600));
    // The id is echoed, so a client can correlate the rejection.
    assert_eq!(resp["id"], json!("1"));
}

#[tokio::test]
async fn missing_jsonrpc_or_method_is_invalid_request() {
    let (_status, no_version) = post_rpc(
        build().await,
        json!({ "id": "1", "method": "tasks/list", "params": {} }),
    )
    .await;
    assert_eq!(no_version["error"]["code"], json!(-32600));

    // Previously a missing `method` reported -32601 "Method not found", which
    // describes a well-formed request naming an unknown method.
    let (_status, no_method) = post_rpc(
        build().await,
        json!({ "jsonrpc": "2.0", "id": "1", "params": {} }),
    )
    .await;
    assert_eq!(no_method["error"]["code"], json!(-32600));
}

#[tokio::test]
async fn batch_request_is_rejected_as_invalid_request() {
    // Neither upstream A2A server implements batching. a2a-python rejects it
    // explicitly with -32600; Rust used to report -32601, which claims the
    // array itself named an unknown method.
    let (_status, resp) = post_rpc(
        build().await,
        json!([{ "jsonrpc": "2.0", "id": "1", "method": "tasks/list", "params": {} }]),
    )
    .await;
    assert_eq!(resp["error"]["code"], json!(-32600));
    assert!(resp["error"]["message"]
        .as_str()
        .unwrap()
        .contains("Batch requests are not supported"));
}

#[tokio::test]
async fn notification_is_answered_with_a_null_id() {
    // Locked deliberately: strict JSON-RPC 2.0 says a notification gets no
    // response, but both upstream A2A servers answer it with `"id": null`
    // (a2a-python jsonrpc_dispatcher.py, a2a-js jsonrpc_transport_handler.ts:87
    // `id: requestId = null`), and neither has a notification code path. Rust
    // follows the transport authority rather than diverging alone.
    let (status, resp) = post_rpc(
        build().await,
        json!({ "jsonrpc": "2.0", "method": "ListTasks", "params": {} }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["id"], Value::Null);
    assert!(resp["result"]["tasks"].is_array());
}

#[tokio::test]
async fn non_json_content_type_is_unsupported_media_type() {
    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("content-type", "text/plain")
        .body(Body::from("{}"))
        .unwrap();
    let resp = build().await.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

/// Test authenticator: accepts `Authorization: Bearer good` as principal `u1`
/// and `Bearer other` as principal `u2`; rejects anything else.
struct TestAuth;

#[async_trait]
impl apcore_a2a::Authenticator for TestAuth {
    async fn authenticate(
        &self,
        headers: &std::collections::HashMap<String, String>,
    ) -> Option<apcore::context::Identity> {
        // `weak` is a principal of a *different* identity type, so an ACL
        // `identity_types` condition can tell it apart from `u1` / `u2`.
        let (principal, identity_type) = match headers.get("authorization").map(String::as_str) {
            Some("Bearer good") => ("u1", "test"),
            Some("Bearer other") => ("u2", "test"),
            Some("Bearer weak") => ("u3", "untrusted"),
            _ => return None,
        };
        Some(apcore::context::Identity::new(
            principal.into(),
            identity_type.into(),
            vec![],
            std::collections::HashMap::new(),
        ))
    }
    fn security_schemes(&self) -> Option<Value> {
        Some(json!({ "bearer": { "type": "http", "scheme": "bearer" } }))
    }
}

async fn build_with_auth() -> axum::Router {
    let (router, _card) = apcore_a2a::build_app_with_auth(
        BackendSource::Registry(echo_registry()),
        APCoreA2AConfig::default(),
        Some(Arc::new(TestAuth)),
    )
    .await
    .expect("build app with auth");
    router
}

#[tokio::test]
async fn auth_rejects_unauthenticated_jsonrpc() {
    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"jsonrpc":"2.0","id":"1","method":"ListTasks","params":{}}).to_string(),
        ))
        .unwrap();
    let resp = build_with_auth().await.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn auth_allows_valid_bearer_and_card_stays_public() {
    // Authenticated JSON-RPC passes.
    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("content-type", "application/json")
        .header("authorization", "Bearer good")
        .body(Body::from(
            json!({"jsonrpc":"2.0","id":"1","method":"ListTasks","params":{}}).to_string(),
        ))
        .unwrap();
    let resp = build_with_auth().await.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Agent card discovery is exempt from auth.
    let card_req = Request::builder()
        .uri("/.well-known/agent-card.json")
        .body(Body::empty())
        .unwrap();
    let card_resp = build_with_auth().await.oneshot(card_req).await.unwrap();
    assert_eq!(card_resp.status(), StatusCode::OK);
    // Auth configured → extendedAgentCard true + security schemes present.
    let card = body_json(card_resp).await;
    assert_eq!(card["capabilities"]["extendedAgentCard"], json!(true));
    assert!(card["securitySchemes"]["bearer"].is_object());
}

/// POST a JSON-RPC request as the principal behind `bearer`.
async fn post_rpc_as(router: axum::Router, bearer: &str, body: Value) -> Value {
    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    body_json(resp).await
}

#[tokio::test]
async fn task_reads_are_scoped_to_the_authenticated_principal() {
    // `ListTasks` returned every caller's tasks, including the full stdout of
    // tasks other callers submitted, and `tasks/get` / `tasks/cancel` accepted
    // another principal's task id.
    let app = build_with_auth().await;

    let sent = post_rpc_as(
        app.clone(),
        "good",
        json!({
            "jsonrpc": "2.0", "id": "1", "method": "message/send",
            "params": {
                "message": { "messageId": "m1", "role": "ROLE_USER", "parts": [{ "data": { "secret": "u1-only" } }] },
                "metadata": { "skillId": "test.echo" }
            }
        }),
    )
    .await;
    let task_id = sent["result"]["id"].as_str().expect("task id").to_string();

    // The owner sees it.
    let mine = post_rpc_as(
        app.clone(),
        "good",
        json!({ "jsonrpc": "2.0", "id": "2", "method": "ListTasks", "params": {} }),
    )
    .await;
    assert_eq!(mine["result"]["tasks"].as_array().map(Vec::len), Some(1));

    // Another principal sees nothing, and cannot reach the task by id.
    let theirs = post_rpc_as(
        app.clone(),
        "other",
        json!({ "jsonrpc": "2.0", "id": "3", "method": "ListTasks", "params": {} }),
    )
    .await;
    assert_eq!(
        theirs["result"]["tasks"].as_array().map(Vec::len),
        Some(0),
        "u2 must not see u1's tasks: {theirs:?}"
    );

    let stolen_get = post_rpc_as(
        app.clone(),
        "other",
        json!({ "jsonrpc": "2.0", "id": "4", "method": "tasks/get", "params": { "id": task_id } }),
    )
    .await;
    // Masked as not-found so the id's existence is not disclosed.
    assert_eq!(stolen_get["error"]["code"], json!(-32001));

    let stolen_cancel = post_rpc_as(
        app,
        "other",
        json!({ "jsonrpc": "2.0", "id": "5", "method": "tasks/cancel", "params": { "id": task_id } }),
    )
    .await;
    assert_eq!(stolen_cancel["error"]["code"], json!(-32001));
}

/// Fetch the Agent Card as the principal behind `bearer` and return its skill ids.
fn skill_ids(card: &Value) -> Vec<String> {
    card["skills"]
        .as_array()
        .expect("skills array")
        .iter()
        .filter_map(|s| s["id"].as_str().map(str::to_string))
        .collect()
}

/// Skills the **extended** card advertises to `bearer`'s principal
/// (srs FR-AGC-004). This is the per-caller surface; the public card is
/// resolved once for the anonymous principal and does not vary by bearer.
async fn advertised_skills(router: axum::Router, bearer: &str) -> Vec<String> {
    let req = Request::builder()
        .uri("/agent/authenticatedExtendedCard")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::empty())
        .unwrap();
    skill_ids(&body_json(router.oneshot(req).await.unwrap()).await)
}

/// Skills the **public** card advertises (srs FR-AGC-003) — no credentials,
/// resolved for the anonymous principal at build time.
async fn public_skills(router: axum::Router) -> Vec<String> {
    let req = Request::builder()
        .uri("/.well-known/agent-card.json")
        .body(Body::empty())
        .unwrap();
    skill_ids(&body_json(router.oneshot(req).await.unwrap()).await)
}

/// Whether `bearer`'s principal can actually invoke `skill_id`.
///
/// An ACL denial reaches the caller as a REJECTED task carrying the fixed
/// "Access denied" string (srs FR-ERR-003 / FR-ERR-012), which is exactly what
/// distinguishes "refused by the ACL" from "ran and failed" (`test.guard` fails
/// with an invalid-input message, not this one).
async fn is_callable(router: axum::Router, bearer: &str, skill_id: &str) -> bool {
    let sent = post_rpc_as(
        router,
        bearer,
        json!({
            "jsonrpc": "2.0", "id": "1", "method": "message/send",
            "params": {
                "message": { "messageId": "m1", "role": "ROLE_USER", "parts": [{ "data": {} }] },
                "metadata": { "skillId": skill_id }
            }
        }),
    )
    .await;
    let text = sent["result"]["status"]["message"]["parts"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    text != "Access denied"
}

#[tokio::test]
async fn agent_card_filter_and_acl_enforcement_agree_for_the_same_principal() {
    // The card filter evaluated the authenticated principal while the call
    // path evaluated `@external` (see `ApCoreAgentExecutor::build_context`:
    // apcore's `Context::child` derives `caller_id` from an empty `call_chain`
    // before `BuiltinACLCheck` runs, because `caller_id` names the calling
    // MODULE, not the principal). It also passed `ctx: None`, which makes
    // apcore's `check_conditions` return false unconditionally — so a rule
    // carrying a `conditions:` block was inert on the card path and live on
    // the call path.
    //
    // Under the ACL below that produced both failure directions at once: `u1`
    // was advertised `test.internal` (rule 2, which the enforcement path can
    // never match) and refused every call to it, while `test.echo` /
    // `test.guard` were callable (rule 3) but hidden, because rule 3's
    // condition could not be evaluated without a context.
    //
    // This test drives *both* surfaces with one ACL and asserts they agree.
    use apcore::acl::{ACLRule, ACL};
    use apcore::config::Config;
    use apcore::executor::Executor;

    let rule =
        |callers: &[&str], targets: &[&str], effect: &str, conditions: Option<Value>| ACLRule {
            callers: callers.iter().map(|c| (*c).to_string()).collect(),
            targets: targets.iter().map(|t| (*t).to_string()).collect(),
            effect: effect.to_string(),
            approval: None,
            description: None,
            conditions,
        };
    let acl = ACL::new(
        vec![
            // 1. Two skills are off-limits to unauthenticated callers — which,
            //    on the enforcement path, is every caller.
            rule(
                &["@external"],
                &["test.internal", "test.slow"],
                "deny",
                None,
            ),
            // 2. A rule naming a principal by id. `callers:` matches the
            //    calling module, and a top-level call has none, so this rule
            //    is inert by design — and the card must not pretend otherwise.
            rule(&["u1"], &["test.internal"], "allow", None),
            // 3. The designed way to discriminate callers: a condition on the
            //    identity, which `Context::child` clones through and
            //    `BuiltinACLCheck` hands to `ACL::check`.
            rule(
                &["*"],
                &["*"],
                "allow",
                Some(json!({ "identity_types": ["test"] })),
            ),
        ],
        "deny",
        None,
    );
    let mut executor = Executor::new(echo_registry(), Config::default());
    executor.set_acl(acl);

    let (app, _card) = apcore_a2a::build_app_with_auth(
        BackendSource::Executor(Arc::new(executor)),
        APCoreA2AConfig::default(),
        Some(Arc::new(TestAuth)),
    )
    .await
    .expect("build app with auth");

    // The public card answers "what may anyone call". Nothing, here: rule 1
    // denies two skills to `@external` outright, and rule 3's `identity_types`
    // condition cannot match a caller with no identity, so the default `deny`
    // takes the rest. Before this became the contract, Python and TypeScript
    // published all four to any anonymous caller.
    assert!(
        public_skills(app.clone()).await.is_empty(),
        "the public card must not advertise what an anonymous caller cannot invoke"
    );

    let all_skills = ["test.echo", "test.guard", "test.internal", "test.slow"];
    for principal in ["good", "weak"] {
        let advertised = advertised_skills(app.clone(), principal).await;
        let mut callable: Vec<String> = vec![];
        for skill in all_skills {
            if is_callable(app.clone(), principal, skill).await {
                callable.push(skill.to_string());
            }
        }
        assert_eq!(
            advertised, callable,
            "the card filter and the ACL must agree for principal {principal}"
        );
    }

    // And the agreed-on answer is the right one: the trusted identity type
    // gets the two skills rule 3 allows and neither of the two rule 1 denies;
    // the untrusted one gets nothing.
    assert_eq!(
        advertised_skills(app.clone(), "good").await,
        vec!["test.echo".to_string(), "test.guard".to_string()],
    );
    assert!(advertised_skills(app, "weak").await.is_empty());
}

#[tokio::test]
async fn an_acl_approval_gate_hides_a_skill_from_the_public_card_only() {
    // apcore 0.28.0 (PROTOCOL_SPEC §6.1.6) lets an ACL rule require a human
    // without denying the call, and §6.9 composes that with the module
    // annotation by union. A skill the operator gated that way is not something
    // an anonymous caller can just call, so it leaves the public card exactly as
    // an annotated one does — and it stays on the extended card, because the
    // caller *is* authorized: the gate is a prompt they can satisfy, not a
    // refusal. The call itself then meets apcore's approval gate, which is what
    // `APPROVAL_PENDING` -> `input_required` already reports.
    //
    // The regression this pins is the fold: `ACL::check` collapses the two axes
    // and returns false for allow-with-approval, so a filter written against the
    // boolean would delete the skill from BOTH cards, reporting a refusal the
    // ACL never issued.
    use apcore::acl::{ACLRule, ApprovalRequirement, ACL};
    use apcore::config::Config;
    use apcore::executor::Executor;

    let acl = ACL::new(
        vec![
            ACLRule {
                callers: vec!["*".into()],
                targets: vec!["test.guard".into()],
                effect: "allow".into(),
                approval: Some(ApprovalRequirement::Required),
                description: None,
                conditions: None,
            },
            ACLRule {
                callers: vec!["*".into()],
                targets: vec!["*".into()],
                effect: "allow".into(),
                approval: None,
                description: None,
                conditions: None,
            },
        ],
        "deny",
        None,
    );
    let mut executor = Executor::new(echo_registry(), Config::default());
    executor.set_acl(acl);

    let (app, _card) = apcore_a2a::build_app_with_auth(
        BackendSource::Executor(Arc::new(executor)),
        APCoreA2AConfig::default(),
        Some(Arc::new(TestAuth)),
    )
    .await
    .expect("build app with auth");

    let mut public = public_skills(app.clone()).await;
    public.sort();
    assert_eq!(
        public,
        vec![
            "test.echo".to_string(),
            "test.internal".to_string(),
            "test.slow".to_string()
        ],
        "an ACL-gated skill must leave the public card"
    );

    let mut extended = advertised_skills(app, "good").await;
    extended.sort();
    assert_eq!(
        extended,
        vec![
            "test.echo".to_string(),
            "test.guard".to_string(),
            "test.internal".to_string(),
            "test.slow".to_string()
        ],
        "the extended card must keep a skill the caller may reach behind a human"
    );
}

/// A registry carrying apcore's real management namespace beside a user module,
/// with `sys_modules.events` on so the three `system.control.*` write modules are
/// registered too.
///
/// The modules are registered by apcore itself rather than hand-rolled: this
/// crate's `Registry::register_module` rejects the id outright
/// (`InvalidModuleId: Module ID contains reserved word: 'system'`), so apcore's
/// own registration is the only way the namespace can reach a registry — which
/// is also why the visibility rule can key on the prefix without fear of a user
/// module colliding with it.
fn system_namespace_executor(
    acl: Option<apcore::acl::ACL>,
) -> (Arc<Registry>, apcore::executor::Executor) {
    use apcore::config::Config;
    use apcore::executor::Executor;

    let registry = Registry::new();
    registry
        .register_module("test.echo", Box::new(EchoModule))
        .expect("register echo module");
    let registry = Arc::new(registry);

    let mut executor = Executor::new(registry.clone(), Config::default());
    if let Some(acl) = acl {
        executor.set_acl(acl);
    }

    let mut sys_config = Config::default();
    sys_config.set("sys_modules.enabled", json!(true));
    sys_config.set("sys_modules.events.enabled", json!(true));
    apcore::register_sys_modules(registry.clone(), &executor, &sys_config, None)
        .expect("register apcore system modules");

    (registry, executor)
}

/// Every `tracing` message emitted while `f` runs, joined by newline.
///
/// Uses `tracing::subscriber::set_default`, which is **thread-local** — not
/// `set_global_default` — so this cannot race the other tests in this binary,
/// and `#[tokio::test]` builds a current-thread runtime, so the emitting code
/// stays on the thread the guard covers. That is what makes asserting a log
/// line affordable here; without it srs FR-AGC-007 criterion 2 would be the one
/// requirement this crate states and never checks.
async fn captured_logs<F, T>(f: F) -> (T, String)
where
    F: std::future::Future<Output = T>,
{
    use std::sync::Mutex;
    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::{Context as LayerContext, Layer, SubscriberExt};

    #[derive(Default)]
    struct Message(String);
    impl Visit for Message {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.0 = format!("{value:?}");
            }
        }
    }

    struct Capture(Arc<Mutex<Vec<String>>>);
    impl<S: tracing::Subscriber> Layer<S> for Capture {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: LayerContext<'_, S>) {
            let mut message = Message::default();
            event.record(&mut message);
            self.0.lock().unwrap().push(message.0);
        }
    }

    let seen = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::Registry::default().with(Capture(seen.clone()));
    let guard = tracing::subscriber::set_default(subscriber);
    let out = f.await;
    drop(guard);
    let text = seen.lock().unwrap().join("\n");
    (out, text)
}

fn system_ids(registry: &Registry) -> Vec<String> {
    let mut ids = registry.list(None, Some("system."), None);
    ids.sort();
    ids
}

fn allow_all_acl(targets: &str, effect: &str) -> apcore::acl::ACL {
    use apcore::acl::{ACLRule, ACL};
    ACL::new(
        vec![ACLRule {
            callers: vec!["*".into()],
            targets: vec![targets.into()],
            effect: effect.into(),
            approval: None,
            description: None,
            conditions: None,
        }],
        "allow",
        None,
    )
}

#[tokio::test]
async fn public_card_excludes_the_system_namespace_with_no_acl() {
    // srs FR-AGC-003 criteria 12 and 13 — the case both ACL-shaped rules leave
    // open. With no ACL the ACL predicates are empty and the annotation covers
    // only `system.control.*`, so without the namespace rule the read modules
    // would publish the deployment's module inventory and health to any
    // anonymous caller on the auth-exempt `/.well-known/` route. This crate
    // returned the card unfiltered in that state (`handlers.rs`: `let Some(acl)
    // = executor.acl() else { return card.clone() }`).
    let (registry, executor) = system_namespace_executor(None);
    assert!(!system_ids(&registry).is_empty(), "precondition");

    let (app, _card) = apcore_a2a::build_app(
        BackendSource::Executor(Arc::new(executor)),
        APCoreA2AConfig::default(),
    )
    .await
    .expect("build app");

    assert_eq!(public_skills(app).await, vec!["test.echo".to_string()]);
}

#[tokio::test]
async fn public_card_excludes_the_system_namespace_even_when_the_acl_allows_it() {
    // The subtraction is unconditional, not a consequence of the ACL denying
    // them: an ACL that explicitly allows everything must not put them back.
    let (_registry, executor) = system_namespace_executor(Some(allow_all_acl("*", "allow")));

    let (app, _card) = apcore_a2a::build_app(
        BackendSource::Executor(Arc::new(executor)),
        APCoreA2AConfig::default(),
    )
    .await
    .expect("build app");

    assert_eq!(public_skills(app).await, vec!["test.echo".to_string()]);
}

#[tokio::test]
async fn extended_card_keeps_the_system_namespace() {
    // srs FR-AGC-004 criterion 11. The exclusion is a property of the public
    // card, not of the skill — an authenticated management agent the ACL permits
    // must still be able to discover the surface it may drive. The
    // `system.control.*` modules are present because `requires_approval` keeps a
    // skill on the extended card (criterion 2), so this also shows the two rules
    // composing rather than one masking the other.
    let (registry, executor) = system_namespace_executor(None);
    let registered = system_ids(&registry);

    let (app, _card) = apcore_a2a::build_app_with_auth(
        BackendSource::Executor(Arc::new(executor)),
        APCoreA2AConfig::default(),
        Some(Arc::new(TestAuth)),
    )
    .await
    .expect("build app with auth");

    let extended = advertised_skills(app, "good").await;
    for id in &registered {
        assert!(
            extended.contains(id),
            "the extended card must carry {id}, got {extended:?}"
        );
    }
    assert!(
        registered
            .iter()
            .any(|id| id.starts_with("system.control.")),
        "precondition: sys_modules.events must have registered the write modules"
    );
    assert!(extended.contains(&"test.echo".to_string()));
}

#[tokio::test]
async fn an_unprotected_control_surface_warns_without_refusing_to_start() {
    // srs FR-AGC-007 criteria 1, 2 and 4: the message is emitted, the server
    // still starts, and neither card changes.
    //
    // The condition is real: `system.control.*` declares `requires_approval`,
    // but apcore's approval gate warns once and continues when no
    // `ApprovalHandler` is configured, so withholding the namespace from the
    // card removes it from discovery and not from dispatch.
    let (_registry, executor) = system_namespace_executor(None);
    assert!(
        executor.governance_state().unprotected_control_surface,
        "precondition: control modules registered with no gate engaging"
    );

    let (built, logged) = captured_logs(apcore_a2a::build_app(
        BackendSource::Executor(Arc::new(executor)),
        APCoreA2AConfig::default(),
    ))
    .await;
    let (app, _card) =
        built.expect("an unprotected control surface must warn, never refuse to start");

    assert!(
        logged.contains("system.control"),
        "the warning must name the control surface; logged: {logged:?}"
    );
    assert!(
        logged.contains("remain callable"),
        "the warning must say the modules are still callable; logged: {logged:?}"
    );
    assert_eq!(public_skills(app).await, vec!["test.echo".to_string()]);
}

#[tokio::test]
async fn a_gated_control_surface_does_not_warn() {
    // srs FR-AGC-007 criterion 3. A diagnostic that fires when the operator HAS
    // configured governance is one nobody reads.
    let (_registry, executor) = system_namespace_executor(Some(allow_all_acl("*", "allow")));
    assert!(
        !executor.governance_state().unprotected_control_surface,
        "precondition: an ACL is attached and the gate is wired"
    );

    let (built, logged) = captured_logs(apcore_a2a::build_app(
        BackendSource::Executor(Arc::new(executor)),
        APCoreA2AConfig::default(),
    ))
    .await;
    let (_app, _card) = built.expect("build app");

    assert!(
        !logged.contains("remain callable"),
        "no control-surface warning was due; logged: {logged:?}"
    );
}

#[tokio::test]
async fn the_system_namespace_is_still_subject_to_the_acl_on_the_extended_card() {
    // Keeping the namespace off the public card must not exempt it from the ACL
    // on the surface where the ACL does apply.
    let (_registry, executor) = system_namespace_executor(Some(allow_all_acl("system.*", "deny")));

    let (app, _card) = apcore_a2a::build_app_with_auth(
        BackendSource::Executor(Arc::new(executor)),
        APCoreA2AConfig::default(),
        Some(Arc::new(TestAuth)),
    )
    .await
    .expect("build app with auth");

    assert_eq!(
        advertised_skills(app, "good").await,
        vec!["test.echo".to_string()]
    );
}

#[tokio::test]
async fn sys_modules_true_actually_registers_the_system_modules() {
    // apcore-a2a#5: this crate passed `Config::default()` to
    // `register_sys_modules`, which reads `sys_modules.enabled` and returns an
    // empty context when it is absent — and `let _ =` swallowed the fact. So the
    // flag was a silent no-op, and the visibility rule above had nothing to act
    // on. Both are fixed together: repairing this alone is what would have
    // opened the hole.
    let registry = echo_registry();
    let config = APCoreA2AConfig {
        sys_modules: true,
        ..Default::default()
    };
    let (app, _card) = apcore_a2a::build_app(BackendSource::Registry(registry.clone()), config)
        .await
        .expect("build app");

    let mut system: Vec<String> = registry.list(None, Some("system."), None);
    system.sort();
    assert!(
        !system.is_empty(),
        "sys_modules = true must register apcore's system.* modules"
    );
    assert!(system.iter().any(|id| id.starts_with("system.health.")));

    // And none of them reaches the public card (srs FR-AGC-003 criterion 12).
    let public = public_skills(app).await;
    assert!(
        !public.iter().any(|id| id.starts_with("system.")),
        "registered system modules must stay off the public card, got {public:?}"
    );
}

#[tokio::test]
async fn sys_modules_false_registers_nothing() {
    // The other half of apcore-a2a#5: the flag was a no-op in BOTH directions,
    // and a suite that only ever asserts the `true` case cannot tell "off" from
    // "broken". The public card cannot carry this assertion — the namespace rule
    // would hide a leaked module anyway — so it is made against the registry.
    let registry = echo_registry();
    let (_app, _card) = apcore_a2a::build_app(
        BackendSource::Registry(registry.clone()),
        APCoreA2AConfig::default(),
    )
    .await
    .expect("build app");

    assert!(
        !APCoreA2AConfig::default().sys_modules,
        "precondition: the default is opt-out"
    );
    assert!(
        system_ids(&registry).is_empty(),
        "sys_modules = false must register nothing, got {:?}",
        system_ids(&registry)
    );
}

#[tokio::test]
async fn agent_card_filter_does_not_re_drive_the_acl_audit_sink() {
    // `/.well-known/agent-card.json` is auth-exempt and the filter runs
    // `ACL::check` per skill, each of which calls the consumer's audit sink —
    // a synchronous `Fn(&AuditEntry)` that may write a file or a socket. Any
    // anonymous client could therefore generate `skills.len()` governance
    // entries per request at arbitrary rate, all recording `decision: "deny"`
    // and indistinguishable from real enforcement decisions.
    use apcore::acl::{ACLRule, AuditEntry, ACL};
    use apcore::config::Config;
    use apcore::executor::Executor;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let entries = Arc::new(AtomicUsize::new(0));
    let counter = entries.clone();
    let acl = ACL::new(
        vec![ACLRule {
            callers: vec!["*".into()],
            targets: vec!["test.echo".into()],
            effect: "allow".into(),
            approval: None,
            description: None,
            conditions: None,
        }],
        "deny",
        Some(Arc::new(move |_: &AuditEntry| {
            counter.fetch_add(1, Ordering::SeqCst);
        })),
    );
    let mut executor = Executor::new(echo_registry(), Config::default());
    executor.set_acl(acl);

    let (app, _card) = build_app(
        BackendSource::Executor(Arc::new(executor)),
        APCoreA2AConfig::default(),
    )
    .await
    .expect("build app");

    let fetch = |app: axum::Router| async move {
        let req = Request::builder()
            .uri("/.well-known/agent-card.json")
            .body(Body::empty())
            .unwrap();
        body_json(app.oneshot(req).await.unwrap()).await
    };

    let first = fetch(app.clone()).await;
    let after_first = entries.load(Ordering::SeqCst);
    assert!(after_first > 0, "the first fetch does evaluate the ACL");

    for _ in 0..20 {
        let card = fetch(app.clone()).await;
        assert_eq!(card["skills"], first["skills"], "cached card must match");
    }
    assert_eq!(
        entries.load(Ordering::SeqCst),
        after_first,
        "repeat discovery requests must not re-drive the audit sink"
    );
}

#[tokio::test]
async fn sys_modules_flag_builds_and_serves() {
    let cfg = APCoreA2AConfig {
        sys_modules: true,
        ..Default::default()
    };
    let (router, _card) = build_app(BackendSource::Registry(echo_registry()), cfg)
        .await
        .expect("build app with sys_modules");
    let req = Request::builder()
        .uri("/.well-known/agent-card.json")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn tasks_get_unknown_is_task_not_found() {
    let (_status, resp) = post_rpc(
        build().await,
        json!({ "jsonrpc": "2.0", "id": "4", "method": "tasks/get", "params": { "id": "nope" } }),
    )
    .await;
    assert_eq!(resp["error"]["code"], json!(-32001));
}

#[tokio::test]
async fn tasks_cancel_unknown_is_task_not_found_and_creates_nothing() {
    // `tasks/cancel` used to write unconditionally, so an unknown id became a
    // persisted CANCELED task — and `tasks/get` reported the same id as missing.
    // The two methods must agree on whether a task exists.
    let router = build().await;
    let (_status, resp) = post_rpc(
        router.clone(),
        json!({ "jsonrpc": "2.0", "id": "1", "method": "tasks/cancel", "params": { "id": "nope" } }),
    )
    .await;
    assert_eq!(resp["error"]["code"], json!(-32001));

    let (_status, after) = post_rpc(
        router,
        json!({ "jsonrpc": "2.0", "id": "2", "method": "tasks/get", "params": { "id": "nope" } }),
    )
    .await;
    assert_eq!(after["error"]["code"], json!(-32001));
}

#[tokio::test]
async fn tasks_cancel_on_completed_task_preserves_its_artifacts() {
    // Cancelling a COMPLETED task used to overwrite it with an empty CANCELED
    // task, destroying the tool output the caller was about to fetch.
    let router = build().await;
    let completed = send_to(router.clone(), "test.echo").await;
    let task_id = completed["result"]["id"].as_str().unwrap().to_string();
    assert_eq!(
        completed["result"]["status"]["state"],
        json!("TASK_STATE_COMPLETED")
    );

    let (_status, cancel) = post_rpc(
        router.clone(),
        json!({ "jsonrpc": "2.0", "id": "2", "method": "tasks/cancel", "params": { "id": task_id } }),
    )
    .await;
    // srs FR-TSK-005: terminal states are not cancelable.
    assert_eq!(cancel["error"]["code"], json!(-32002));
    assert!(cancel["error"]["message"]
        .as_str()
        .unwrap()
        .contains("TASK_STATE_COMPLETED"));

    let (_status, fetched) = post_rpc(
        router,
        json!({ "jsonrpc": "2.0", "id": "3", "method": "tasks/get", "params": { "id": task_id } }),
    )
    .await;
    let task = &fetched["result"];
    assert_eq!(task["status"]["state"], json!("TASK_STATE_COMPLETED"));
    assert_eq!(
        task["artifacts"].as_array().map(Vec::len),
        Some(1),
        "the completed task's artifact must survive a late cancel"
    );
}

#[tokio::test]
async fn tasks_cancel_succeeds_while_the_task_is_in_flight() {
    // The guarded handler must still cancel from a non-terminal state.
    let router = build().await;
    let sending = tokio::spawn({
        let router = router.clone();
        async move { send_to(router, "test.slow").await }
    });

    let task_id = await_in_flight_task_id(router.clone()).await;

    let (_status, cancel) = post_rpc(
        router,
        json!({ "jsonrpc": "2.0", "id": "2", "method": "tasks/cancel", "params": { "id": task_id } }),
    )
    .await;
    assert!(
        cancel.get("error").is_none(),
        "unexpected error: {cancel:?}"
    );
    assert_eq!(
        cancel["result"]["status"]["state"],
        json!("TASK_STATE_CANCELED")
    );
    assert_eq!(
        cancel["result"]["status"]["message"]["parts"][0]["text"],
        json!("Canceled by client")
    );
    let _ = sending.await;
}

async fn build_explorer() -> axum::Router {
    let cfg = APCoreA2AConfig {
        explorer: true,
        ..Default::default()
    };
    let (router, _card) = build_app(BackendSource::Registry(echo_registry()), cfg)
        .await
        .expect("build explorer app");
    router
}

#[tokio::test]
async fn explorer_serves_html_and_card() {
    let router = build_explorer().await;
    // UI page.
    let html_req = Request::builder()
        .uri("/explorer")
        .body(Body::empty())
        .unwrap();
    let html_resp = router.clone().oneshot(html_req).await.unwrap();
    assert_eq!(html_resp.status(), StatusCode::OK);
    let bytes = to_bytes(html_resp.into_body(), usize::MAX).await.unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("<html"));

    // Explorer agent card with per-skill input schemas.
    let card_req = Request::builder()
        .uri("/explorer/agent-card")
        .body(Body::empty())
        .unwrap();
    let card_resp = router.oneshot(card_req).await.unwrap();
    assert_eq!(card_resp.status(), StatusCode::OK);
    let card = body_json(card_resp).await;
    assert_eq!(card["_inputSchemas"]["test.echo"]["type"], json!("object"));
}

#[tokio::test]
async fn push_config_set_get_delete() {
    let router = build().await;
    // The three push-config methods are task-addressed and now require the
    // caller to own the task, so the id has to be a real one.
    let task_id = send_to(router.clone(), "test.echo").await["result"]["id"]
        .as_str()
        .expect("task id")
        .to_string();
    let cfg = json!({ "url": "https://hook.example.com/n", "token": "t1" });

    let (_s, set) = post_rpc(
        router.clone(),
        json!({"jsonrpc":"2.0","id":"1","method":"tasks/pushNotificationConfig/set",
               "params":{"id":task_id,"pushNotificationConfig":cfg}}),
    )
    .await;
    assert_eq!(
        set["result"]["pushNotificationConfig"]["url"],
        json!("https://hook.example.com/n")
    );

    let (_s, got) = post_rpc(
        router.clone(),
        json!({"jsonrpc":"2.0","id":"2","method":"tasks/pushNotificationConfig/get","params":{"id":task_id}}),
    )
    .await;
    assert_eq!(
        got["result"]["pushNotificationConfig"]["token"],
        json!("t1")
    );

    let (_s, _del) = post_rpc(
        router.clone(),
        json!({"jsonrpc":"2.0","id":"3","method":"tasks/pushNotificationConfig/delete","params":{"id":task_id}}),
    )
    .await;
    let (_s, after) = post_rpc(
        router,
        json!({"jsonrpc":"2.0","id":"4","method":"tasks/pushNotificationConfig/get","params":{"id":task_id}}),
    )
    .await;
    assert_eq!(after["error"]["code"], json!(-32001));
}

#[tokio::test]
async fn push_config_is_scoped_to_the_authenticated_principal() {
    // `tasks/pushNotificationConfig/*` bypassed the ownership check entirely,
    // so a principal holding another's task id could point its terminal
    // `statusUpdate` at an attacker-controlled webhook (`/set`) or silently
    // suppress the owner's notifications (`/delete`). Only UUIDv4 task ids
    // stood in the way.
    let app = build_with_auth().await;

    let task_id = post_rpc_as(
        app.clone(),
        "good",
        json!({
            "jsonrpc": "2.0", "id": "1", "method": "message/send",
            "params": {
                "message": { "messageId": "m1", "role": "ROLE_USER", "parts": [{ "data": {} }] },
                "metadata": { "skillId": "test.echo" }
            }
        }),
    )
    .await["result"]["id"]
        .as_str()
        .expect("task id")
        .to_string();

    // u1 owns it and may configure it.
    let mine = post_rpc_as(
        app.clone(),
        "good",
        json!({"jsonrpc":"2.0","id":"2","method":"tasks/pushNotificationConfig/set",
               "params":{"id":task_id,"pushNotificationConfig":{"url":"https://ok.example/n"}}}),
    )
    .await;
    assert_eq!(
        mine["result"]["pushNotificationConfig"]["url"],
        json!("https://ok.example/n")
    );

    // u2 holding the same id reaches none of the three, and the id's existence
    // is not disclosed.
    for (id, method, params) in [
        (
            "3",
            "tasks/pushNotificationConfig/set",
            json!({"id":task_id,"pushNotificationConfig":{"url":"https://attacker.example/n"}}),
        ),
        (
            "4",
            "tasks/pushNotificationConfig/get",
            json!({"id":task_id}),
        ),
        (
            "5",
            "tasks/pushNotificationConfig/delete",
            json!({"id":task_id}),
        ),
    ] {
        let stolen = post_rpc_as(
            app.clone(),
            "other",
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
        )
        .await;
        assert_eq!(
            stolen["error"]["code"],
            json!(-32001),
            "{method}: {stolen:?}"
        );
    }

    // The owner's config is intact: neither overwritten nor deleted.
    let after = post_rpc_as(
        app,
        "good",
        json!({"jsonrpc":"2.0","id":"6","method":"tasks/pushNotificationConfig/get","params":{"id":task_id}}),
    )
    .await;
    assert_eq!(
        after["result"]["pushNotificationConfig"]["url"],
        json!("https://ok.example/n")
    );
}

#[tokio::test]
async fn push_notification_delivered_to_webhook() {
    use axum::routing::post as axum_post;

    // Spin up a webhook receiver on an ephemeral port.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Value>(4);
    let receiver = axum::Router::new().route(
        "/hook",
        axum_post(move |axum::Json(body): axum::Json<Value>| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(body).await;
                StatusCode::OK
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, receiver).await.unwrap();
    });
    let webhook_url = format!("http://{addr}/hook");

    // Start a real long-running task, register a webhook for it, then cancel it:
    // tasks/cancel emits the terminal CANCELED status, which is delivered to the
    // webhook. (tasks/cancel no longer accepts an arbitrary id, so the task has
    // to exist first.)
    let app = build().await;
    let sending = tokio::spawn({
        let app = app.clone();
        async move { send_to(app, "test.slow").await }
    });
    let task_id = await_in_flight_task_id(app.clone()).await;

    let (_s, _set) = post_rpc(
        app.clone(),
        json!({"jsonrpc":"2.0","id":"1","method":"tasks/pushNotificationConfig/set",
               "params":{"id":task_id,"pushNotificationConfig":{"url":webhook_url}}}),
    )
    .await;

    let (_s, _cancel) = post_rpc(
        app,
        json!({"jsonrpc":"2.0","id":"2","method":"tasks/cancel","params":{"id":task_id}}),
    )
    .await;

    let received = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("webhook delivery timed out")
        .expect("channel closed");
    assert_eq!(
        received["statusUpdate"]["status"]["state"],
        json!("TASK_STATE_CANCELED")
    );
    assert_eq!(received["statusUpdate"]["taskId"], json!(task_id));
    let _ = sending.await;
}

/// Build a server over caller-supplied stores.
///
/// Calling this twice with the same `Arc` stores models a **restart**: the
/// stores survive, the server around them is rebuilt from scratch.
async fn build_with_stores(
    task_store: Arc<dyn apcore_a2a::TaskStore>,
    push_config_store: Arc<dyn apcore_a2a::PushConfigStore>,
) -> axum::Router {
    let registry = echo_registry();
    let executor = Arc::new(apcore::executor::Executor::new(
        registry.clone(),
        apcore::config::Config::default(),
    ));
    let agent_executor = Arc::new(apcore_a2a::ApCoreAgentExecutor::new(
        executor,
        apcore_a2a::PartConverter::new(apcore_a2a::SchemaConverter::new()),
        300,
    ));
    let opts = apcore_a2a::CreateOptions::new(
        agent_executor,
        "test-agent",
        "test agent",
        "0.0.0",
        "http://localhost:8000",
    )
    .with_task_store(task_store)
    .with_push_config_store(push_config_store)
    .with_auth(Arc::new(TestAuth));
    let (router, _card) = apcore_a2a::A2AServerFactory::new().create(&registry, opts);
    router
}

/// Submit a task as `bearer` and return its id.
async fn send_as(router: axum::Router, bearer: &str) -> String {
    let result = post_rpc_as(
        router,
        bearer,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "message/send",
            "params": {
                "message": { "messageId": "m1", "role": "ROLE_USER", "parts": [{ "data": { "hello": "world" } }] },
                "metadata": { "skillId": "test.echo" }
            }
        }),
    )
    .await;
    result["result"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("send failed: {result}"))
        .to_string()
}

#[tokio::test]
async fn a_persistent_store_keeps_task_isolation_across_a_restart() {
    // Ownership used to live in a process-memory map beside the store, so a
    // consumer-supplied persistent store came back from a restart holding
    // tasks that no longer had a recorded owner — and `is_owned_by` fails
    // closed, which made every one of them permanently unreachable to its
    // genuine owner (`tasks/get` -32001, `ListTasks` []). Owner now lives in
    // the store, so a restart preserves both halves: the owner still reaches
    // its tasks, and nobody else does.
    let tasks: Arc<dyn apcore_a2a::TaskStore> = Arc::new(apcore_a2a::InMemoryTaskStore::new());
    let push: Arc<dyn apcore_a2a::PushConfigStore> =
        Arc::new(apcore_a2a::InMemoryPushConfigStore::new());

    let task_id = send_as(build_with_stores(tasks.clone(), push.clone()).await, "good").await;

    // Restart: same stores, a brand-new server that never saw the submission.
    let restarted = || build_with_stores(tasks.clone(), push.clone());

    let owner_get = post_rpc_as(
        restarted().await,
        "good",
        json!({"jsonrpc": "2.0", "id": 2, "method": "tasks/get", "params": {"id": task_id}}),
    )
    .await;
    assert_eq!(
        owner_get["result"]["id"].as_str(),
        Some(task_id.as_str()),
        "the owner must still reach its task after a restart, got {owner_get}"
    );

    let owner_list = post_rpc_as(
        restarted().await,
        "good",
        json!({"jsonrpc": "2.0", "id": 3, "method": "ListTasks", "params": {}}),
    )
    .await;
    let listed: Vec<&str> = owner_list["result"]["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect();
    assert_eq!(listed, vec![task_id.as_str()]);

    // Isolation survives the restart too: another principal still sees nothing.
    let other_get = post_rpc_as(
        restarted().await,
        "other",
        json!({"jsonrpc": "2.0", "id": 4, "method": "tasks/get", "params": {"id": task_id}}),
    )
    .await;
    assert_eq!(other_get["error"]["code"], -32001);

    let other_list = post_rpc_as(
        restarted().await,
        "other",
        json!({"jsonrpc": "2.0", "id": 5, "method": "ListTasks", "params": {}}),
    )
    .await;
    assert_eq!(other_list["result"]["tasks"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn a_persistent_push_config_store_keeps_its_scoping_across_a_restart() {
    // Push configs used to live in the server's own state, so a persistent
    // TaskStore restored tasks after a restart while their delivery targets
    // evaporated. A separate PushConfigStore lets both come back together —
    // still scoped to their owner.
    let tasks: Arc<dyn apcore_a2a::TaskStore> = Arc::new(apcore_a2a::InMemoryTaskStore::new());
    let push: Arc<dyn apcore_a2a::PushConfigStore> =
        Arc::new(apcore_a2a::InMemoryPushConfigStore::new());

    let task_id = send_as(build_with_stores(tasks.clone(), push.clone()).await, "good").await;
    let set = post_rpc_as(
        build_with_stores(tasks.clone(), push.clone()).await,
        "good",
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "tasks/pushNotificationConfig/set",
            "params": {"id": task_id, "pushNotificationConfig": {"url": "https://hook.example/x"}}
        }),
    )
    .await;
    assert!(set["error"].is_null(), "set failed: {set}");

    // Restart.
    let owner_get = post_rpc_as(
        build_with_stores(tasks.clone(), push.clone()).await,
        "good",
        json!({
            "jsonrpc": "2.0", "id": 3, "method": "tasks/pushNotificationConfig/get",
            "params": {"id": task_id}
        }),
    )
    .await;
    assert_eq!(
        owner_get["result"]["pushNotificationConfig"]["url"],
        "https://hook.example/x"
    );

    // Another principal still cannot read — or redirect — the owner's webhook.
    let other_get = post_rpc_as(
        build_with_stores(tasks.clone(), push.clone()).await,
        "other",
        json!({
            "jsonrpc": "2.0", "id": 4, "method": "tasks/pushNotificationConfig/get",
            "params": {"id": task_id}
        }),
    )
    .await;
    assert_eq!(other_get["error"]["code"], -32001);
}

// ---------------------------------------------------------------------------
// Agent Card visibility (srs FR-AGC-003 / FR-AGC-004 / FR-AGC-006)
// ---------------------------------------------------------------------------

/// A module the operator has gated behind human approval.
struct ApprovalGatedModule;

#[async_trait]
impl Module for ApprovalGatedModule {
    fn input_schema(&self) -> Value {
        json!({ "type": "object" })
    }
    fn output_schema(&self) -> Value {
        json!({ "type": "object" })
    }
    fn description(&self) -> &str {
        "Deletes a user"
    }
    fn annotations(&self) -> apcore::module::ModuleAnnotations {
        apcore::module::ModuleAnnotations {
            requires_approval: true,
            destructive: true,
            ..Default::default()
        }
    }
    async fn execute(&self, inputs: Value, _ctx: &Context<Value>) -> Result<Value, ModuleError> {
        Ok(inputs)
    }
}

fn gated_registry() -> Arc<Registry> {
    let registry = Registry::new();
    registry
        .register_module("test.echo", Box::new(EchoModule))
        .expect("register echo module");
    registry
        .register_module("admin.users.delete", Box::new(ApprovalGatedModule))
        .expect("register gated module");
    Arc::new(registry)
}

#[tokio::test]
async fn public_card_withholds_approval_gated_skills_even_without_an_acl() {
    // srs FR-AGC-003 criterion 7. An approval gate is an operator saying "a
    // human decides each of these", which is not something to advertise to
    // anonymous callers — and withholding it is what leaves the extended card
    // with something to carry.
    let (app, _card) = apcore_a2a::build_app(
        BackendSource::Registry(gated_registry()),
        APCoreA2AConfig::default(),
    )
    .await
    .expect("build app");

    assert_eq!(public_skills(app).await, vec!["test.echo".to_string()]);
}

#[tokio::test]
async fn extended_card_restores_approval_gated_skills_for_an_authenticated_caller() {
    // srs FR-AGC-004 criterion 2 and criterion 9: the extended card is not a
    // copy of the public one. Python used to return `CopyFrom(base_card)` and
    // this crate did not route the endpoint at all.
    let (app, _card) = apcore_a2a::build_app_with_auth(
        BackendSource::Registry(gated_registry()),
        APCoreA2AConfig::default(),
        Some(Arc::new(TestAuth)),
    )
    .await
    .expect("build app with auth");

    let public = public_skills(app.clone()).await;
    let mut extended = advertised_skills(app, "good").await;
    extended.sort();
    assert_eq!(public, vec!["test.echo".to_string()]);
    assert_eq!(
        extended,
        vec!["admin.users.delete".to_string(), "test.echo".to_string()]
    );
    assert_ne!(public, extended, "the extended card must not be a copy");
}

#[tokio::test]
async fn extended_card_capability_and_endpoint_agree() {
    // srs FR-AGC-006. Advertising a capability this crate does not serve is
    // worse than not advertising it: a client cannot tell "no extra skills
    // exist" from "this server is broken".
    let (app, card) = apcore_a2a::build_app(
        BackendSource::Registry(gated_registry()),
        APCoreA2AConfig::default(),
    )
    .await
    .expect("build app");
    assert!(!card.capabilities.extended_agent_card);

    // Not advertised, so both surfaces refuse rather than half-answering.
    let req = Request::builder()
        .uri("/agent/authenticatedExtendedCard")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(req).await.unwrap().status(),
        axum::http::StatusCode::NOT_FOUND
    );

    let (_status, rpc) = post_rpc(
        app,
        json!({"jsonrpc": "2.0", "id": 1, "method": "GetExtendedAgentCard", "params": {}}),
    )
    .await;
    assert_eq!(rpc["error"]["code"], json!(-32007));

    let (app, card) = apcore_a2a::build_app_with_auth(
        BackendSource::Registry(gated_registry()),
        APCoreA2AConfig::default(),
        Some(Arc::new(TestAuth)),
    )
    .await
    .expect("build app with auth");
    assert!(card.capabilities.extended_agent_card);

    // Advertised, so the RPC method a client is entitled to call answers.
    let rpc = post_rpc_as(
        app,
        "good",
        json!({"jsonrpc": "2.0", "id": 1, "method": "GetExtendedAgentCard", "params": {}}),
    )
    .await;
    assert_eq!(skill_ids(&rpc["result"]).len(), 2);
}

#[tokio::test]
async fn behavioral_annotations_reach_the_agent_card_as_namespaced_tags() {
    // srs FR-SKL-004. Before this, a caller could construct a call to
    // `admin.users.delete` from the card and had nothing on the card telling it
    // the call was destructive.
    let (app, _card) = apcore_a2a::build_app_with_auth(
        BackendSource::Registry(gated_registry()),
        APCoreA2AConfig::default(),
        Some(Arc::new(TestAuth)),
    )
    .await
    .expect("build app with auth");

    let req = Request::builder()
        .uri("/agent/authenticatedExtendedCard")
        .header("authorization", "Bearer good")
        .body(Body::empty())
        .unwrap();
    let card = body_json(app.oneshot(req).await.unwrap()).await;
    let gated = card["skills"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == json!("admin.users.delete"))
        .expect("gated skill on the extended card");
    let tags: Vec<&str> = gated["tags"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    // Fixed order, and only the flags that are actually set.
    assert_eq!(tags, vec!["apcore:destructive", "apcore:requires-approval"]);
    assert!(!tags.contains(&"apcore:readonly"));
    assert!(!tags.contains(&"apcore:idempotent"));
}

// ---------------------------------------------------------------------------
// Bind address (apcore-a2a-rust#2)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_agent_card_url_is_derived_from_host_and_port_when_unset() {
    // `url` is what the card publishes; `host`/`port` are what the server
    // binds. Conflating them is what let a scheme-less `url` silently bind
    // 0.0.0.0:8000. An empty `url` means "derive", as Python documents.
    let config = APCoreA2AConfig {
        host: "127.0.0.1".to_string(),
        port: 18999,
        url: String::new(),
        ..Default::default()
    };
    let (_app, card) = apcore_a2a::build_app(BackendSource::Registry(echo_registry()), config)
        .await
        .expect("build app");
    assert_eq!(
        card.supported_interfaces[0].url, "http://127.0.0.1:18999",
        "the published endpoint must be resolvable"
    );
}

#[tokio::test]
async fn a_loopback_bind_is_never_widened_to_every_interface() {
    // The old code split `url` on "://" and fell back to 0.0.0.0:8000, so a
    // scheme-less loopback value published every skill on every interface with
    // nothing logged. `host`/`port` are typed, so that is not expressible: the
    // card's `url` can say anything at all without moving the bind.
    let config = APCoreA2AConfig {
        host: "127.0.0.1".to_string(),
        port: 18999,
        // Deliberately the shape that used to trigger the fallback.
        url: "127.0.0.1:18999".to_string(),
        ..Default::default()
    };
    let addr: String = apcore_a2a::bind_addr(&config).expect("bind address");
    assert_eq!(addr, "127.0.0.1:18999");
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind");
    let bound = listener.local_addr().unwrap();
    assert_eq!(bound.ip().to_string(), "127.0.0.1");
    assert_eq!(bound.port(), 18999);
    assert!(!bound.ip().is_unspecified(), "must never become 0.0.0.0");
}

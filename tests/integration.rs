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

/// Poll `tasks/list` until the task a concurrent `message/send` persisted as
/// SUBMITTED appears, and return its (server-generated) id.
async fn await_in_flight_task_id(router: axum::Router) -> String {
    for _ in 0..100 {
        let (_status, listed) = post_rpc(
            router.clone(),
            json!({ "jsonrpc": "2.0", "id": "list", "method": "tasks/list", "params": {} }),
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
        json!({ "jsonrpc": "2.0", "method": "tasks/list", "params": {} }),
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
        let principal = match headers.get("authorization").map(String::as_str) {
            Some("Bearer good") => "u1",
            Some("Bearer other") => "u2",
            _ => return None,
        };
        Some(apcore::context::Identity::new(
            principal.into(),
            "test".into(),
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
            json!({"jsonrpc":"2.0","id":"1","method":"tasks/list","params":{}}).to_string(),
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
            json!({"jsonrpc":"2.0","id":"1","method":"tasks/list","params":{}}).to_string(),
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
    // `tasks/list` returned every caller's tasks, including the full stdout of
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
        json!({ "jsonrpc": "2.0", "id": "2", "method": "tasks/list", "params": {} }),
    )
    .await;
    assert_eq!(mine["result"]["tasks"].as_array().map(Vec::len), Some(1));

    // Another principal sees nothing, and cannot reach the task by id.
    let theirs = post_rpc_as(
        app.clone(),
        "other",
        json!({ "jsonrpc": "2.0", "id": "3", "method": "tasks/list", "params": {} }),
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
    let cfg = json!({ "url": "https://hook.example.com/n", "token": "t1" });

    let (_s, set) = post_rpc(
        router.clone(),
        json!({"jsonrpc":"2.0","id":"1","method":"tasks/pushNotificationConfig/set",
               "params":{"id":"task-1","pushNotificationConfig":cfg}}),
    )
    .await;
    assert_eq!(
        set["result"]["pushNotificationConfig"]["url"],
        json!("https://hook.example.com/n")
    );

    let (_s, got) = post_rpc(
        router.clone(),
        json!({"jsonrpc":"2.0","id":"2","method":"tasks/pushNotificationConfig/get","params":{"id":"task-1"}}),
    )
    .await;
    assert_eq!(
        got["result"]["pushNotificationConfig"]["token"],
        json!("t1")
    );

    let (_s, _del) = post_rpc(
        router.clone(),
        json!({"jsonrpc":"2.0","id":"3","method":"tasks/pushNotificationConfig/delete","params":{"id":"task-1"}}),
    )
    .await;
    let (_s, after) = post_rpc(
        router,
        json!({"jsonrpc":"2.0","id":"4","method":"tasks/pushNotificationConfig/get","params":{"id":"task-1"}}),
    )
    .await;
    assert_eq!(after["error"]["code"], json!(-32001));
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

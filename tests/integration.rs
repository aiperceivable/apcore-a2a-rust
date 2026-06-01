//! End-to-end HTTP integration tests for the A2A 1.0 server.
//!
//! Drives the assembled axum `Router` via `tower::ServiceExt::oneshot` — no
//! network sockets — exercising JSON-RPC dispatch, the Agent Card endpoint, and
//! task execution against a real apcore `Registry` holding an echo module.

use std::sync::Arc;

use apcore::context::Context;
use apcore::errors::ModuleError;
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

/// A registry with a single `test.echo` module.
fn echo_registry() -> Arc<Registry> {
    let registry = Registry::new();
    registry
        .register_module("test.echo", Box::new(EchoModule))
        .expect("register echo module");
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

#[tokio::test]
async fn message_send_missing_skill_id_is_invalid_params() {
    let (_status, resp) = post_rpc(
        build().await,
        json!({
            "jsonrpc": "2.0",
            "id": "2",
            "method": "message/send",
            "params": { "message": { "messageId": "m", "role": "ROLE_USER", "parts": [{ "text": "hi" }] } }
        }),
    )
    .await;
    assert_eq!(resp["error"]["code"], json!(-32602));
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

/// Test authenticator: accepts `Authorization: Bearer good`, rejects otherwise.
struct TestAuth;

#[async_trait]
impl apcore_a2a::Authenticator for TestAuth {
    async fn authenticate(
        &self,
        headers: &std::collections::HashMap<String, String>,
    ) -> Option<apcore::context::Identity> {
        if headers.get("authorization").map(String::as_str) == Some("Bearer good") {
            Some(apcore::context::Identity::new(
                "u1".into(),
                "test".into(),
                vec![],
                std::collections::HashMap::new(),
            ))
        } else {
            None
        }
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

    // Register a webhook for a known task id, then trigger a terminal status via
    // tasks/cancel — which delivers the CANCELED statusUpdate to the webhook.
    // (message/send generates a random task id, so a pre-registered id can't be
    // targeted; tasks/cancel accepts an explicit id, giving a deterministic test.)
    let app = build().await;
    let task_id = "push-task-1";
    let (_s, _set) = post_rpc(
        app.clone(),
        json!({"jsonrpc":"2.0","id":"1","method":"tasks/pushNotificationConfig/set",
               "params":{"id":task_id,"pushNotificationConfig":{"url":webhook_url}}}),
    )
    .await;

    // tasks/cancel emits a terminal CANCELED status for the task id, which is
    // delivered to the registered webhook.
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
}

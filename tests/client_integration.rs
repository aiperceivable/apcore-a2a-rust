//! End-to-end tests for `A2AClient` against a live in-process A2A server.
//!
//! The server is bound to an ephemeral TCP port (`axum::serve`) so the client's
//! real reqwest HTTP path — JSON-RPC dispatch, SSE streaming, and Agent Card
//! discovery — is exercised over a socket.

use std::sync::Arc;

use apcore::context::Context;
use apcore::errors::ModuleError;
use apcore::module::Module;
use apcore::registry::registry::Registry;
use apcore_a2a::{build_app, A2AClient, A2AClientError, APCoreA2AConfig, BackendSource};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{json, Value};

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

fn echo_registry() -> Arc<Registry> {
    let registry = Registry::new();
    registry
        .register_module("test.echo", Box::new(EchoModule))
        .expect("register echo module");
    Arc::new(registry)
}

/// Spawn the A2A server on an ephemeral port and return its base URL.
async fn spawn_server() -> String {
    let (router, _card) = build_app(
        BackendSource::Registry(echo_registry()),
        APCoreA2AConfig::default(),
    )
    .await
    .expect("build app");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

fn user_message(data: Value) -> Value {
    json!({
        "messageId": "m1",
        "role": "ROLE_USER",
        "parts": [{ "data": data }],
    })
}

#[tokio::test]
async fn send_message_executes_and_completes() {
    let url = spawn_server().await;
    let client = A2AClient::new(url);

    let task = client
        .send_message(
            user_message(json!({ "hello": "world" })),
            Some(json!({ "skillId": "test.echo" })),
            None,
        )
        .await
        .expect("send_message");

    assert_eq!(task["status"]["state"], json!("TASK_STATE_COMPLETED"));
    assert_eq!(
        task["artifacts"][0]["parts"][0]["data"],
        json!({ "hello": "world" })
    );
}

#[tokio::test]
async fn discover_returns_agent_card() {
    let url = spawn_server().await;
    let client = A2AClient::new(url);

    let card = client.discover().await.expect("discover");
    assert_eq!(card["skills"][0]["id"], json!("test.echo"));
    assert_eq!(
        card["supportedInterfaces"][0]["protocolVersion"],
        json!("1.0")
    );
}

#[tokio::test]
async fn get_task_unknown_is_task_not_found() {
    let url = spawn_server().await;
    let client = A2AClient::new(url);

    let err = client.get_task("does-not-exist").await.unwrap_err();
    match err {
        A2AClientError::TaskNotFound { task_id } => {
            assert_eq!(task_id.as_deref(), Some("does-not-exist"));
        }
        other => panic!("expected TaskNotFound, got {other:?}"),
    }
}

#[tokio::test]
async fn cancel_task_unknown_is_task_not_found() {
    let url = spawn_server().await;
    let client = A2AClient::new(url);

    // `tasks/cancel` on an id that was never issued must agree with `tasks/get`
    // (-32001), not fabricate a CANCELED task for it.
    let err = client.cancel_task("task-x").await.unwrap_err();
    assert!(
        matches!(err, A2AClientError::TaskNotFound { .. }),
        "expected TaskNotFound, got {err:?}"
    );
}

#[tokio::test]
async fn cancel_task_on_completed_is_task_not_cancelable() {
    let url = spawn_server().await;
    let client = A2AClient::new(url);

    let task = client
        .send_message(
            user_message(json!({ "a": 1 })),
            Some(json!({ "skillId": "test.echo" })),
            None,
        )
        .await
        .expect("send_message");
    let task_id = task["id"].as_str().expect("task id");

    // srs FR-TSK-005: a terminal task is not cancelable. The client already had
    // the typed variant for -32002; only the server never emitted it.
    let err = client.cancel_task(task_id).await.unwrap_err();
    assert!(
        matches!(err, A2AClientError::TaskNotCancelable { .. }),
        "expected TaskNotCancelable, got {err:?}"
    );
}

#[tokio::test]
async fn list_tasks_returns_tasks_array() {
    let url = spawn_server().await;
    let client = A2AClient::new(url);

    // Run one task so the store is non-empty.
    client
        .send_message(
            user_message(json!({ "a": 1 })),
            Some(json!({ "skillId": "test.echo" })),
            None,
        )
        .await
        .expect("send_message");

    let result = client.list_tasks(None, 50).await.expect("list_tasks");
    assert!(result["tasks"].is_array());
}

#[tokio::test]
async fn stream_message_yields_terminal_event() {
    let url = spawn_server().await;
    let client = A2AClient::new(url);

    let stream = client.stream_message(
        user_message(json!({ "k": "v" })),
        Some(json!({ "skillId": "test.echo" })),
        None,
    );
    futures_util::pin_mut!(stream);

    let mut saw_terminal = false;
    let mut count = 0;
    while let Some(item) = stream.next().await {
        let event = item.expect("stream event");
        count += 1;
        if let Some(state) = event
            .get("statusUpdate")
            .and_then(|s| s.get("status"))
            .and_then(|s| s.get("state"))
            .and_then(Value::as_str)
        {
            if state == "TASK_STATE_COMPLETED" {
                saw_terminal = true;
            }
        }
    }
    assert!(count > 0, "expected at least one SSE event");
    assert!(
        saw_terminal,
        "expected a terminal TASK_STATE_COMPLETED event"
    );
}

#[tokio::test]
async fn invalid_url_rejected_at_construction() {
    match A2AClient::try_new("ftp://nope", None, None, None) {
        Err(A2AClientError::InvalidUrl(_)) => {}
        Ok(_) => panic!("expected InvalidUrl error"),
        Err(other) => panic!("expected InvalidUrl, got {other:?}"),
    }
}

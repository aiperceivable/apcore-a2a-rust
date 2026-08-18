//! TEMPORARY review probe — delete after use.
use std::sync::Arc;

use apcore::context::Context;
use apcore::errors::ModuleError;
use apcore::module::Module;
use apcore::registry::registry::Registry;
use apcore_a2a::{build_app, APCoreA2AConfig, BackendSource};
use async_trait::async_trait;
use axum::body::Body;
use axum::http::Request;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tower::ServiceExt;

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
        "Sleeps a long time"
    }
    async fn execute(&self, inputs: Value, _ctx: &Context<Value>) -> Result<Value, ModuleError> {
        tokio::time::sleep(std::time::Duration::from_millis(5000)).await;
        Ok(inputs)
    }
}

async fn build() -> axum::Router {
    let registry = Registry::new();
    registry
        .register_module("test.slow", Box::new(SlowModule))
        .expect("register");
    let (router, _card) = build_app(
        BackendSource::Registry(Arc::new(registry)),
        APCoreA2AConfig::default(),
    )
    .await
    .expect("build app");
    router
}

async fn post_rpc(router: axum::Router, body: Value) -> Value {
    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn probe_cancel_of_a_streaming_task() {
    let router = build().await;

    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "jsonrpc": "2.0", "id": "s1", "method": "message/stream",
                "params": {
                    "message": { "messageId": "m1", "role": "ROLE_USER", "parts": [{ "data": {} }] },
                    "metadata": { "skillId": "test.slow" }
                }
            })
            .to_string(),
        ))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let mut stream = resp.into_body().into_data_stream();

    // Read the first SSE frame to learn the server-generated task id.
    let mut task_id = String::new();
    for _ in 0..5 {
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
            .await
            .expect("timed out waiting for an SSE frame")
            .expect("stream closed")
            .expect("body error");
        let text = String::from_utf8(chunk.to_vec()).unwrap();
        for line in text.lines() {
            if let Some(d) = line.strip_prefix("data:") {
                let frame: Value = serde_json::from_str(d.trim()).unwrap();
                if let Some(id) = frame["result"]["statusUpdate"]["taskId"].as_str() {
                    task_id = id.to_string();
                }
            }
        }
        if !task_id.is_empty() {
            break;
        }
    }
    assert!(!task_id.is_empty(), "no taskId seen on the stream");
    println!("PROBE streaming task id = {task_id}");

    // The task is in flight. Can the caller see it / cancel it?
    let got = post_rpc(
        router.clone(),
        json!({"jsonrpc":"2.0","id":1,"method":"tasks/get","params":{"id": task_id}}),
    )
    .await;
    println!("PROBE tasks/get  -> {got}");

    let listed = post_rpc(
        router.clone(),
        json!({"jsonrpc":"2.0","id":2,"method":"ListTasks","params":{}}),
    )
    .await;
    println!("PROBE ListTasks  -> {listed}");

    let cancel = post_rpc(
        router.clone(),
        json!({"jsonrpc":"2.0","id":3,"method":"tasks/cancel","params":{"id": task_id}}),
    )
    .await;
    println!("PROBE tasks/cancel -> {cancel}");
}

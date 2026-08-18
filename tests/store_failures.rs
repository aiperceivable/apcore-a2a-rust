//! Store-backend failure paths.
//!
//! Every other suite runs against the in-memory stores, which never fail — so
//! nothing exercised what the server reports when a consumer-supplied store is
//! unreachable. Two failures hid there: `pushNotificationConfig/delete`
//! answered `result: null` while the config was still live and still receiving
//! deliveries, and `ListTasks` answered `[]`, which reads as "you have no
//! tasks" rather than "the database is down".
//!
//! The rule these tests pin down: a backend failure is `-32603`, never
//! `-32001`. The two demand opposite responses from a caller — an A2A agent
//! reading *not found* re-submits work that is actually still there — so they
//! must stay distinguishable.
//!
//! Implementing the two store traits here also proves the migration path: this
//! file is an external crate and uses nothing but the public API.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use apcore::context::Context;
use apcore::errors::ModuleError;
use apcore::module::Module;
use apcore::registry::registry::Registry;
use apcore_a2a::{
    CallContext, InMemoryPushConfigStore, InMemoryTaskStore, ListParams, PushConfigStore,
    StoreError, TaskStore,
};
use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::Request;
use serde_json::{json, Value};
use tower::ServiceExt;

const INTERNAL_ERROR: i64 = -32603;
const TASK_NOT_FOUND: i64 = -32001;

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
        "Echo inputs"
    }
    async fn execute(&self, inputs: Value, _ctx: &Context<Value>) -> Result<Value, ModuleError> {
        Ok(inputs)
    }
}

/// Sleeps, so a task using it can be cancelled while still non-terminal.
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

/// Works normally until its switch is flipped, then every operation fails.
///
/// Flippable rather than always-failing so a test can store a real task first
/// and then break the backend under it — which is what an outage looks like.
struct FlakyTaskStore {
    inner: InMemoryTaskStore,
    down: Arc<AtomicBool>,
    writes_down: Arc<AtomicBool>,
}

impl FlakyTaskStore {
    fn check(&self) -> Result<(), StoreError> {
        if self.down.load(Ordering::SeqCst) {
            return Err(StoreError::backend_msg("database is unreachable"));
        }
        Ok(())
    }

    /// Reads still work, writes do not — a read-only replica, or a full disk.
    fn check_write(&self) -> Result<(), StoreError> {
        self.check()?;
        if self.writes_down.load(Ordering::SeqCst) {
            return Err(StoreError::backend_msg("database is read-only"));
        }
        Ok(())
    }
}

#[async_trait]
impl TaskStore for FlakyTaskStore {
    async fn save(&self, task_id: &str, task: Value, ctx: &CallContext) -> Result<(), StoreError> {
        self.check_write()?;
        self.inner.save(task_id, task, ctx).await
    }
    async fn get(&self, task_id: &str, ctx: &CallContext) -> Result<Option<Value>, StoreError> {
        self.check()?;
        self.inner.get(task_id, ctx).await
    }
    async fn delete(&self, task_id: &str, ctx: &CallContext) -> Result<(), StoreError> {
        self.check()?;
        self.inner.delete(task_id, ctx).await
    }
    async fn list(&self, params: &ListParams, ctx: &CallContext) -> Result<Vec<Value>, StoreError> {
        self.check()?;
        self.inner.list(params, ctx).await
    }
}

struct FlakyPushStore {
    inner: InMemoryPushConfigStore,
    down: Arc<AtomicBool>,
}

impl FlakyPushStore {
    fn check(&self) -> Result<(), StoreError> {
        if self.down.load(Ordering::SeqCst) {
            return Err(StoreError::backend_msg("database is unreachable"));
        }
        Ok(())
    }
}

#[async_trait]
impl PushConfigStore for FlakyPushStore {
    async fn save(
        &self,
        task_id: &str,
        config: Value,
        ctx: &CallContext,
    ) -> Result<(), StoreError> {
        self.check()?;
        self.inner.save(task_id, config, ctx).await
    }
    async fn get(&self, task_id: &str, ctx: &CallContext) -> Result<Option<Value>, StoreError> {
        self.check()?;
        self.inner.get(task_id, ctx).await
    }
    async fn delete(&self, task_id: &str, ctx: &CallContext) -> Result<(), StoreError> {
        self.check()?;
        self.inner.delete(task_id, ctx).await
    }
}

/// A server plus the two switches that take its stores offline.
struct Harness {
    router: axum::Router,
    tasks_down: Arc<AtomicBool>,
    task_writes_down: Arc<AtomicBool>,
    push_down: Arc<AtomicBool>,
}

fn harness() -> Harness {
    let registry = Arc::new(Registry::new());
    registry
        .register_module("test.echo", Box::new(EchoModule))
        .expect("register echo module");
    registry
        .register_module("test.slow", Box::new(SlowModule))
        .expect("register slow module");

    let tasks_down = Arc::new(AtomicBool::new(false));
    let task_writes_down = Arc::new(AtomicBool::new(false));
    let push_down = Arc::new(AtomicBool::new(false));

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
        "flaky-agent",
        "store failure harness",
        "0.0.0",
        "http://localhost:8000",
    )
    .with_task_store(Arc::new(FlakyTaskStore {
        inner: InMemoryTaskStore::new(),
        down: tasks_down.clone(),
        writes_down: task_writes_down.clone(),
    }))
    .with_push_config_store(Arc::new(FlakyPushStore {
        inner: InMemoryPushConfigStore::new(),
        down: push_down.clone(),
    }));

    let (router, _card) = apcore_a2a::A2AServerFactory::new().create(&registry, opts);
    Harness {
        router,
        tasks_down,
        task_writes_down,
        push_down,
    }
}

async fn post(router: axum::Router, body: Value) -> Value {
    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Submit a task while both stores are healthy and return its id.
async fn seed_task(h: &Harness) -> String {
    let sent = post(
        h.router.clone(),
        json!({
            "jsonrpc": "2.0", "id": "seed", "method": "message/send",
            "params": {
                "message": { "messageId": "m1", "role": "ROLE_USER", "parts": [{ "data": {} }] },
                "metadata": { "skillId": "test.echo" }
            }
        }),
    )
    .await;
    sent["result"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("seed failed: {sent}"))
        .to_string()
}

fn error_code(resp: &Value) -> i64 {
    resp["error"]["code"]
        .as_i64()
        .unwrap_or_else(|| panic!("expected a JSON-RPC error, got {resp}"))
}

#[tokio::test]
async fn tasks_get_reports_a_store_outage_as_internal_not_missing() {
    let h = harness();
    let task_id = seed_task(&h).await;
    h.tasks_down.store(true, Ordering::SeqCst);

    let resp = post(
        h.router.clone(),
        json!({"jsonrpc": "2.0", "id": 1, "method": "tasks/get", "params": {"id": task_id}}),
    )
    .await;
    assert_eq!(error_code(&resp), INTERNAL_ERROR, "got {resp}");
}

#[tokio::test]
async fn a_missing_task_stays_distinguishable_from_a_store_outage() {
    // The whole point of the classification: these two must not collapse into
    // one code, or a caller cannot tell "resubmit" from "retry later".
    let h = harness();
    let seeded = seed_task(&h).await;

    let missing = post(
        h.router.clone(),
        json!({"jsonrpc": "2.0", "id": 1, "method": "tasks/get",
               "params": {"id": "00000000-0000-4000-8000-000000000000"}}),
    )
    .await;
    assert_eq!(error_code(&missing), TASK_NOT_FOUND, "got {missing}");

    h.tasks_down.store(true, Ordering::SeqCst);
    let outage = post(
        h.router.clone(),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tasks/get", "params": {"id": seeded}}),
    )
    .await;
    assert_eq!(error_code(&outage), INTERNAL_ERROR, "got {outage}");
}

#[tokio::test]
async fn tasks_list_reports_a_store_outage_instead_of_an_empty_list() {
    // `[]` reads as "you have no tasks" — the caller's own tasks silently
    // vanish rather than the failure surfacing.
    let h = harness();
    seed_task(&h).await;
    h.tasks_down.store(true, Ordering::SeqCst);

    let resp = post(
        h.router.clone(),
        json!({"jsonrpc": "2.0", "id": 1, "method": "ListTasks", "params": {}}),
    )
    .await;
    assert!(
        resp["result"].is_null(),
        "a store outage must not answer with a task list: {resp}"
    );
    assert_eq!(error_code(&resp), INTERNAL_ERROR, "got {resp}");
}

#[tokio::test]
async fn tasks_cancel_reports_a_store_outage_as_internal() {
    let h = harness();
    let task_id = seed_task(&h).await;
    h.tasks_down.store(true, Ordering::SeqCst);

    let resp = post(
        h.router.clone(),
        json!({"jsonrpc": "2.0", "id": 1, "method": "tasks/cancel", "params": {"id": task_id}}),
    )
    .await;
    assert_eq!(error_code(&resp), INTERNAL_ERROR, "got {resp}");
}

#[tokio::test]
async fn push_config_set_reports_a_store_outage() {
    let h = harness();
    let task_id = seed_task(&h).await;
    h.push_down.store(true, Ordering::SeqCst);

    let resp = post(
        h.router.clone(),
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "tasks/pushNotificationConfig/set",
            "params": {"id": task_id, "pushNotificationConfig": {"url": "https://hook.example/x"}}
        }),
    )
    .await;
    assert_eq!(error_code(&resp), INTERNAL_ERROR, "got {resp}");
}

#[tokio::test]
async fn push_config_get_reports_a_store_outage_as_internal_not_missing() {
    // Reported `-32001 "Push notification config not found"` for an outage,
    // while `set` reported `-32603` for the same one — two contradictory
    // signals about a single failure.
    let h = harness();
    let task_id = seed_task(&h).await;
    let set = post(
        h.router.clone(),
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "tasks/pushNotificationConfig/set",
            "params": {"id": task_id, "pushNotificationConfig": {"url": "https://hook.example/x"}}
        }),
    )
    .await;
    assert!(set["error"].is_null(), "set failed: {set}");

    h.push_down.store(true, Ordering::SeqCst);
    let resp = post(
        h.router.clone(),
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "tasks/pushNotificationConfig/get",
            "params": {"id": task_id}
        }),
    )
    .await;
    assert_eq!(error_code(&resp), INTERNAL_ERROR, "got {resp}");
}

#[tokio::test]
async fn push_config_delete_fails_loudly_instead_of_reporting_success() {
    // This answered `result: null` while the config stayed live and kept
    // receiving deliveries — a caller revoking a leaked webhook was told it had
    // worked. Revocation must never report a success it did not achieve.
    let h = harness();
    let task_id = seed_task(&h).await;
    let set = post(
        h.router.clone(),
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "tasks/pushNotificationConfig/set",
            "params": {"id": task_id, "pushNotificationConfig": {"url": "https://hook.example/x"}}
        }),
    )
    .await;
    assert!(set["error"].is_null(), "set failed: {set}");

    h.push_down.store(true, Ordering::SeqCst);
    let resp = post(
        h.router.clone(),
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "tasks/pushNotificationConfig/delete",
            "params": {"id": task_id}
        }),
    )
    .await;
    assert!(
        !resp["result"].is_null() || !resp["error"].is_null(),
        "must not answer with a bare success: {resp}"
    );
    assert_eq!(error_code(&resp), INTERNAL_ERROR, "got {resp}");

    // And the config really is still there once the backend recovers, which is
    // exactly why reporting success was wrong.
    h.push_down.store(false, Ordering::SeqCst);
    let still_there = post(
        h.router.clone(),
        json!({
            "jsonrpc": "2.0", "id": 3, "method": "tasks/pushNotificationConfig/get",
            "params": {"id": task_id}
        }),
    )
    .await;
    assert_eq!(
        still_there["result"]["pushNotificationConfig"]["url"],
        "https://hook.example/x"
    );
}

/// Start a slow task and return its id once it has been persisted as
/// non-terminal, so `tasks/cancel` reaches the write instead of stopping at the
/// terminal-state guard.
async fn in_flight_task_id(h: &Harness) -> String {
    let router = h.router.clone();
    tokio::spawn(async move {
        post(
            router,
            json!({
                "jsonrpc": "2.0", "id": "slow", "method": "message/send",
                "params": {
                    "message": { "messageId": "m1", "role": "ROLE_USER", "parts": [{ "data": {} }] },
                    "metadata": { "skillId": "test.slow" }
                }
            }),
        )
        .await;
    });
    for _ in 0..100 {
        let listed = post(
            h.router.clone(),
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

#[tokio::test]
async fn tasks_cancel_does_not_claim_success_when_the_write_failed() {
    // Cancelling *is* the write that records CANCELED. Answering with a
    // CANCELED task while the store still holds the old state would tell the
    // caller the task is cancelled and leave `tasks/get` contradicting it.
    // Distinct from the read-failure case above: here the lookup succeeds, the
    // task really is cancelable, and only the persist fails.
    let h = harness();
    let task_id = in_flight_task_id(&h).await;
    h.task_writes_down.store(true, Ordering::SeqCst);

    let resp = post(
        h.router.clone(),
        json!({"jsonrpc": "2.0", "id": 1, "method": "tasks/cancel", "params": {"id": task_id}}),
    )
    .await;
    assert!(
        resp["result"].is_null(),
        "must not answer with a cancelled task it failed to store: {resp}"
    );
    assert_eq!(error_code(&resp), INTERNAL_ERROR, "got {resp}");
}

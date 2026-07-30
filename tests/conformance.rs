//! Cross-language conformance runners (Algorithm A01).
//!
//! Loads the shared fixtures from the apcore-a2a spec repo
//! (`$APCORE_A2A_SPEC_REPO` or the sibling `../apcore-a2a` checkout) and drives
//! them through this crate's public API. When the fixtures are absent each test
//! returns early (the spec repo is not vendored into the SDK repo).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use apcore::context::Context;
use apcore::errors::{ErrorCode, ModuleError};
use apcore::module::Module;
use apcore::registry::registry::Registry;
use apcore_a2a::adapters::agent_card::AgentCapabilities;
use apcore_a2a::auth::protocol::Authenticator;
use apcore_a2a::types::Part;
use apcore_a2a::{
    build_app, APCoreA2AConfig, AgentCardBuilder, BackendSource, ErrorMapper, JWTAuthenticator,
    PartConverter, SchemaConverter, SkillMapper,
};
use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::Request;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde_json::{json, Value};
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// Fixture loading + partial matching
// ---------------------------------------------------------------------------

fn spec_root() -> PathBuf {
    if let Ok(p) = std::env::var("APCORE_A2A_SPEC_REPO") {
        return PathBuf::from(p);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate dir has a parent")
        .join("apcore-a2a")
}

fn load_fixture(name: &str) -> Option<Value> {
    let path = spec_root().join("conformance/fixtures").join(name);
    if !path.is_file() {
        eprintln!(
            "conformance fixture not found, skipping: {}",
            path.display()
        );
        return None;
    }
    Some(serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap())
}

/// Deep partial match: every key/value in `expected` must appear in `actual`.
fn partial_match(expected: &Value, actual: &Value, path: &str) -> Result<(), String> {
    match expected {
        Value::Object(map) => {
            let obj = actual
                .as_object()
                .ok_or_else(|| format!("{path}: expected object, got {actual}"))?;
            for (k, v) in map {
                let sub = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                let av = obj.get(k).ok_or_else(|| format!("{sub}: missing key"))?;
                partial_match(v, av, &sub)?;
            }
            Ok(())
        }
        Value::Array(arr) => {
            let aarr = actual
                .as_array()
                .ok_or_else(|| format!("{path}: expected array, got {actual}"))?;
            if aarr.len() < arr.len() {
                return Err(format!(
                    "{path}: expected >= {} items, got {}",
                    arr.len(),
                    aarr.len()
                ));
            }
            for (i, v) in arr.iter().enumerate() {
                partial_match(v, &aarr[i], &format!("{path}[{i}]"))?;
            }
            Ok(())
        }
        _ => {
            if expected == actual {
                Ok(())
            } else {
                Err(format!("{path}: expected {expected}, got {actual}"))
            }
        }
    }
}

fn arr<'a>(v: &'a Value, key: &str) -> &'a Vec<Value> {
    static EMPTY: Vec<Value> = Vec::new();
    v.get(key).and_then(Value::as_array).unwrap_or(&EMPTY)
}

fn str_list(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Test modules + HTTP harness
// ---------------------------------------------------------------------------

struct EchoModule {
    description: String,
    schema: Value,
}

#[async_trait]
impl Module for EchoModule {
    fn input_schema(&self) -> Value {
        self.schema.clone()
    }
    fn output_schema(&self) -> Value {
        json!({ "type": "object" })
    }
    fn description(&self) -> &str {
        &self.description
    }
    async fn execute(&self, inputs: Value, _ctx: &Context<Value>) -> Result<Value, ModuleError> {
        Ok(inputs)
    }
}

struct FailModule;

#[async_trait]
impl Module for FailModule {
    fn input_schema(&self) -> Value {
        json!({ "type": "object" })
    }
    fn output_schema(&self) -> Value {
        json!({ "type": "object" })
    }
    fn description(&self) -> &str {
        "Always fails"
    }
    async fn execute(&self, _inputs: Value, _ctx: &Context<Value>) -> Result<Value, ModuleError> {
        // The raw message must never reach the client (sanitized to a fixed string).
        Err(ModuleError::new(
            ErrorCode::GeneralInternalError,
            "boom at 10.0.0.5",
        ))
    }
}

async fn router_with(modules: Vec<(&str, Box<dyn Module>)>) -> axum::Router {
    let registry = Registry::new();
    for (id, m) in modules {
        registry.register_module(id, m).expect("register module");
    }
    let (router, _card) = build_app(
        BackendSource::Registry(Arc::new(registry)),
        APCoreA2AConfig::default(),
    )
    .await
    .expect("build app");
    router
}

fn echo(desc: &str) -> Box<dyn Module> {
    Box::new(EchoModule {
        description: desc.into(),
        schema: json!({ "type": "object" }),
    })
}

async fn post_rpc(router: axum::Router, body: Value) -> Value {
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

async fn collect_sse(router: axum::Router, body: Value) -> Vec<Value> {
    let req = Request::builder()
        .method("POST")
        .uri("/")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    String::from_utf8(bytes.to_vec())
        .unwrap()
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
        // Each frame is a JSON-RPC response carrying the A2A event as `result`
        // (matching a2a-python / a2a-js); the fixtures describe the event.
        .map(|frame| frame["result"].clone())
        .collect()
}

// ---------------------------------------------------------------------------
// A-ERR: error mapping
// ---------------------------------------------------------------------------

#[test]
fn conformance_error_mapping() {
    let Some(fixture) = load_fixture("error_mapping.json") else {
        return;
    };
    for case in arr(&fixture, "error_cases") {
        let id = case["id"].as_str().unwrap();
        let spec = &case["input"];
        let name = spec["exception"].as_str().unwrap();
        let (code, message) = match name {
            "ModuleNotFoundError" => (ErrorCode::ModuleNotFound, "module not found".to_string()),
            "SchemaValidationError" => {
                let fields: Vec<String> = spec["errors"]
                    .as_object()
                    .map(|m| m.iter().map(|(k, v)| format!("{k}: {v}")).collect())
                    .unwrap_or_default();
                (ErrorCode::SchemaValidationError, fields.join("; "))
            }
            "ACLDeniedError" => (
                ErrorCode::ACLDenied,
                format!("caller {} module {}", spec["caller"], spec["module"]),
            ),
            // ModuleExecuteError and unknown exceptions both hit the catch-all arm.
            _ => (
                ErrorCode::GeneralInternalError,
                spec["message"].as_str().unwrap_or("boom").to_string(),
            ),
        };
        let rpc = ErrorMapper::to_jsonrpc_error(&ModuleError::new(code, &message));
        assert_eq!(
            rpc.code,
            case["expected_error_code"].as_i64().unwrap() as i32,
            "[{id}] code"
        );
        for needle in str_list(&case["expected_error_message"]) {
            assert!(
                rpc.message.contains(&needle),
                "[{id}] missing {needle:?} in {:?}",
                rpc.message
            );
        }
        for forbidden in str_list(&case["expected_message_excludes"]) {
            assert!(
                !rpc.message.contains(&forbidden),
                "[{id}] leaked {forbidden:?} in {:?}",
                rpc.message
            );
        }
    }
}

// ---------------------------------------------------------------------------
// A-PART: part conversion
// ---------------------------------------------------------------------------

fn build_part(spec: &Value) -> Part {
    if let Some(t) = spec.get("text") {
        Part::Text {
            text: t.as_str().unwrap().to_string(),
        }
    } else if let Some(d) = spec.get("data") {
        Part::Data { data: d.clone() }
    } else if let Some(u) = spec.get("url") {
        Part::FileUrl {
            url: u.as_str().unwrap().to_string(),
        }
    } else {
        panic!("unrecognized part spec: {spec}");
    }
}

#[test]
fn conformance_part_conversion() {
    let Some(fixture) = load_fixture("part_conversion.json") else {
        return;
    };
    let conv = PartConverter::new(SchemaConverter::new());

    for case in arr(&fixture, "test_cases") {
        let id = case["id"].as_str().unwrap();
        if case["direction"] == json!("output_to_parts") {
            let artifact =
                conv.output_to_parts(&case["input"], case["task_id"].as_str().unwrap_or(""));
            let actual = serde_json::to_value(&artifact).unwrap();
            if let Some(expected_id) = case["expected_artifact_id"].as_str() {
                assert_eq!(
                    actual["artifactId"],
                    json!(expected_id),
                    "[{id}] artifactId"
                );
            }
            partial_match(
                &case["expected_parts"],
                &actual["parts"],
                &format!("{id}.parts"),
            )
            .unwrap();
        } else {
            let parts: Vec<Part> = arr(case, "parts").iter().map(build_part).collect();
            let schema = case.get("input_schema");
            let result = conv
                .parts_to_input(&parts, schema)
                .expect("parts_to_input ok");
            partial_match(&case["expected_input"], &result, id).unwrap();
        }
    }

    for case in arr(&fixture, "error_cases") {
        let id = case["id"].as_str().unwrap();
        let parts: Vec<Part> = arr(case, "parts").iter().map(build_part).collect();
        let err = conv
            .parts_to_input(&parts, case.get("input_schema"))
            .expect_err(&format!("[{id}] expected an error"));
        for needle in str_list(&case["expected_error_message"]) {
            assert!(
                err.contains(&needle),
                "[{id}] missing {needle:?} in {err:?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// A-AUTH: jwt claim coercion
// ---------------------------------------------------------------------------

fn sign(claims: &Value, secret: &str) -> String {
    encode(
        &Header::new(Algorithm::HS256),
        claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .unwrap()
}

fn headers_for(case: &Value, default_secret: &str) -> HashMap<String, String> {
    if let Some(h) = case.get("headers").and_then(Value::as_object) {
        return h
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
            .collect();
    }
    let mut claims = case["claims"].clone();
    if let Some(offset) = claims.get("exp_offset_seconds").and_then(Value::as_i64) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        claims.as_object_mut().unwrap().remove("exp_offset_seconds");
        claims
            .as_object_mut()
            .unwrap()
            .insert("exp".into(), json!(now + offset));
    }
    let secret = case
        .get("sign_with_secret")
        .and_then(Value::as_str)
        .unwrap_or(default_secret);
    let mut headers = HashMap::new();
    headers.insert(
        "authorization".to_string(),
        format!("Bearer {}", sign(&claims, secret)),
    );
    headers
}

#[tokio::test]
async fn conformance_jwt_claim_coercion() {
    let Some(fixture) = load_fixture("jwt_claim_coercion.json") else {
        return;
    };
    let secret = fixture["secret"].as_str().unwrap();
    let auth = JWTAuthenticator::new(secret);

    for case in arr(&fixture, "test_cases") {
        let id = case["id"].as_str().unwrap();
        let identity = auth
            .authenticate(&headers_for(case, secret))
            .await
            .unwrap_or_else(|| panic!("[{id}] expected an Identity, got None"));
        let exp = &case["expected_identity"];
        assert_eq!(identity.id(), exp["id"].as_str().unwrap(), "[{id}] id");
        assert_eq!(
            identity.identity_type(),
            exp["type"].as_str().unwrap(),
            "[{id}] type"
        );
        assert_eq!(
            identity.roles(),
            str_list(&exp["roles"]).as_slice(),
            "[{id}] roles"
        );
    }

    for case in arr(&fixture, "reject_cases") {
        let id = case["id"].as_str().unwrap();
        assert!(
            auth.authenticate(&headers_for(case, secret))
                .await
                .is_none(),
            "[{id}] expected rejection (None)"
        );
    }
}

// ---------------------------------------------------------------------------
// A-CARD: agent card builder wire shape
// ---------------------------------------------------------------------------

#[test]
fn conformance_agent_card() {
    let Some(fixture) = load_fixture("agent_card.json") else {
        return;
    };
    for case in arr(&fixture, "test_cases") {
        let id = case["id"].as_str().unwrap();
        let spec = &case["input"];
        let registry = Registry::new();
        for m in arr(spec, "modules") {
            registry
                .register_module(
                    m["module_id"].as_str().unwrap(),
                    echo(m["description"].as_str().unwrap()),
                )
                .expect("register module");
        }
        let builder = AgentCardBuilder::new(SkillMapper::new());
        let card = builder.build(
            &registry,
            spec["name"].as_str().unwrap(),
            spec["description"].as_str().unwrap(),
            spec["version"].as_str().unwrap(),
            spec["url"].as_str().unwrap(),
            AgentCapabilities {
                streaming: false,
                push_notifications: false,
                extensions: vec![],
                extended_agent_card: false,
            },
            spec.get("security_schemes_input").cloned(),
        );
        let actual = serde_json::to_value(&card).unwrap();

        if let Some(expected) = case.get("expected_card") {
            partial_match(expected, &actual, id).unwrap();
        }
        for absent in str_list(&case["expected_card_absent_keys"]) {
            assert!(
                actual.get(&absent).is_none(),
                "[{id}] unexpected key {absent:?}"
            );
        }
        if let Some(n) = case["expected_skill_count"].as_u64() {
            assert_eq!(arr(&actual, "skills").len() as u64, n, "[{id}] skill count");
        }
        if case["expected_security_requirements_empty"] == json!(true) {
            assert!(
                arr(&actual, "securityRequirements").is_empty(),
                "[{id}] securityRequirements"
            );
        }
        if case["expected_security_schemes_empty"] == json!(true) {
            let empty = actual["securitySchemes"]
                .as_object()
                .map(|o| o.is_empty())
                .unwrap_or(true);
            assert!(empty, "[{id}] securitySchemes not empty");
        }
    }
}

// ---------------------------------------------------------------------------
// A-SKILL: skill resolution (message/send over the HTTP handler)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn conformance_skill_resolution() {
    let Some(fixture) = load_fixture("skill_resolution.json") else {
        return;
    };

    for case in arr(&fixture, "test_cases") {
        let id = case["id"].as_str().unwrap();
        let params = &case["params"];
        let router = router_with(vec![("math.add", echo("Adds"))]).await;
        let resp = post_rpc(
            router,
            json!({ "jsonrpc": "2.0", "id": "1", "method": "message/send", "params": params }),
        )
        .await;
        let result = &resp["result"];
        assert_eq!(
            result["status"]["state"], case["expected_task_state"],
            "[{id}] state; resp={resp}"
        );
        let serialized = result.to_string();
        for needle in str_list(&case["expected_status_message"]) {
            assert!(
                serialized.contains(&needle),
                "[{id}] missing {needle:?} in {serialized}"
            );
        }
    }

    for case in arr(&fixture, "error_cases") {
        let id = case["id"].as_str().unwrap();
        let router = router_with(vec![("math.add", echo("Adds"))]).await;
        let resp = post_rpc(
            router,
            json!({ "jsonrpc": "2.0", "id": "1", "method": "message/send", "params": case["params"] }),
        )
        .await;
        assert_eq!(
            resp["error"]["code"].as_i64(),
            case["expected_error_code"].as_i64(),
            "[{id}] error code; resp={resp}"
        );
    }
}

// ---------------------------------------------------------------------------
// A-STREAM: streaming events (message/stream over the HTTP handler)
// ---------------------------------------------------------------------------

fn stream_state(event: &Value, kind: &str) -> Option<String> {
    event
        .get(kind)
        .and_then(|e| e["status"]["state"].as_str())
        .map(String::from)
}

#[tokio::test]
async fn conformance_streaming_events() {
    let Some(fixture) = load_fixture("streaming_events.json") else {
        return;
    };

    for case in arr(&fixture, "test_cases") {
        let id = case["id"].as_str().unwrap();
        // The Rust runner cannot cheaply simulate a module timeout in-process.
        if case["optional"] == json!(true) {
            continue;
        }
        let skill = case["params"]["metadata"]["skillId"].as_str().unwrap();
        let module: Box<dyn Module> = if case.get("module_error").is_some() {
            Box::new(FailModule)
        } else {
            echo("Streams")
        };
        let router = router_with(vec![(skill, module)]).await;
        let events = collect_sse(
            router,
            json!({ "jsonrpc": "2.0", "id": "s1", "method": "message/stream", "params": case["params"] }),
        )
        .await;

        // Start state (carried by a task or statusUpdate event).
        if let Some(start) = case["expected_start_state"].as_str() {
            let found = events.iter().any(|e| {
                stream_state(e, "statusUpdate").as_deref() == Some(start)
                    || stream_state(e, "task").as_deref() == Some(start)
            });
            assert!(found, "[{id}] start state {start} not found in {events:?}");
        }

        let markers: Vec<&Value> = events
            .iter()
            .filter(|e| e["artifactUpdate"]["lastChunk"] == json!(true))
            .collect();
        let non_marker = events
            .iter()
            .filter(|e| {
                e.get("artifactUpdate").is_some() && e["artifactUpdate"]["lastChunk"] != json!(true)
            })
            .count();

        if let Some(min) = case["expected_min_artifact_updates"].as_u64() {
            assert!(
                non_marker as u64 >= min,
                "[{id}] artifact count {non_marker} < {min}: {events:?}"
            );
        }
        match case["expected_terminal_marker"].as_bool() {
            Some(true) => {
                let marker = markers
                    .last()
                    .unwrap_or_else(|| panic!("[{id}] no terminal marker: {events:?}"));
                assert_eq!(
                    marker["artifactUpdate"]["artifact"]["parts"],
                    json!([]),
                    "[{id}] marker parts"
                );
            }
            Some(false) => assert!(markers.is_empty(), "[{id}] unexpected marker: {events:?}"),
            None => {}
        }

        let final_state = events
            .iter()
            .filter_map(|e| stream_state(e, "statusUpdate"))
            .next_back();
        assert_eq!(
            final_state.as_deref(),
            case["expected_final_state"].as_str(),
            "[{id}] final state; events={events:?}"
        );

        let failed_msg = events
            .iter()
            .filter(|e| stream_state(e, "statusUpdate").as_deref() == Some("TASK_STATE_FAILED"))
            .find_map(|e| e["statusUpdate"]["status"]["message"]["parts"][0]["text"].as_str())
            .unwrap_or("");
        for needle in str_list(&case["expected_status_message"]) {
            assert!(
                failed_msg.contains(&needle),
                "[{id}] missing {needle:?} in {failed_msg:?}"
            );
        }
        for forbidden in str_list(&case["expected_status_message_excludes"]) {
            assert!(
                !failed_msg.contains(&forbidden),
                "[{id}] leaked {forbidden:?} in {failed_msg:?}"
            );
        }
    }
}

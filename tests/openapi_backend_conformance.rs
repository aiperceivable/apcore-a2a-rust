//! Conformance — Algorithm A-OAS: OpenAPI backend parity (feature F-12).
//!
//! Fixture: `conformance/fixtures/openapi_backend.json`, shared verbatim with
//! the Python and TypeScript runners. Each case builds a `Registry` through
//! [`apcore_a2a::openapi_backend::openapi_backend`] and asserts the registered
//! module set, the repaired descriptions, the emitted diagnostics and the
//! resulting Agent Card.
//!
//! The scanner's own derivation — including the normalisation of every module
//! ID into apcore's Canonical ID alphabet (apcore-toolkit >= 0.13.0) — is pinned
//! by apcore-toolkit's 33-case corpus, not here. What this driver checks is
//! everything the binding adds on top.
//!
//! Unlike apcore-mcp's Rust driver — which asserts no diagnostic at all,
//! because it has no log capture — every warning and INFO line the fixture
//! names is asserted here through the same thread-local `tracing` capture layer
//! `tests/integration.rs` uses for srs FR-AGC-007. Two of the fixture's cases
//! (`projection_unprojectable_segment_dropped_with_warning` and
//! `write_warning_not_suppressed_by_permissive_acl`) assert *only* a
//! diagnostic, so without the capture they would be vacuous.

#![cfg(feature = "openapi")]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use apcore::acl::{ACLRule, ApprovalRequirement, ACL};
use apcore::config::Config;
use apcore::context::Context;
use apcore::errors::ModuleError;
use apcore::executor::Executor;
use apcore::module::Module;
use apcore::registry::registry::Registry;
use apcore_a2a::adapters::agent_card::AgentCapabilities;
use apcore_a2a::openapi_backend::{openapi_backend, resolve_spec_location, OpenAPIBackendOptions};
use apcore_a2a::{
    build_app, build_app_with_auth, APCoreA2AConfig, AgentCardBuilder, BackendSource, SkillMapper,
};
use apcore_toolkit::openapi_scanner::TransformModuleHook;
use apcore_toolkit::types::ScannedModule;
use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::Request;
use serde_json::{json, Value};
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// Fixture loading
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

fn fixture() -> Option<Value> {
    let path = spec_root().join("conformance/fixtures/openapi_backend.json");
    if !path.is_file() {
        eprintln!(
            "conformance fixture not found, skipping: {}",
            path.display()
        );
        return None;
    }
    Some(serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap())
}

/// Every case in one fixture group.
///
/// **Panics on a missing or empty group rather than returning nothing.** An
/// `unwrap_or_default()` here makes the whole driver pass vacuously the moment a
/// group is renamed in the spec repo: the loop iterates zero cases and the test
/// reports success, which is the failure mode this suite exists to prevent.
/// Measured — renaming `test_cases` left this binary green at 9 passing tests
/// while asserting nothing, and apcore-a2a-python failed loudly on the same
/// mutation because it indexes the key directly.
fn cases(fixture: &Value, group: &str) -> Vec<Value> {
    let found = fixture
        .get(group)
        .and_then(Value::as_array)
        .unwrap_or_else(|| {
            panic!(
                "conformance fixture has no `{group}` array — the group was renamed or \
                 removed. Refusing to pass vacuously."
            )
        });
    assert!(
        !found.is_empty(),
        "conformance fixture group `{group}` is empty — refusing to pass vacuously."
    );
    found.clone()
}

fn case_id(case: &Value) -> &str {
    case["id"].as_str().unwrap_or("<unnamed>")
}

// ---------------------------------------------------------------------------
// Log capture — the thread-local pattern `tests/integration.rs` established
// ---------------------------------------------------------------------------

/// Every `tracing` message emitted while `f` runs, one per line, each prefixed
/// with its level (`WARN apcore-a2a: ...`).
///
/// `tracing::subscriber::set_default` is thread-local (not
/// `set_global_default`), so this cannot race the other tests in this binary,
/// and `#[tokio::test]` builds a current-thread runtime, so the emitting code
/// stays on the thread the guard covers.
///
/// The level prefix is what makes the diagnostic assertions real. The feature
/// spec states a *level* for each report — the illegal-ID skip at WARNING, the
/// description repair at INFO, a write failure at ERROR — and a capture that
/// records every level without saying which would pass for an implementation
/// that demoted all of them to `debug!`, where no operator will ever see them.
async fn captured_logs<F, T>(f: F) -> (T, String)
where
    F: std::future::Future<Output = T>,
{
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
            self.0
                .lock()
                .unwrap()
                .push(format!("{} {}", event.metadata().level(), message.0));
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

// ---------------------------------------------------------------------------
// Building a case
// ---------------------------------------------------------------------------

/// Options from a fixture case's `options` object.
///
/// `additional_backend_source` is a pseudo-option: it names the *situation*
/// (an extensions directory is also configured), not a keyword argument.
fn options_for(case: &Value) -> OpenAPIBackendOptions {
    let options = case.get("options").cloned().unwrap_or_else(|| json!({}));
    OpenAPIBackendOptions {
        // Forwarded, never defaulted. `no_base_url_anywhere_rejected` asserts the
        // failure when it is absent from both the options and the document, and
        // `base_url_option_supplies_what_the_document_lacks` asserts it is honoured
        // when the document has no `servers` — a key this mapping originally
        // omitted, which no case could detect until that second one existed.
        base_url: options
            .get("base_url")
            .and_then(Value::as_str)
            .map(String::from),
        prefix: options
            .get("prefix")
            .and_then(Value::as_str)
            .map(String::from),
        include: options
            .get("include")
            .and_then(Value::as_str)
            .map(String::from),
        exclude: options
            .get("exclude")
            .and_then(Value::as_str)
            .map(String::from),
        acknowledge_unapproved_writes: options
            .get("acknowledge_unapproved_writes")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        has_other_backend_source: options
            .get("additional_backend_source")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        transform_module: transform_module_hook_for(case),
        ..OpenAPIBackendOptions::new()
    }
}

/// The `transform_module` hook a case names under `hooks`, implemented here.
/// The fixture's notes define each one.
///
/// **Panics on a name this driver does not implement**, and on any other hook
/// key: ignoring it would silently run the hook-free path and pass a case that
/// asserts what a hook does.
fn transform_module_hook_for(case: &Value) -> Option<TransformModuleHook> {
    let hooks = case.get("hooks").and_then(Value::as_object)?;
    let id = case_id(case);
    for key in hooks.keys() {
        assert!(
            key == "transform_module",
            "{id}: the fixture names a hook this driver does not implement: {key:?}"
        );
    }
    let name = hooks.get("transform_module")?.as_str().unwrap_or_default();
    match name {
        "rename_to_mixed_case_id" => Some(Box::new(|mut module: ScannedModule| {
            module.module_id = "MyThing".to_string();
            Some(module)
        })),
        other => {
            panic!("{id}: the fixture names a transform_module hook this driver lacks: {other:?}")
        }
    }
}

async fn build(case: &Value, registry: Arc<Registry>) -> Result<Arc<Registry>, String> {
    openapi_backend(&case["document"], registry, options_for(case))
        .await
        .map_err(|e| e.to_string())
}

fn registry_ids(registry: &Registry) -> Vec<String> {
    let mut ids = registry.list(None, None, Some(&["public", "hidden"]));
    ids.sort();
    ids
}

fn str_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// test_cases — document -> registered modules
// ---------------------------------------------------------------------------

#[tokio::test]
async fn conformance_openapi_modules() {
    let Some(fixture) = fixture() else { return };

    for case in cases(&fixture, "test_cases") {
        let id = case_id(&case);
        let (result, logs) = captured_logs(build(&case, Arc::new(Registry::new()))).await;
        let registry = result.unwrap_or_else(|e| panic!("{id}: backend failed: {e}"));

        let expected = case["expected_modules"].as_array().cloned().unwrap();
        let mut want: Vec<String> = expected
            .iter()
            .map(|m| m["module_id"].as_str().unwrap().to_string())
            .collect();
        want.sort();
        assert_eq!(registry_ids(&registry), want, "{id}: registered module set");

        for module in &expected {
            let module_id = module["module_id"].as_str().unwrap();
            let definition = registry
                .get_definition(module_id)
                .expect("registry read")
                .unwrap_or_else(|| panic!("{id}: {module_id} not registered"));
            assert_eq!(
                definition.description,
                module["description"].as_str().unwrap(),
                "{id}: {module_id} description"
            );

            if let Some(annotations) = module.get("annotations").and_then(Value::as_object) {
                let got = definition
                    .annotations
                    .as_ref()
                    .unwrap_or_else(|| panic!("{id}: {module_id} has no annotations"));
                for (field, want) in annotations {
                    let want = want.as_bool().unwrap();
                    let actual = match field.as_str() {
                        "readonly" => got.readonly,
                        "destructive" => got.destructive,
                        "idempotent" => got.idempotent,
                        "open_world" => got.open_world,
                        "requires_approval" => got.requires_approval,
                        other => panic!("{id}: fixture names an unknown annotation {other:?}"),
                    };
                    assert_eq!(actual, want, "{id}: {module_id}.{field}");
                }
            }

            // The scanner's own per-module warning (e.g. the dedup rename) must
            // reach the operator: this binding re-emits them, at WARNING.
            if let Some(fragment) = module.get("warnings_contain").and_then(Value::as_str) {
                assert!(
                    at_level(&logs, "WARN")
                        .iter()
                        .any(|line| line.contains(fragment)),
                    "{id}: {module_id} warning {fragment:?} missing from the WARN lines in:\n{logs}"
                );
            }

            // FR-OAS-003: the INFO line names the EMITTED id — dedup suffix
            // included — because that is the one that reaches the Agent Card.
            // Naming any other id would send the operator looking for a skill
            // that does not exist — so the assertion is scoped to the synthesis
            // line itself. The whole log would not discriminate: apcore-toolkit's
            // writer may log its own "Registered HTTP proxy: <id>" line.
            let synthesis_report = synthesis_line(&logs);
            match module
                .get("description_was_synthesized")
                .and_then(Value::as_bool)
            {
                Some(true) => {
                    let line = synthesis_report
                        .unwrap_or_else(|| panic!("{id}: no synthesis report in:\n{logs}"));
                    assert!(
                        line.contains(module_id),
                        "{id}: the synthesis report names a different id than the one on the \
                         Agent Card ({module_id}): {line}"
                    );
                }
                Some(false) => assert!(
                    synthesis_report.is_none(),
                    "{id}: the repair fired for a module that already had a description:\n{logs}"
                ),
                None => {}
            }
        }

        // FR-OAS-003 report contents, scoped to the synthesis line itself.
        if let Some(report) = case.get("expected_synthesis_report") {
            let line = synthesis_line(&logs)
                .unwrap_or_else(|| panic!("{id}: no synthesis report in:\n{logs}"));
            for needle in str_list(report.get("contains")) {
                assert!(
                    line.contains(&needle),
                    "{id}: the synthesis report lacks {needle:?}: {line}"
                );
            }
            for needle in str_list(report.get("excludes")) {
                assert!(
                    !line.contains(&needle),
                    "{id}: the synthesis report names {needle:?}: {line}"
                );
            }
        }

        // Skipping an illegal ID before the writer, not leaving apcore's
        // registry to reject it: the rejection also leaves it unregistered, but
        // as a write-failure ERROR.
        if case
            .get("expected_no_error_logs")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let errors = at_level(&logs, "ERROR");
            assert!(
                errors.is_empty(),
                "{id}: expected no ERROR lines, got {errors:?}"
            );
        }

        // A skipped operation must be reported, naming the emitted ID and the
        // offending segment: an implementation that silently drops it fails here.
        for drop in case
            .get("expected_dropped")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            let derived = drop["derived_id"].as_str().unwrap();
            let segment = drop["offending_segment"].as_str().unwrap();
            // At WARNING, and naming BOTH the emitted ID and the offending
            // segment — an implementation that merely drops the module passes
            // every other assertion in this group and fails this one.
            let line = at_level(&logs, "WARN")
                .into_iter()
                .find(|line| line.contains(derived))
                .unwrap_or_else(|| {
                    panic!("{id}: no WARNING names the dropped operation {derived:?}:\n{logs}")
                });
            // The segment must be named in its own right. `2fa` is a substring
            // of `v1.2fa.get`, so a line carrying only the derived ID would
            // satisfy a naive `contains` — remove the ID first.
            let without_id = line.replacen(derived, "", 1);
            assert!(
                without_id.contains(segment),
                "{id}: the drop warning names the derived ID but not the offending segment \
                 {segment:?}: {line}"
            );
        }
        for substring in str_list(case.get("expected_warning_substrings")) {
            assert!(
                at_level(&logs, "WARN")
                    .iter()
                    .any(|line| line.contains(&substring)),
                "{id}: missing {substring:?} from the WARN lines in:\n{logs}"
            );
        }

        // The operation reaches the Agent Card, which is the whole point of the
        // FR-OAS-003 repair: AgentCardBuilder skips an empty-description module.
        let on_card = str_list(case.get("expected_on_agent_card"));
        if !on_card.is_empty() {
            let skills = card_skill_ids(&registry);
            for module_id in on_card {
                assert!(
                    skills.contains(&module_id),
                    "{id}: {module_id} is absent from the Agent Card: {skills:?}"
                );
            }
        }
    }
}

/// Captured lines emitted at `level` (`"WARN"`, `"INFO"`, `"ERROR"`).
fn at_level<'a>(logs: &'a str, level: &str) -> Vec<&'a str> {
    logs.lines()
        .filter(|line| line.starts_with(level))
        .collect()
}

/// The FR-OAS-003 report line, which the spec places at INFO.
fn synthesis_line(logs: &str) -> Option<&str> {
    at_level(logs, "INFO")
        .into_iter()
        .find(|line| line.contains("synthesized"))
}

/// Skill IDs the `AgentCardBuilder` publishes for `registry`.
fn card_skill_ids(registry: &Registry) -> Vec<String> {
    let card = AgentCardBuilder::new(SkillMapper::new()).build(
        registry,
        "test",
        "test agent",
        "1.0.0",
        "http://localhost:8000",
        AgentCapabilities {
            streaming: false,
            push_notifications: false,
            extensions: vec![],
            extended_agent_card: false,
        },
        None,
    );
    card.skills.iter().map(|s| s.id.clone()).collect()
}

// ---------------------------------------------------------------------------
// warning_cases — FR-OAS-005
// ---------------------------------------------------------------------------

#[tokio::test]
async fn conformance_openapi_unapproved_write_warning() {
    let Some(fixture) = fixture() else { return };

    for case in cases(&fixture, "warning_cases") {
        let id = case_id(&case);

        // When the case attaches an ACL, it is attached for real: the governance
        // state of a live Executor carrying that ACL is passed in, so
        // `acl_configured` and `builtin_acl_gate_wired` are both true here. That
        // is what makes `write_warning_not_suppressed_by_permissive_acl`
        // discriminating — an implementation gating the warning on
        // `acl_configured` sees a wired, fully permissive ACL and must warn anyway.
        let mut options = options_for(&case);
        if let Some(acl) = case.get("acl").filter(|v| !v.is_null()) {
            let mut executor = Executor::new(Arc::new(Registry::new()), Config::default());
            executor.set_acl(build_acl(acl));
            assert!(
                executor.governance_state().acl_configured,
                "{id}: precondition — the ACL must actually be attached"
            );
            options.governance_state = Some(executor.governance_state());
        }

        let (result, logs) = captured_logs(openapi_backend(
            &case["document"],
            Arc::new(Registry::new()),
            options,
        ))
        .await;
        result.unwrap_or_else(|e| panic!("{id}: backend failed: {e}"));

        // Asserted at WARNING specifically: a `debug!` that nobody will read is
        // not the report FR-OAS-005 requires.
        let warnings = at_level(&logs, "WARN").join("\n");
        let fired = warnings.contains("PUBLIC Agent Card");
        let expected = case["expect_warning"].as_bool().unwrap();
        assert_eq!(
            fired, expected,
            "{id}: expected warning={expected}:\n{logs}"
        );

        let lower = warnings.to_lowercase();
        for substring in str_list(case.get("expected_warning_substrings")) {
            assert!(
                lower.contains(&substring.to_lowercase()),
                "{id}: missing {substring:?} in:\n{warnings}"
            );
        }
    }
}

fn build_acl(spec: &Value) -> ACL {
    let rules: Vec<ACLRule> = spec
        .get("rules")
        .and_then(Value::as_array)
        .map(|rules| {
            rules
                .iter()
                .map(|raw| {
                    let mut rule = ACLRule::new(
                        str_list(raw.get("callers")),
                        str_list(raw.get("targets")),
                        raw["effect"].as_str().unwrap(),
                    );
                    if raw.get("approval").and_then(Value::as_str) == Some("required") {
                        rule.approval = Some(ApprovalRequirement::Required);
                    }
                    rule
                })
                .collect()
        })
        .unwrap_or_default();
    ACL::try_new(
        rules,
        spec.get("default_effect").and_then(Value::as_str).unwrap(),
        None,
    )
    .expect("fixture ACL is valid")
}

// ---------------------------------------------------------------------------
// config_cases — FR-OAS-004
// ---------------------------------------------------------------------------

#[tokio::test]
async fn conformance_openapi_spec_location() {
    let Some(fixture) = fixture() else { return };

    for case in cases(&fixture, "config_cases") {
        let id = case_id(&case);
        let project_root = case["project_root"].as_str().unwrap();
        let spec = case["spec_value"].as_str().unwrap();

        let (resolved, logs) = captured_logs(async {
            let first = resolve_spec_location(spec, Some(project_root));
            match (
                first,
                case.get("spec_value_next_tier").and_then(Value::as_str),
            ) {
                (None, Some(next)) => resolve_spec_location(next, Some(project_root)),
                (first, _) => first,
            }
        })
        .await;

        let expected = case["expected_resolved_spec"].as_str().unwrap();
        assert_eq!(resolved.as_deref(), Some(expected), "{id}");

        // The fixture's differing `cwd` is what makes these cases assert
        // anything. Rust cannot chdir without racing every other test in the
        // binary, so the same premise is checked the other way round: the
        // answer must not have been built from the process CWD.
        let cwd = std::env::current_dir().unwrap();
        assert_ne!(cwd, PathBuf::from(project_root), "{id}: precondition");
        assert!(
            !expected.starts_with(&cwd.to_string_lossy().into_owned()),
            "{id}: the expected answer is CWD-shaped, so this case discriminates nothing"
        );
        assert!(
            !resolved
                .as_deref()
                .unwrap()
                .starts_with(cwd.to_string_lossy().as_ref())
                || expected.starts_with(cwd.to_string_lossy().as_ref()),
            "{id}: resolved against the process CWD instead of project_root"
        );

        if let Some(substring) = case
            .get("expected_warning_substring")
            .and_then(Value::as_str)
        {
            assert!(
                at_level(&logs, "WARN")
                    .iter()
                    .any(|line| line.contains(substring)),
                "{id}: the discard must be reported at WARNING naming {substring:?}:\n{logs}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// card_cases — the public/extended Agent Card exposure
// ---------------------------------------------------------------------------

struct CardAuth;

#[async_trait]
impl apcore_a2a::Authenticator for CardAuth {
    async fn authenticate(
        &self,
        headers: &std::collections::HashMap<String, String>,
    ) -> Option<apcore::context::Identity> {
        match headers.get("authorization").map(String::as_str) {
            Some("Bearer good") => Some(apcore::context::Identity::new(
                "u1".into(),
                "test".into(),
                vec![],
                std::collections::HashMap::new(),
            )),
            _ => None,
        }
    }
    fn security_schemes(&self) -> Option<Value> {
        Some(json!({ "bearer": { "type": "http", "scheme": "bearer" } }))
    }
}

#[tokio::test]
async fn conformance_openapi_card_exposure() {
    let Some(fixture) = fixture() else { return };

    for case in cases(&fixture, "card_cases") {
        let id = case_id(&case);
        let registry = build(&case, Arc::new(Registry::new()))
            .await
            .unwrap_or_else(|e| panic!("{id}: backend failed: {e}"));

        let mut executor = Executor::new(registry.clone(), Config::default());
        if let Some(acl) = case.get("acl").filter(|v| !v.is_null()) {
            executor.set_acl(build_acl(acl));
        }
        let executor = Arc::new(executor);

        let (public_app, _) = build_app(
            BackendSource::Executor(executor.clone()),
            APCoreA2AConfig::default(),
        )
        .await
        .expect("build app");
        let mut public = skill_ids(public_app, "/.well-known/agent-card.json", None).await;
        public.sort();
        let mut want_public = str_list(case.get("expected_public_card_skills"));
        want_public.sort();
        assert_eq!(public, want_public, "{id}: public card");

        let (extended_app, _) = build_app_with_auth(
            BackendSource::Executor(executor),
            APCoreA2AConfig::default(),
            Some(Arc::new(CardAuth)),
        )
        .await
        .expect("build app with auth");
        let mut extended = skill_ids(
            extended_app,
            "/agent/authenticatedExtendedCard",
            Some("good"),
        )
        .await;
        extended.sort();
        let mut want_extended = str_list(case.get("expected_extended_card_skills"));
        want_extended.sort();
        assert_eq!(extended, want_extended, "{id}: extended card");
    }
}

async fn skill_ids(app: axum::Router, uri: &str, bearer: Option<&str>) -> Vec<String> {
    let mut builder = Request::builder().uri(uri);
    if let Some(token) = bearer {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let card: Value = serde_json::from_slice(&bytes).unwrap();
    card["skills"]
        .as_array()
        .map(|skills| {
            skills
                .iter()
                .filter_map(|s| s["id"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// error_cases
// ---------------------------------------------------------------------------

struct StubModule(String);

#[async_trait]
impl Module for StubModule {
    fn input_schema(&self) -> Value {
        json!({ "type": "object" })
    }
    fn output_schema(&self) -> Value {
        json!({ "type": "object" })
    }
    fn description(&self) -> &str {
        &self.0
    }
    async fn execute(&self, inputs: Value, _ctx: &Context<Value>) -> Result<Value, ModuleError> {
        Ok(inputs)
    }
}

#[tokio::test]
async fn conformance_openapi_errors() {
    let Some(fixture) = fixture() else { return };

    for case in cases(&fixture, "error_cases") {
        let id = case_id(&case);
        let registry = Arc::new(Registry::new());
        for module_id in str_list(case.get("preexisting_registry_module_ids")) {
            registry
                .register_module(
                    &module_id,
                    Box::new(StubModule(format!("stub {module_id}"))),
                )
                .expect("register stub");
        }

        let error = build(&case, registry.clone())
            .await
            .err()
            .unwrap_or_else(|| panic!("{id}: expected an error, got a populated registry"));

        for substring in str_list(case.get("expected_error_substrings")) {
            assert!(
                error.contains(&substring),
                "{id}: missing {substring:?} in {error:?}"
            );
        }

        if let Some(after) = case.get("expected_registry_module_ids_after") {
            let mut want = str_list(Some(after));
            want.sort();
            assert_eq!(
                registry_ids(&registry),
                want,
                "{id}: the preflight must leave the registry byte-for-byte unchanged"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Beyond the fixture
//
// Three options the shared corpus cannot reach. Every fixture case hands the
// backend an already-parsed document, so nothing in it can tell where `timeout`
// and `headers` are wired — and those two are exactly where apcore-mcp's Rust
// binding diverges from the spec (it applies `timeout` to the writer, i.e. the
// per-call proxy timeout, and drops `headers` on the floor). No case supplies
// `base_url` either: `no_base_url_anywhere_rejected` asserts only the failure.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn base_url_option_supplies_what_the_document_lacks() {
    // The mirror image of the fixture's `no_base_url_anywhere_rejected`: the
    // same document, which declares no `servers`, registers once the option
    // provides the host every proxied call would otherwise resolve against.
    let document = json!({
        "openapi": "3.0.3",
        "info": { "title": "NoServer", "version": "1.0.0" },
        "paths": { "/pets": { "get": {
            "operationId": "listPets", "summary": "List pets",
            "responses": { "200": { "description": "ok" } }
        } } }
    });

    let rejected = openapi_backend(
        &document,
        Arc::new(Registry::new()),
        OpenAPIBackendOptions::new(),
    )
    .await;
    assert!(
        rejected.is_err(),
        "precondition: the document has no servers"
    );

    let registry = openapi_backend(
        &document,
        Arc::new(Registry::new()),
        OpenAPIBackendOptions {
            base_url: Some("https://api.example.com".to_string()),
            ..OpenAPIBackendOptions::new()
        },
    )
    .await
    .expect("base_url must be honoured");
    assert_eq!(registry_ids(&registry), vec!["list_pets".to_string()]);
}

/// A minimal, valid OpenAPI 3.0 document, as an HTTP/1.1 response body.
const TINY_SPEC: &str = r#"{"openapi":"3.0.3","info":{"title":"T","version":"1.0.0"},
"servers":[{"url":"https://api.example.com"}],
"paths":{"/pets":{"get":{"operationId":"listPets","summary":"List pets",
"responses":{"200":{"description":"ok"}}}}}}"#;

#[tokio::test]
async fn timeout_is_the_spec_fetch_timeout() {
    use std::time::Duration;

    // Bound but never accepted: the kernel completes the handshake from the
    // backlog and no response ever arrives, so the only thing that can end this
    // request is the fetch timeout.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let options = OpenAPIBackendOptions {
        timeout_secs: 0.3,
        ..OpenAPIBackendOptions::new()
    };
    let spec = format!("http://{addr}/openapi.json");
    let call = apcore_a2a::openapi_backend::openapi_backend_from_spec(
        &spec,
        Arc::new(Registry::new()),
        options,
    );

    // 5s is far below apcore-toolkit's own 30s `LoadSpecOptions` default, so an
    // implementation that wires `timeout` to the writer instead of the fetch —
    // leaving the fetch at that default — fails here rather than passing slowly.
    let outcome = tokio::time::timeout(Duration::from_secs(5), call).await;
    let result = outcome.expect("the fetch must honour timeout_secs, not the toolkit default");
    let error = result.expect_err("a never-answering server must fail");
    assert!(
        !error.to_string().is_empty(),
        "the failure must name the resolved location"
    );
    drop(listener);
}

#[tokio::test]
async fn headers_are_sent_with_the_spec_fetch() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = vec![0_u8; 4096];
        let read = socket.read(&mut buffer).await.unwrap();
        let request = String::from_utf8_lossy(&buffer[..read]).to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{TINY_SPEC}",
            TINY_SPEC.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.flush().await.unwrap();
        request
    });

    let options = OpenAPIBackendOptions {
        headers: Some(std::collections::HashMap::from([(
            "X-Api-Key".to_string(),
            "spec-read-key".to_string(),
        )])),
        ..OpenAPIBackendOptions::new()
    };
    let registry = apcore_a2a::openapi_backend::openapi_backend_from_spec(
        &format!("http://{addr}/openapi.json"),
        Arc::new(Registry::new()),
        options,
    )
    .await
    .expect("fetch and scan");
    assert_eq!(registry_ids(&registry), vec!["list_pets".to_string()]);

    let request = server.await.unwrap().to_lowercase();
    assert!(
        request.contains("x-api-key: spec-read-key"),
        "the spec-fetch headers never reached the request:\n{request}"
    );
}

// ---------------------------------------------------------------------------
// FR-OAS-002 unit coverage — registry-legal IDs, and the deprecated projection
// ---------------------------------------------------------------------------

/// Deprecated — apcore-toolkit >= 0.13 emits IDs in apcore's alphabet and the
/// backend no longer calls it — but still public, including through the crate
/// root, so its behaviour stays pinned until the minor release that removes it.
#[test]
#[allow(deprecated)]
fn projection_alphabet() {
    for (raw, expected) in [
        ("listPets", Some("listpets")),
        ("pet-store.items.get", Some("pet_store.items.get")),
        ("already.legal", Some("already.legal")),
        ("v1.2fa.get", None),
        ("Users.UserId.Get", Some("users.userid.get")),
        ("9lives", None),
    ] {
        assert_eq!(
            apcore_a2a::project_module_id(raw).as_deref(),
            expected,
            "project_module_id({raw:?})"
        );
    }
}

/// Nothing internal may call the deprecated projection any more. It is the
/// identity on every ID apcore-toolkit >= 0.13 emits, so a call would be dead
/// work; where it was NOT a no-op — inside `transform_module`, before the
/// toolkit's final normalisation — it produced a different ID than the toolkit
/// (`MyThing` -> `mything`, not `my_thing`). Read from the source, outside its
/// own `#[cfg(test)]` module, since a free function cannot be intercepted.
#[test]
fn the_backend_never_calls_project_module_id() {
    let source = include_str!("../src/openapi_backend.rs");
    let production = source.split("#[cfg(test)]").next().unwrap();
    let calls = production.matches("project_module_id(").count();
    let definitions = production.matches("fn project_module_id(").count();
    assert_eq!(
        definitions, 1,
        "precondition: the definition is in the scanned text"
    );
    assert_eq!(
        calls - definitions,
        0,
        "project_module_id is called from production code"
    );
}

fn one_operation_document() -> Value {
    json!({
        "openapi": "3.0.3",
        "info": { "title": "Pets", "version": "1.0.0" },
        "servers": [{ "url": "https://api.example.com" }],
        "paths": { "/pets": { "get": {
            "operationId": "listPets", "summary": "List pets",
            "responses": { "200": { "description": "ok" } }
        } } }
    })
}

#[tokio::test]
async fn an_illegal_hook_returned_id_is_skipped_naming_its_segment() {
    // The skip applies to whatever the scanner emitted, hook output included. An
    // empty ID (only a hook can produce one) names the empty segment, as the
    // toolkit's own legality warning does.
    for (hook_id, segment) in [("", ""), ("v1.2fa", "2fa"), ("Ab.9x", "9x")] {
        let owned = hook_id.to_string();
        let options = OpenAPIBackendOptions {
            derive_module_id: Some(Box::new(move |_, _, _| Some(owned.clone()))),
            ..OpenAPIBackendOptions::new()
        };
        let (result, logs) = captured_logs(openapi_backend(
            &one_operation_document(),
            Arc::new(Registry::new()),
            options,
        ))
        .await;
        let registry = result.expect("a skipped module is not an error");
        assert!(
            registry_ids(&registry).is_empty(),
            "{hook_id:?}: registered"
        );
        let skips: Vec<&str> = at_level(&logs, "WARN")
            .into_iter()
            .filter(|l| l.contains("skipping OpenAPI operation"))
            .collect();
        assert_eq!(skips.len(), 1, "{hook_id:?}: {logs}");
        assert!(
            skips[0].contains(&format!("('{segment}')")),
            "{hook_id:?}: the skip must name segment {segment:?}: {}",
            skips[0]
        );
        assert!(at_level(&logs, "ERROR").is_empty(), "{hook_id:?}: {logs}");
    }
}

#[tokio::test]
async fn a_skipped_module_reaches_no_later_diagnostic() {
    // An undocumented `POST /v1/2fa` is the worst case: handed to the writer
    // instead, the synthesis report would count it, FR-OAS-005 would warn about a
    // write operation that is not on the card, and the zero-modules warning —
    // the only true statement — would not fire.
    let document = json!({
        "openapi": "3.0.3",
        "info": { "title": "t", "version": "1" },
        "servers": [{ "url": "https://api.example.com" }],
        "paths": { "/v1/2fa": { "post": { "responses": { "200": { "description": "ok" } } } } }
    });
    let (result, logs) = captured_logs(openapi_backend(
        &document,
        Arc::new(Registry::new()),
        OpenAPIBackendOptions::new(),
    ))
    .await;
    let registry = result.expect("a skipped module is not an error");
    assert!(registry_ids(&registry).is_empty());
    let warnings = at_level(&logs, "WARN").join("\n");
    assert!(
        warnings.contains("skipping OpenAPI operation 'v1.2fa.post'"),
        "{logs}"
    );
    assert!(warnings.contains("no registrable modules"), "{logs}");
    assert!(!warnings.contains("PUBLIC Agent Card"), "{logs}");
    assert!(synthesis_line(&logs).is_none(), "{logs}");
    assert!(at_level(&logs, "ERROR").is_empty(), "{logs}");
    // The toolkit's own legality warning is not re-emitted beside the skip line.
    assert!(
        !warnings.contains("is not a legal apcore module ID"),
        "{logs}"
    );
}

#[tokio::test]
async fn the_caller_hook_runs_before_normalisation_and_the_repair() {
    // The hook renames to a camelCase, hyphenated ID and clears the description.
    // apcore-toolkit >= 0.13 normalises the final ID after the hook
    // (`pet_store.list_pets`, words split), and the repair runs on what `scan`
    // returns — so the module registers, legal and described. A legality check
    // inside the hook would have skipped it; the retired in-hook projection would
    // have registered `pet_store.listpets`.
    let options = OpenAPIBackendOptions {
        transform_module: Some(Box::new(|mut module: ScannedModule| {
            module.module_id = "Pet-Store.ListPets".to_string();
            module.description = "   ".to_string();
            Some(module)
        })),
        ..OpenAPIBackendOptions::new()
    };
    let (result, logs) = captured_logs(openapi_backend(
        &one_operation_document(),
        Arc::new(Registry::new()),
        options,
    ))
    .await;
    let registry = result.expect("backend");
    assert_eq!(
        registry_ids(&registry),
        vec!["pet_store.list_pets".to_string()]
    );
    let definition = registry
        .get_definition("pet_store.list_pets")
        .expect("registry read")
        .expect("registered");
    assert_eq!(definition.description, "GET /pets");
    let line = synthesis_line(&logs).expect("a synthesis report");
    assert!(line.contains("pet_store.list_pets"), "{line}");
    assert!(!line.contains("Pet-Store.ListPets"), "{line}");
}

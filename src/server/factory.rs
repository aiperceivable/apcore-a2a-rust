//! A2AServerFactory — wires all components into an axum Router (A2A 1.0).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};

use apcore::registry::registry::Registry;

use crate::adapters::agent_card::{AgentCapabilities, AgentCard, AgentCardBuilder};
use crate::adapters::errors::register_a2a_error_formatter;
use crate::adapters::skill_mapper::SkillMapper;
use crate::auth::middleware::AuthMiddlewareLayer;
use crate::auth::protocol::Authenticator;
use crate::server::executor::ApCoreAgentExecutor;
use crate::server::handlers::{
    self, explorer_card as explorer_card_handler, explorer_html, jsonrpc_handler, AppState,
    AuthIdentity, FilteredCard,
};
use crate::storage::{InMemoryPushConfigStore, InMemoryTaskStore, PushConfigStore, TaskStore};

/// Everything [`A2AServerFactory::create`] needs to build a server.
///
/// A struct rather than a parameter list: `create` had grown to ten positional
/// arguments behind `#[allow(clippy::too_many_arguments)]`, and every new
/// component was another breaking signature change. Adding a field here is
/// breaking only for callers that build it with a struct literal, which they
/// can avoid via [`CreateOptions::new`] plus the `with_*` setters.
pub struct CreateOptions {
    pub executor: Arc<ApCoreAgentExecutor>,
    pub task_store: Arc<dyn TaskStore>,
    pub push_config_store: Arc<dyn PushConfigStore>,
    pub name: String,
    pub description: String,
    pub version: String,
    pub url: String,
    pub auth: Option<Arc<dyn Authenticator>>,
    pub explorer: bool,
    pub explorer_prefix: String,
}

impl CreateOptions {
    /// Options with the default in-memory stores, no authenticator and the
    /// Explorer disabled.
    pub fn new(
        executor: Arc<ApCoreAgentExecutor>,
        name: impl Into<String>,
        description: impl Into<String>,
        version: impl Into<String>,
        url: impl Into<String>,
    ) -> Self {
        Self {
            executor,
            task_store: Arc::new(InMemoryTaskStore::new()),
            push_config_store: Arc::new(InMemoryPushConfigStore::new()),
            name: name.into(),
            description: description.into(),
            version: version.into(),
            url: url.into(),
            auth: None,
            explorer: false,
            explorer_prefix: "/explorer".to_string(),
        }
    }

    /// Use a custom task store. See the [`TaskStore`] contract: a store that
    /// ignores its `CallContext` disables task isolation.
    pub fn with_task_store(mut self, task_store: Arc<dyn TaskStore>) -> Self {
        self.task_store = task_store;
        self
    }

    /// Use a custom push-notification config store. Pair it with a persistent
    /// [`TaskStore`], or a restart restores tasks whose webhook targets are
    /// gone.
    pub fn with_push_config_store(mut self, store: Arc<dyn PushConfigStore>) -> Self {
        self.push_config_store = store;
        self
    }

    /// Require authentication, and derive the card's security schemes from it.
    pub fn with_auth(mut self, auth: Arc<dyn Authenticator>) -> Self {
        self.auth = Some(auth);
        self
    }

    /// Serve the Explorer UI at `prefix`.
    pub fn with_explorer(mut self, enabled: bool, prefix: impl Into<String>) -> Self {
        self.explorer = enabled;
        self.explorer_prefix = prefix.into();
        self
    }
}

/// Factory for building the A2A 1.0 server application.
pub struct A2AServerFactory {
    _skill_mapper: SkillMapper,
    agent_card_builder: AgentCardBuilder,
}

impl Default for A2AServerFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl A2AServerFactory {
    pub fn new() -> Self {
        register_a2a_namespace();
        register_a2a_error_formatter();

        let skill_mapper = SkillMapper::new();
        let agent_card_builder = AgentCardBuilder::new(skill_mapper.clone());
        Self {
            _skill_mapper: skill_mapper,
            agent_card_builder,
        }
    }

    pub fn create(&self, registry: &Registry, opts: CreateOptions) -> (Router, AgentCard) {
        let CreateOptions {
            executor,
            task_store,
            push_config_store,
            name,
            description,
            version,
            url,
            auth,
            explorer,
            explorer_prefix,
        } = opts;
        // Security schemes (and the extended-card flag) are derived from the
        // authenticator, if one is configured.
        let security_schemes = auth.as_ref().and_then(|a| a.security_schemes());
        let capabilities = AgentCapabilities {
            streaming: true,
            push_notifications: false,
            extensions: vec![],
            // Advertised only when this binding actually serves it
            // (srs FR-AGC-006). With an `Authenticator` configured, the
            // `GetExtendedAgentCard` method and the
            // `/agent/authenticatedExtendedCard` route are both wired below, so
            // the flag and the behaviour agree. Without one, the endpoint 404s
            // and this stays `false` — a client is entitled to read the flag and
            // call the method (A2A §3.2.x), so advertising an unserved
            // capability is worse than not advertising it.
            extended_agent_card: auth.is_some(),
        };

        let agent_card = self.agent_card_builder.build(
            registry,
            &name,
            &description,
            &version,
            &url,
            capabilities,
            security_schemes,
        );

        let card_value = serde_json::to_value(&agent_card).unwrap();

        // Skills whose module is annotated `requires_approval` are withheld from
        // the public card (srs FR-AGC-003) and restored on the extended one
        // (srs FR-AGC-004). Resolved once from the registry: an annotation
        // cannot change for the life of a descriptor.
        let approval_gated: HashSet<String> = agent_card
            .skills
            .iter()
            .filter(|skill| {
                registry
                    .get_definition(&skill.id)
                    .ok()
                    .flatten()
                    .and_then(|def| def.annotations)
                    .is_some_and(|ann| ann.requires_approval)
            })
            .map(|skill| skill.id.clone())
            .collect();

        // The public card is filtered for the anonymous principal once, here,
        // rather than per request on an auth-exempt route.
        let public_card_value = Arc::new(handlers::public_card(
            &card_value,
            &executor,
            &approval_gated,
        ));

        // The extended card carries the full skill set, filtered per
        // authenticated identity at request time. Absent without an
        // authenticator, matching `capabilities.extendedAgentCard`.
        let extended_card = auth
            .is_some()
            .then(|| Arc::new(FilteredCard::new(Arc::new(card_value.clone()))));

        // Explorer card: agent card enriched with per-skill input schemas.
        let explorer_card = {
            let mut card = card_value.clone();
            let mut schemas = serde_json::Map::new();
            for skill in &agent_card.skills {
                if let Some(def) = registry.get_definition(&skill.id).ok().flatten() {
                    if def.input_schema.is_object() {
                        schemas.insert(skill.id.clone(), def.input_schema);
                    }
                }
            }
            if !schemas.is_empty() {
                if let Some(obj) = card.as_object_mut() {
                    obj.insert("_inputSchemas".to_string(), Value::Object(schemas));
                }
            }
            Arc::new(FilteredCard::new(Arc::new(card)))
        };

        // Known skill (module) ids for request-time validation, and their input
        // schemas so an inbound TextPart can be parsed against the right shape.
        let module_ids = registry.list(None, None, None);
        let input_schemas: HashMap<String, Value> = module_ids
            .iter()
            .filter_map(|module_id| {
                let def = registry.get_definition(module_id).ok().flatten()?;
                def.input_schema
                    .is_object()
                    .then(|| (module_id.clone(), def.input_schema))
            })
            .collect();
        let skill_ids: HashSet<String> = module_ids.into_iter().collect();

        let state = AppState {
            executor,
            task_store,
            skill_ids: Arc::new(skill_ids),
            input_schemas: Arc::new(input_schemas),
            agent_card: public_card_value,
            extended_card,
            explorer_card,
            cancel_tokens: Arc::new(Mutex::new(HashMap::new())),
            push_config_store,
            http: reqwest::Client::new(),
        };

        let mut router = Router::new()
            .route("/", post(jsonrpc_handler))
            .route("/health", get(health))
            // A2A 1.0 primary discovery path + 0.3 compat alias.
            .route("/.well-known/agent-card.json", get(serve_card))
            .route("/.well-known/agent.json", get(serve_card))
            // srs FR-AGC-004. Deliberately absent from the auth-exempt list
            // below, so an unauthenticated request is rejected by the middleware
            // rather than by this handler.
            .route("/agent/authenticatedExtendedCard", get(serve_extended_card));

        // Explorer UI (opt-in) at the configured prefix.
        if explorer {
            let prefix = explorer_prefix.trim_end_matches('/');
            router = router
                .route(prefix, get(explorer_html))
                .route(&format!("{prefix}/agent-card"), get(explorer_card_handler));
        }

        let mut router = router.with_state(state);

        // Apply auth middleware (if configured). Discovery + health stay public.
        if let Some(authenticator) = auth {
            // Note: a `/metrics` endpoint is not yet implemented in the Rust SDK
            // (Python/TS-only), so it is intentionally absent from this list.
            let exempt = vec![
                "/.well-known/agent-card.json".to_string(),
                "/.well-known/agent.json".to_string(),
                "/health".to_string(),
            ];
            router = router.layer(AuthMiddlewareLayer::new(authenticator, exempt));
        }

        (router, agent_card)
    }
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "healthy" }))
}

async fn serve_card(State(state): State<AppState>) -> Json<Value> {
    handlers::public_agent_card(State(state)).await
}

/// `GET /agent/authenticatedExtendedCard` (srs FR-AGC-004).
///
/// 404 when no `Authenticator` is configured, which is the same condition under
/// which `capabilities.extendedAgentCard` is `false`.
async fn serve_extended_card(
    State(state): State<AppState>,
    AuthIdentity(identity): AuthIdentity,
) -> Response {
    match handlers::extended_agent_card(&state, identity.as_ref()) {
        Some(card) => Json(card).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Extended agent card is not configured" })),
        )
            .into_response(),
    }
}

fn register_a2a_namespace() {
    use apcore::config::{Config, EnvStyle, NamespaceRegistration};
    let _ = Config::register_namespace(NamespaceRegistration {
        name: "apcore-a2a".to_string(),
        env_prefix: Some("APCORE_A2A".to_string()),
        defaults: Some(json!({
            "execution_timeout": 300,
            "cors_origins": [],
            "explorer": false,
            "metrics": false,
            "push_notifications": false,
        })),
        schema: None,
        env_style: EnvStyle::default(),
        max_depth: 16,
        env_map: None,
    });
}

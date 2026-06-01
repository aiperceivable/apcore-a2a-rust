//! A2AServerFactory — wires all components into an axum Router (A2A 1.0).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use axum::{
    extract::State,
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
    explorer_card as explorer_card_handler, explorer_html, jsonrpc_handler, AppState,
};
use crate::storage::TaskStore;

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

    #[allow(clippy::too_many_arguments)]
    pub fn create(
        &self,
        registry: &Registry,
        executor: Arc<ApCoreAgentExecutor>,
        task_store: Arc<dyn TaskStore>,
        name: &str,
        description: &str,
        version: &str,
        url: &str,
        auth: Option<Arc<dyn Authenticator>>,
        explorer: bool,
        explorer_prefix: &str,
    ) -> (Router, AgentCard) {
        // Security schemes (and the extended-card flag) are derived from the
        // authenticator, if one is configured.
        let security_schemes = auth.as_ref().and_then(|a| a.security_schemes());
        let capabilities = AgentCapabilities {
            streaming: true,
            push_notifications: false,
            extensions: vec![],
            extended_agent_card: false,
        };

        let agent_card = self.agent_card_builder.build(
            registry,
            name,
            description,
            version,
            url,
            capabilities,
            security_schemes,
        );

        let card_value = serde_json::to_value(&agent_card).unwrap();

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
            Arc::new(card)
        };

        // Known skill (module) ids for request-time validation.
        let skill_ids: HashSet<String> = registry.list(None, None, None).into_iter().collect();

        let state = AppState {
            executor,
            task_store,
            skill_ids: Arc::new(skill_ids),
            agent_card: Arc::new(card_value),
            explorer_card,
            cancel_tokens: Arc::new(Mutex::new(HashMap::new())),
            push_configs: Arc::new(Mutex::new(HashMap::new())),
            http: reqwest::Client::new(),
        };

        let mut router = Router::new()
            .route("/", post(jsonrpc_handler))
            .route("/health", get(health))
            // A2A 1.0 primary discovery path + 0.3 compat alias.
            .route("/.well-known/agent-card.json", get(serve_card))
            .route("/.well-known/agent.json", get(serve_card));

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
            let exempt = vec![
                "/.well-known/agent-card.json".to_string(),
                "/.well-known/agent.json".to_string(),
                "/health".to_string(),
                "/metrics".to_string(),
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
    Json((*state.agent_card).clone())
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

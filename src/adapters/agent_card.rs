//! AgentCardBuilder — builds an A2A 1.0 Agent Card from an apcore Registry.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use apcore::registry::registry::Registry;

use super::skill_mapper::SkillMapper;

/// A2A 1.0 Agent Card.
///
/// The 0.3 top-level `url` is replaced by `supported_interfaces`; the protocol
/// version lives on each interface. `extended_agent_card` moved onto
/// `capabilities`; `security_requirements` / `signatures` / `provider` are
/// first-class.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCard {
    pub name: String,
    pub description: String,
    pub version: String,
    pub supported_interfaces: Vec<AgentInterface>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<Value>,
    pub capabilities: AgentCapabilities,
    pub skills: Vec<AgentSkill>,
    pub default_input_modes: Vec<String>,
    pub default_output_modes: Vec<String>,
    pub security_schemes: Value,
    pub security_requirements: Vec<Value>,
    pub signatures: Vec<Value>,
}

/// A2A 1.0 `AgentInterface` — one supported transport endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentInterface {
    pub url: String,
    pub protocol_binding: String,
    pub protocol_version: String,
    pub tenant: String,
}

/// A2A 1.0 `AgentCapabilities` (dropped 0.3 `stateTransitionHistory`; added
/// `extensions` and `extendedAgentCard`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    pub streaming: bool,
    pub push_notifications: bool,
    pub extensions: Vec<Value>,
    pub extended_agent_card: bool,
}

/// A2A 1.0 Agent Skill — derived from an apcore module descriptor.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSkill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,
    pub examples: Vec<String>,
    pub input_modes: Vec<String>,
    pub output_modes: Vec<String>,
    pub security_requirements: Vec<Value>,
}

/// Builds an [`AgentCard`] from an apcore Registry.
pub struct AgentCardBuilder {
    skill_mapper: SkillMapper,
    /// Cached card populated by [`get_cached_or_build`](Self::get_cached_or_build);
    /// interior mutability lets the builder cache behind a shared `&self`.
    cache: Mutex<Option<AgentCard>>,
}

impl AgentCardBuilder {
    pub fn new(skill_mapper: SkillMapper) -> Self {
        Self {
            skill_mapper,
            cache: Mutex::new(None),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn build(
        &self,
        registry: &Registry,
        name: &str,
        description: &str,
        version: &str,
        url: &str,
        capabilities: AgentCapabilities,
        security_schemes: Option<Value>,
    ) -> AgentCard {
        let module_ids = registry.list(None, None, None);
        let skills: Vec<AgentSkill> = module_ids
            .iter()
            .filter_map(|module_id| {
                // apcore 0.22: get_definition returns Result<Option<…>>.
                let def = registry.get_definition(module_id).ok().flatten()?;
                let desc = registry.describe(module_id);
                // Skip modules without a description (Python/TS `to_skill` → None,
                // and the spec excludes them from the Agent Card).
                if desc.trim().is_empty() {
                    tracing::warn!("Skipping module {module_id}: missing description");
                    return None;
                }
                Some(self.skill_mapper.to_skill(module_id, &def, &desc))
            })
            .collect();

        let card = AgentCard {
            name: name.to_string(),
            description: description.to_string(),
            version: version.to_string(),
            supported_interfaces: vec![AgentInterface {
                url: url.to_string(),
                protocol_binding: "JSONRPC".to_string(),
                protocol_version: "1.0".to_string(),
                tenant: String::new(),
            }],
            provider: None,
            capabilities: AgentCapabilities {
                extended_agent_card: security_schemes.is_some(),
                ..capabilities
            },
            skills,
            default_input_modes: vec!["text/plain".to_string(), "application/json".to_string()],
            default_output_modes: vec!["text/plain".to_string(), "application/json".to_string()],
            security_schemes: security_schemes.unwrap_or_else(|| Value::Object(Default::default())),
            security_requirements: vec![],
            signatures: vec![],
        };

        // Populate the cache (Python/TS parity): a direct build() makes the card
        // available to a subsequent get_cached_or_build(). The lock is taken only
        // here, after the (potentially slow) card construction completes, so it is
        // never held across the build work.
        *self.cache.lock().unwrap() = Some(card.clone());
        card
    }

    /// Build the extended Agent Card (Python/TS parity).
    ///
    /// The extended card is a deep clone of `base_card`. apcore exposes no extra
    /// authenticated-only fields today, so this mirrors the base card; the method
    /// exists for cross-language API parity and future enrichment.
    pub fn build_extended(&self, base_card: &AgentCard) -> AgentCard {
        base_card.clone()
    }

    /// Return the cached Agent Card if present, otherwise build it, cache it, and
    /// return the freshly built card (Python/TS parity).
    #[allow(clippy::too_many_arguments)]
    pub fn get_cached_or_build(
        &self,
        registry: &Registry,
        name: &str,
        description: &str,
        version: &str,
        url: &str,
        capabilities: AgentCapabilities,
        security_schemes: Option<Value>,
    ) -> AgentCard {
        // Check the cache first; the lock is released before delegating to
        // build() (which does its own locking) to avoid holding the lock across
        // the build call.
        if let Some(cached) = self.cache.lock().unwrap().clone() {
            return cached;
        }
        // build() caches the freshly built card itself.
        self.build(
            registry,
            name,
            description,
            version,
            url,
            capabilities,
            security_schemes,
        )
    }

    /// Invalidate the cached Agent Card (Python/TS parity).
    pub fn invalidate_cache(&self) {
        *self.cache.lock().unwrap() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apcore::context::Context;
    use apcore::errors::ModuleError;
    use apcore::module::Module;
    use apcore::registry::registry::Registry;
    use async_trait::async_trait;
    use serde_json::json;

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
        async fn execute(
            &self,
            inputs: Value,
            _ctx: &Context<Value>,
        ) -> Result<Value, ModuleError> {
            Ok(inputs)
        }
    }

    fn echo_registry() -> Registry {
        let registry = Registry::new();
        registry
            .register_module("test.echo", Box::new(EchoModule))
            .expect("register echo module");
        registry
    }

    fn capabilities() -> AgentCapabilities {
        AgentCapabilities {
            streaming: true,
            push_notifications: false,
            extensions: vec![],
            extended_agent_card: false,
        }
    }

    fn build_card(builder: &AgentCardBuilder, registry: &Registry) -> AgentCard {
        builder.build(
            registry,
            "Agent",
            "An agent",
            "1.0.0",
            "http://localhost",
            capabilities(),
            None,
        )
    }

    #[test]
    fn build_populates_cache_for_subsequent_get() {
        // Regression (A-D-018): a direct build() must populate the cache so a
        // following get_cached_or_build() returns that same cached card, rather
        // than rebuilding (Python/TS parity).
        let registry = echo_registry();
        let builder = AgentCardBuilder::new(SkillMapper::new());

        let built = build_card(&builder, &registry);

        // The cache now holds the built card.
        let cached = builder.cache.lock().unwrap().clone();
        assert!(cached.is_some(), "build() must populate the cache");

        // get_cached_or_build returns the cached instance (same skills/name),
        // not a fresh build.
        let from_cache = builder.get_cached_or_build(
            &registry,
            "DIFFERENT",
            "different",
            "9.9.9",
            "http://other",
            capabilities(),
            None,
        );
        // Despite different args, the cached card (from build) is returned.
        assert_eq!(from_cache.name, built.name);
        assert_eq!(from_cache.version, built.version);
        assert_eq!(from_cache.name, "Agent");
    }
}

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

/// Convert a flat security-scheme value (the shape `JWTAuthenticator::security_schemes()`
/// returns, e.g. `{"type":"http","scheme":...,"bearerFormat":...}`) into the A2A 1.0
/// protobuf-JSON `oneof` shape served by the reference a2a-sdk, e.g.
/// `{"httpAuthSecurityScheme":{"scheme":...,"bearerFormat":...}}`. Keeps the served
/// `securitySchemes` byte-identical across the Python/TS/Rust SDKs (the proto3 JSON
/// mapping is the canonical A2A 1.0 wire shape; the flat OpenAPI-style form is not).
fn to_a2a_security_schemes(schemes: Option<Value>) -> Value {
    let Some(Value::Object(map)) = schemes else {
        return Value::Object(serde_json::Map::new());
    };
    let out: serde_json::Map<String, Value> = map
        .into_iter()
        .map(|(key, scheme)| (key, to_a2a_security_scheme(&scheme)))
        .collect();
    Value::Object(out)
}

fn to_a2a_security_scheme(scheme: &Value) -> Value {
    if scheme.get("type").and_then(Value::as_str) == Some("http") {
        let mut http = serde_json::Map::new();
        let scheme_name = scheme
            .get("scheme")
            .and_then(Value::as_str)
            .unwrap_or("bearer");
        http.insert("scheme".to_string(), Value::String(scheme_name.to_string()));
        // proto3 JSON omits empty fields; only emit bearerFormat when present.
        if let Some(fmt) = scheme.get("bearerFormat").and_then(Value::as_str) {
            if !fmt.is_empty() {
                http.insert("bearerFormat".to_string(), Value::String(fmt.to_string()));
            }
        }
        return serde_json::json!({ "httpAuthSecurityScheme": http });
    }
    // Unknown scheme types map to an empty SecurityScheme (matches the Python builder).
    Value::Object(serde_json::Map::new())
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
                // The skill description is the descriptor's own one-line
                // `description`, which is what `apcore-a2a-python`'s
                // `AgentCardBuilder` reads (`agent_card.py:147`).
                //
                // NOT `Registry::describe`: since apcore 0.27 that returns the
                // cross-SDK Markdown *document* — an `# {module_id}` heading,
                // `**Tags:**`, a `**Parameters:**` list and `**Documentation:**`
                // — which would land whole in every `AgentSkill.description` on
                // the Agent Card. Through apcore 0.26 it returned
                // `module.description()`, i.e. exactly this field, so reading
                // the descriptor keeps the card identical across the bump.
                let desc = def.description.clone();
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
            // `extended_agent_card` is decided by the caller, not derived from
            // `security_schemes`. The factory sets it to `auth.is_some()`
            // (Python/TS parity). Deriving it from `security_schemes.is_some()`
            // would diverge for a custom Authenticator that returns null schemes
            // while auth is configured.
            capabilities,
            skills,
            default_input_modes: vec!["text/plain".to_string(), "application/json".to_string()],
            default_output_modes: vec!["text/plain".to_string(), "application/json".to_string()],
            security_schemes: to_a2a_security_schemes(security_schemes),
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

    #[test]
    fn security_schemes_use_a2a_oneof_shape() {
        // A-D-201: the flat {type:"http",...} input is transformed into the proto3
        // `httpAuthSecurityScheme` oneof shape (canonical A2A 1.0, byte-identical to
        // the Python a2a-sdk's served card).
        let input = Some(json!({
            "bearerAuth": { "type": "http", "scheme": "bearer", "bearerFormat": "JWT" }
        }));
        assert_eq!(
            to_a2a_security_schemes(input),
            json!({
                "bearerAuth": { "httpAuthSecurityScheme": { "scheme": "bearer", "bearerFormat": "JWT" } }
            })
        );
        // None / empty input -> empty object (no schemes).
        assert_eq!(to_a2a_security_schemes(None), json!({}));
    }

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
    fn skill_description_is_the_one_line_description_not_the_describe_document() {
        // Regression (apcore 0.27): `Registry::describe` changed from returning
        // `module.description()` to returning the cross-SDK Markdown document
        // (`# {module_id}` heading, `**Tags:**`, `**Parameters:**`,
        // `**Documentation:**`). The card must carry the one-line description
        // `apcore-a2a-python` puts there (`agent_card.py:147` reads
        // `descriptor.description`), not that whole document.
        let registry = echo_registry();
        let builder = AgentCardBuilder::new(SkillMapper::new());

        let card = build_card(&builder, &registry);
        let skill = card
            .skills
            .iter()
            .find(|s| s.id == "test.echo")
            .expect("echo skill is advertised");

        assert_eq!(skill.description, "Echoes its inputs");

        // Anchor on the runtime value: if a future apcore makes `describe()`
        // return the bare line again, this stops being a real guard and the
        // assertion below says so rather than passing vacuously.
        let document = registry
            .describe("test.echo")
            .expect("module is registered");
        assert_ne!(
            document, skill.description,
            "Registry::describe no longer differs from the one-line description \
             — re-check which of the two the Agent Card should carry"
        );
        assert!(
            !skill.description.contains("# test.echo"),
            "the describe() document leaked into the skill description: {}",
            skill.description
        );
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

    #[test]
    fn build_respects_caller_extended_agent_card_not_security_schemes() {
        // Regression (D10-001): extended_agent_card must come from the caller's
        // capabilities (the factory sets it to `auth.is_some()`, Python/TS
        // parity), NOT from `security_schemes.is_some()`. A custom Authenticator
        // can configure auth while returning no security_schemes.
        let registry = echo_registry();
        let builder = AgentCardBuilder::new(SkillMapper::new());

        // auth configured (extended=true) but no security schemes → must stay true.
        let caps_true = AgentCapabilities {
            extended_agent_card: true,
            ..capabilities()
        };
        let card = builder.build(
            &registry,
            "Agent",
            "An agent",
            "1.0.0",
            "http://localhost",
            caps_true,
            None,
        );
        assert!(
            card.capabilities.extended_agent_card,
            "caller's extended_agent_card=true must be preserved when security_schemes is None"
        );

        // no auth (extended=false) but security schemes present → must stay false.
        let caps_false = AgentCapabilities {
            extended_agent_card: false,
            ..capabilities()
        };
        let card = builder.build(
            &registry,
            "Agent",
            "An agent",
            "1.0.0",
            "http://localhost",
            caps_false,
            Some(serde_json::json!({"bearer": {"type": "http"}})),
        );
        assert!(
            !card.capabilities.extended_agent_card,
            "caller's extended_agent_card=false must be preserved when security_schemes is Some"
        );
    }
}

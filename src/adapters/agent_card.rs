//! AgentCardBuilder — builds an A2A 1.0 Agent Card from an apcore Registry.

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
}

impl AgentCardBuilder {
    pub fn new(skill_mapper: SkillMapper) -> Self {
        Self { skill_mapper }
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
                Some(self.skill_mapper.map(module_id, &def, &desc))
            })
            .collect();

        AgentCard {
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
        }
    }
}

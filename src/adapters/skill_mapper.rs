//! SkillMapper — converts apcore ModuleDescriptor to an A2A 1.0 AgentSkill.

use apcore::registry::ModuleDescriptor;

use super::agent_card::AgentSkill;

/// Maps an apcore module descriptor to an A2A skill.
#[derive(Debug, Clone, Default)]
pub struct SkillMapper;

impl SkillMapper {
    pub fn new() -> Self {
        Self
    }

    pub fn map(
        &self,
        module_id: &str,
        descriptor: &ModuleDescriptor,
        description: &str,
    ) -> AgentSkill {
        AgentSkill {
            id: module_id.to_string(),
            name: humanize_id(module_id),
            description: description.to_string(),
            tags: descriptor.tags.clone(),
            examples: vec![],
            input_modes: vec!["application/json".to_string()],
            output_modes: vec!["application/json".to_string()],
            // Required by A2A 1.0 AgentSkill; empty by default.
            security_requirements: vec![],
        }
    }
}

fn humanize_id(id: &str) -> String {
    id.replace(['.', '_', '-'], " ")
        .split_whitespace()
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                None => String::new(),
                Some(c) => c.to_uppercase().to_string() + chars.as_str(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

//! SkillMapper — converts apcore ModuleDescriptor to an A2A 1.0 AgentSkill.
//!
//! Honors the §5.13 `metadata.display` overlay (with an `a2a`-scoped override)
//! for surface-facing fields (name / description / tags) and derives A2A
//! input/output modes from the descriptor's JSON Schemas (Python/TS parity).

use apcore::registry::ModuleDescriptor;
use serde_json::Value;

use super::agent_card::AgentSkill;
use super::schema::SchemaConverter;

/// Maps an apcore module descriptor to an A2A skill.
#[derive(Debug, Clone, Default)]
pub struct SkillMapper {
    // Share root-type detection with SchemaConverter so the "string root" rule
    // lives in exactly one place.
    schema_converter: SchemaConverter,
}

impl SkillMapper {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn map(
        &self,
        module_id: &str,
        descriptor: &ModuleDescriptor,
        description: &str,
    ) -> AgentSkill {
        // §5.13 display overlay: prefer `metadata.display`, falling back to the
        // dedicated `descriptor.display` field. Within that, an `a2a`-scoped
        // object overrides the generic display fields.
        let display = descriptor
            .metadata
            .get("display")
            .cloned()
            .or_else(|| descriptor.display.clone())
            .unwrap_or(Value::Null);
        let a2a_display = display.get("a2a").cloned().unwrap_or(Value::Null);

        // name = a2a.alias || display.alias || humanize(module_id)
        let name = str_field(&a2a_display, "alias")
            .or_else(|| str_field(&display, "alias"))
            .unwrap_or_else(|| humanize_id(module_id));

        // description = a2a.description || display.description || raw description
        let mut skill_description = str_field(&a2a_display, "description")
            .or_else(|| str_field(&display, "description"))
            .unwrap_or_else(|| description.to_string());

        // Append guidance (a2a.guidance || display.guidance) when present.
        if let Some(guidance) =
            str_field(&a2a_display, "guidance").or_else(|| str_field(&display, "guidance"))
        {
            skill_description = format!("{skill_description}\n\nGuidance: {guidance}");
        }

        // tags = display.tags (when non-empty) else descriptor.tags
        let tags = display
            .get("tags")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| descriptor.tags.clone());

        AgentSkill {
            id: module_id.to_string(),
            name,
            description: skill_description,
            tags,
            examples: self.build_examples(descriptor),
            input_modes: self.compute_input_modes(descriptor),
            output_modes: self.compute_output_modes(descriptor),
            // Required by A2A 1.0 AgentSkill; empty by default.
            security_requirements: vec![],
        }
    }

    /// Compute A2A `input_modes` from the descriptor's input schema.
    fn compute_input_modes(&self, descriptor: &ModuleDescriptor) -> Vec<String> {
        match schema_opt(&descriptor.input_schema) {
            None => vec!["text/plain".to_string()],
            Some(schema) => {
                if self.schema_converter.detect_root_type(Some(schema)) == "string" {
                    vec!["application/json".to_string(), "text/plain".to_string()]
                } else {
                    vec!["application/json".to_string()]
                }
            }
        }
    }

    /// Compute A2A `output_modes` from the descriptor's output schema.
    fn compute_output_modes(&self, descriptor: &ModuleDescriptor) -> Vec<String> {
        match schema_opt(&descriptor.output_schema) {
            None => vec!["text/plain".to_string()],
            Some(_) => vec!["application/json".to_string()],
        }
    }

    /// Build up to 10 example titles from the descriptor's examples (skip empties).
    fn build_examples(&self, descriptor: &ModuleDescriptor) -> Vec<String> {
        descriptor
            .examples
            .iter()
            .take(10)
            .filter(|ex| !ex.title.trim().is_empty())
            .map(|ex| ex.title.clone())
            .collect()
    }
}

/// Treat a `null` or empty-object JSON Schema as "no schema present".
fn schema_opt(schema: &Value) -> Option<&Value> {
    match schema {
        Value::Null => None,
        Value::Object(map) if map.is_empty() => None,
        other => Some(other),
    }
}

/// Read a non-empty string field from a JSON object, if present.
fn str_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn descriptor(module_id: &str) -> ModuleDescriptor {
        ModuleDescriptor {
            module_id: module_id.to_string(),
            name: None,
            description: "raw description".to_string(),
            documentation: None,
            input_schema: json!({ "type": "object" }),
            output_schema: json!({ "type": "object" }),
            version: "1.0.0".to_string(),
            tags: vec!["t1".to_string()],
            annotations: None,
            examples: vec![],
            metadata: Default::default(),
            display: None,
            sunset_date: None,
            dependencies: vec![],
            enabled: true,
        }
    }

    #[test]
    fn a2a_alias_overrides_name() {
        let mut d = descriptor("image.resize");
        d.metadata.insert(
            "display".to_string(),
            json!({ "a2a": { "alias": "Resize Image" } }),
        );
        let skill = SkillMapper::new().map("image.resize", &d, "raw description");
        assert_eq!(skill.name, "Resize Image");
    }

    #[test]
    fn display_alias_used_when_no_a2a() {
        let mut d = descriptor("image.resize");
        d.metadata
            .insert("display".to_string(), json!({ "alias": "Shrinker" }));
        let skill = SkillMapper::new().map("image.resize", &d, "raw description");
        assert_eq!(skill.name, "Shrinker");
    }

    #[test]
    fn name_humanized_without_overlay() {
        let d = descriptor("image.resize");
        let skill = SkillMapper::new().map("image.resize", &d, "raw description");
        assert_eq!(skill.name, "Image Resize");
    }

    #[test]
    fn guidance_appended_to_description() {
        let mut d = descriptor("x.y");
        d.metadata.insert(
            "display".to_string(),
            json!({ "a2a": { "description": "Does X", "guidance": "Use carefully" } }),
        );
        let skill = SkillMapper::new().map("x.y", &d, "raw description");
        assert_eq!(skill.description, "Does X\n\nGuidance: Use carefully");
    }

    #[test]
    fn display_tags_override_descriptor_tags() {
        let mut d = descriptor("x.y");
        d.metadata
            .insert("display".to_string(), json!({ "tags": ["a", "b"] }));
        let skill = SkillMapper::new().map("x.y", &d, "raw description");
        assert_eq!(skill.tags, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn empty_display_tags_fall_back_to_descriptor() {
        let mut d = descriptor("x.y");
        d.metadata
            .insert("display".to_string(), json!({ "tags": [] }));
        let skill = SkillMapper::new().map("x.y", &d, "raw description");
        assert_eq!(skill.tags, vec!["t1".to_string()]);
    }

    #[test]
    fn no_input_schema_yields_text_plain() {
        let mut d = descriptor("x.y");
        d.input_schema = Value::Null;
        d.output_schema = Value::Null;
        let skill = SkillMapper::new().map("x.y", &d, "raw description");
        assert_eq!(skill.input_modes, vec!["text/plain".to_string()]);
        assert_eq!(skill.output_modes, vec!["text/plain".to_string()]);
    }

    #[test]
    fn string_root_input_yields_json_and_text() {
        let mut d = descriptor("x.y");
        d.input_schema = json!({ "type": "string" });
        let skill = SkillMapper::new().map("x.y", &d, "raw description");
        assert_eq!(
            skill.input_modes,
            vec!["application/json".to_string(), "text/plain".to_string()]
        );
    }

    #[test]
    fn object_schema_yields_json_modes() {
        let d = descriptor("x.y");
        let skill = SkillMapper::new().map("x.y", &d, "raw description");
        assert_eq!(skill.input_modes, vec!["application/json".to_string()]);
        assert_eq!(skill.output_modes, vec!["application/json".to_string()]);
    }

    #[test]
    fn examples_populated_from_descriptor() {
        use apcore::module::ModuleExample;
        // ModuleExample is #[non_exhaustive]; construct via deserialization.
        let ex = |title: &str| -> ModuleExample {
            serde_json::from_value(json!({
                "title": title,
                "inputs": {},
                "output": {},
            }))
            .unwrap()
        };
        let mut d = descriptor("x.y");
        d.examples = vec![ex("First"), ex(""), ex("Second")]; // empty title skipped
        let skill = SkillMapper::new().map("x.y", &d, "raw description");
        assert_eq!(
            skill.examples,
            vec!["First".to_string(), "Second".to_string()]
        );
    }
}

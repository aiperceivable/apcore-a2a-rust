//! SkillMapper — converts apcore ModuleDescriptor to an A2A 1.0 AgentSkill.
//!
//! Honors the §5.13 `metadata.display` overlay (with an `a2a`-scoped override)
//! for surface-facing fields (name / description / tags) and derives A2A
//! input/output modes from the descriptor's JSON Schemas (Python/TS parity).

use apcore::module::ModuleAnnotations;
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

    /// Map a module descriptor to an [`AgentSkill`].
    ///
    /// Precondition: the caller ([`AgentCardBuilder`](super::agent_card::AgentCardBuilder))
    /// guarantees `description` is non-empty and non-whitespace. Unlike Python/TS
    /// `to_skill` (which returns an `Option` and skips empty-description modules),
    /// this does not itself skip empty-description modules — the empty-description
    /// filter lives in the builder. The `Option` refactor is intentionally
    /// deferred (no observable behavior change).
    pub fn to_skill(
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
        let mut tags = display
            .get("tags")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| descriptor.tags.clone());
        append_annotation_tags(&mut tags, descriptor);

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

    /// Build up to 10 example titles from the descriptor's examples.
    ///
    /// Empty titles are skipped, but whitespace-only titles are kept (no trim)
    /// to match Python/TS, which only filter strictly empty strings.
    fn build_examples(&self, descriptor: &ModuleDescriptor) -> Vec<String> {
        descriptor
            .examples
            .iter()
            .take(10)
            .filter(|ex| !ex.title.is_empty())
            .map(|ex| ex.title.clone())
            .collect()
    }
}

/// The four behavioral annotations promoted onto the A2A wire, with the tag
/// each becomes. Order is fixed so the card is byte-identical across the three
/// bindings (srs FR-SKL-004 criterion 8).
type AnnotationFlag = fn(&ModuleAnnotations) -> bool;

const ANNOTATION_TAGS: [(&str, AnnotationFlag); 4] = [
    ("apcore:readonly", |a| a.readonly),
    ("apcore:destructive", |a| a.destructive),
    ("apcore:idempotent", |a| a.idempotent),
    ("apcore:requires-approval", |a| a.requires_approval),
];

/// Append apcore's behavioral annotations to a skill's tags (srs FR-SKL-004).
///
/// A2A 1.0 `AgentSkill` is `{id, name, description, tags, examples, inputModes,
/// outputModes, securityRequirements}` — no `extensions`, no `metadata` — so
/// `tags` is the only carrier that exists. The `apcore:` prefix keeps these out
/// of the module's own flat tag namespace, where a user tag named `destructive`
/// would otherwise be indistinguishable from the annotation.
///
/// Without this the Agent Card carried enough for a caller to *construct* a call
/// and not enough to judge whether making it is safe. It is also what makes
/// retry semantics usable: `retryable` is a property of the error, but whether a
/// retry is safe is a property of the operation, and a timeout is retryable for
/// a read and dangerous for a non-idempotent mutation.
///
/// Only `true` flags are emitted, matching how the apcore MCP binding maps the
/// same annotations onto optional `readOnlyHint` / `destructiveHint` /
/// `idempotentHint`. Absence means "not asserted", never "asserted false".
fn append_annotation_tags(tags: &mut Vec<String>, descriptor: &ModuleDescriptor) {
    let Some(annotations) = descriptor.annotations.as_ref() else {
        return;
    };
    for (tag, is_set) in ANNOTATION_TAGS {
        if is_set(annotations) && !tags.iter().any(|existing| existing == tag) {
            tags.push(tag.to_string());
        }
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
    // Only '.' and '_' become word separators; hyphens are preserved as
    // characters (e.g. "foo-bar" → "Foo-Bar"), matching the spec
    // `_humanize_module_id` and Python `.title()` / TS `\b\w → toUpperCase`:
    // the first alphanumeric of every run (after any non-alphanumeric boundary,
    // including a hyphen) is capitalized; other characters are left as-is.
    let mut out = String::with_capacity(id.len());
    let mut at_boundary = true;
    for ch in id.chars() {
        if ch == '.' || ch == '_' {
            // Separators collapse to a single space; the following char starts
            // a new word. Avoid emitting leading/duplicate spaces.
            if !out.ends_with(' ') && !out.is_empty() {
                out.push(' ');
            }
            at_boundary = true;
        } else if ch.is_alphanumeric() {
            if at_boundary {
                out.extend(ch.to_uppercase());
            } else {
                out.push(ch);
            }
            at_boundary = false;
        } else {
            // Preserved punctuation (e.g. '-') is a word boundary but stays.
            out.push(ch);
            at_boundary = true;
        }
    }
    out.trim_end().to_string()
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
        let skill = SkillMapper::new().to_skill("image.resize", &d, "raw description");
        assert_eq!(skill.name, "Resize Image");
    }

    #[test]
    fn display_alias_used_when_no_a2a() {
        let mut d = descriptor("image.resize");
        d.metadata
            .insert("display".to_string(), json!({ "alias": "Shrinker" }));
        let skill = SkillMapper::new().to_skill("image.resize", &d, "raw description");
        assert_eq!(skill.name, "Shrinker");
    }

    #[test]
    fn name_humanized_without_overlay() {
        let d = descriptor("image.resize");
        let skill = SkillMapper::new().to_skill("image.resize", &d, "raw description");
        assert_eq!(skill.name, "Image Resize");
    }

    #[test]
    fn hyphen_preserved_when_humanizing_id() {
        // Regression (A-D-011): only '.' and '_' become spaces; the hyphen is
        // preserved as a character (not turned into a space → no "Foo Bar").
        // Matching Python `.title()` / TS `\b\w`, the letter after the hyphen is
        // still capitalized, so "foo-bar" → "Foo-Bar".
        let d = descriptor("foo-bar");
        let skill = SkillMapper::new().to_skill("foo-bar", &d, "raw description");
        assert_eq!(skill.name, "Foo-Bar");
    }

    #[test]
    fn guidance_appended_to_description() {
        let mut d = descriptor("x.y");
        d.metadata.insert(
            "display".to_string(),
            json!({ "a2a": { "description": "Does X", "guidance": "Use carefully" } }),
        );
        let skill = SkillMapper::new().to_skill("x.y", &d, "raw description");
        assert_eq!(skill.description, "Does X\n\nGuidance: Use carefully");
    }

    #[test]
    fn display_tags_override_descriptor_tags() {
        let mut d = descriptor("x.y");
        d.metadata
            .insert("display".to_string(), json!({ "tags": ["a", "b"] }));
        let skill = SkillMapper::new().to_skill("x.y", &d, "raw description");
        assert_eq!(skill.tags, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn empty_display_tags_fall_back_to_descriptor() {
        let mut d = descriptor("x.y");
        d.metadata
            .insert("display".to_string(), json!({ "tags": [] }));
        let skill = SkillMapper::new().to_skill("x.y", &d, "raw description");
        assert_eq!(skill.tags, vec!["t1".to_string()]);
    }

    #[test]
    fn no_input_schema_yields_text_plain() {
        let mut d = descriptor("x.y");
        d.input_schema = Value::Null;
        d.output_schema = Value::Null;
        let skill = SkillMapper::new().to_skill("x.y", &d, "raw description");
        assert_eq!(skill.input_modes, vec!["text/plain".to_string()]);
        assert_eq!(skill.output_modes, vec!["text/plain".to_string()]);
    }

    #[test]
    fn string_root_input_yields_json_and_text() {
        let mut d = descriptor("x.y");
        d.input_schema = json!({ "type": "string" });
        let skill = SkillMapper::new().to_skill("x.y", &d, "raw description");
        assert_eq!(
            skill.input_modes,
            vec!["application/json".to_string(), "text/plain".to_string()]
        );
    }

    #[test]
    fn object_schema_yields_json_modes() {
        let d = descriptor("x.y");
        let skill = SkillMapper::new().to_skill("x.y", &d, "raw description");
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
        // Strictly-empty title skipped; whitespace-only title kept (Python/TS parity).
        d.examples = vec![ex("First"), ex(""), ex("  "), ex("Second")];
        let skill = SkillMapper::new().to_skill("x.y", &d, "raw description");
        assert_eq!(
            skill.examples,
            vec!["First".to_string(), "  ".to_string(), "Second".to_string()]
        );
    }
}

//! PartConverter — converts between A2A 1.0 Parts and apcore module I/O.

use serde_json::Value;
use uuid::Uuid;

use super::schema::SchemaConverter;
use crate::types::{Artifact, Part};

/// Re-exported for backward compatibility; the canonical type is [`crate::types::Part`].
pub use crate::types::Part as A2aPart;

pub struct PartConverter {
    schema_converter: SchemaConverter,
}

impl PartConverter {
    pub fn new(schema_converter: SchemaConverter) -> Self {
        Self { schema_converter }
    }

    /// Convert apcore module output into A2A 1.0 Parts.
    ///
    /// - string → text Part
    /// - object → data Part
    /// - array → text Part (JSON-serialized, matching Python/TS `json.dumps`)
    /// - other scalars → text Part (stringified)
    pub fn convert_result(&self, result: &Value) -> Vec<Part> {
        match result {
            Value::String(s) => vec![Part::text(s.clone())],
            Value::Object(_) => vec![Part::data(result.clone())],
            Value::Array(_) => vec![Part::text(
                serde_json::to_string(result).unwrap_or_else(|_| result.to_string()),
            )],
            Value::Null => vec![],
            other => vec![Part::text(other.to_string())],
        }
    }

    /// Convert apcore module output into an A2A 1.0 [`Artifact`].
    ///
    /// Wraps [`convert_result`](Self::convert_result) and assigns the artifact id
    /// `art-{task_id}`, falling back to `art-{uuid}` when `task_id` is empty
    /// (matches Python `art-{task_id or uuid4()}` and the TS adapter).
    pub fn output_to_parts(&self, output: &Value, task_id: &str) -> Artifact {
        let parts = self.convert_result(output);
        let artifact_id = format!(
            "art-{}",
            if task_id.is_empty() {
                Uuid::new_v4().to_string()
            } else {
                task_id.to_string()
            }
        );
        Artifact::new(artifact_id, parts)
    }

    /// Convert an inbound A2A message's Parts into apcore module input.
    ///
    /// Mirrors the Python/TS adapters:
    /// 1. empty → error
    /// 2. more than one part → error (exactly one expected)
    /// 3. data part → its data value
    /// 4. text part + object-typed input schema → parse JSON
    /// 5. text part otherwise → raw string
    /// 6. file part → unsupported
    pub fn parts_to_input(
        &self,
        parts: &[Part],
        input_schema: Option<&Value>,
    ) -> Result<Value, String> {
        if parts.is_empty() {
            return Err("Message must contain at least one Part".to_string());
        }
        if parts.len() > 1 {
            return Err("Multiple parts are not supported; expected exactly one Part".to_string());
        }
        match &parts[0] {
            Part::Data { data } => Ok(data.clone()),
            Part::Text { text } => {
                if self.schema_converter.detect_root_type(input_schema) == "object" {
                    serde_json::from_str::<Value>(text)
                        .map_err(|e| format!("TextPart text is not valid JSON: {e}"))
                } else {
                    Ok(Value::String(text.clone()))
                }
            }
            Part::FileUrl { .. } | Part::FileRaw { .. } => {
                Err("FilePart is not supported".to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::schema::SchemaConverter;
    use serde_json::json;

    fn conv() -> PartConverter {
        PartConverter::new(SchemaConverter::new())
    }

    #[test]
    fn convert_string_to_text_part() {
        assert_eq!(conv().convert_result(&json!("hi")), vec![Part::text("hi")]);
    }

    #[test]
    fn convert_object_to_data_part() {
        let out = conv().convert_result(&json!({ "a": 1 }));
        assert_eq!(out, vec![Part::data(json!({ "a": 1 }))]);
    }

    #[test]
    fn convert_array_to_text_part() {
        // Arrays are emitted as a JSON-serialized text Part (Python/TS parity),
        // not a data Part.
        let out = conv().convert_result(&json!([1, 2, 3]));
        assert_eq!(out, vec![Part::text("[1,2,3]")]);
    }

    #[test]
    fn output_to_parts_uses_task_id() {
        let art = conv().output_to_parts(&json!("hi"), "t1");
        assert_eq!(art.artifact_id, "art-t1");
        assert_eq!(art.parts, vec![Part::text("hi")]);
    }

    #[test]
    fn output_to_parts_empty_task_id_uses_uuid_fallback() {
        let art = conv().output_to_parts(&json!("hi"), "");
        assert!(art.artifact_id.starts_with("art-"));
        // The uuid fallback must produce a non-empty id beyond the "art-" prefix.
        assert!(art.artifact_id.len() > "art-".len());
        assert_ne!(art.artifact_id, "art-");
    }

    #[test]
    fn data_part_to_input() {
        let parts = vec![Part::data(json!({ "x": 1 }))];
        assert_eq!(
            conv().parts_to_input(&parts, None).unwrap(),
            json!({ "x": 1 })
        );
    }

    #[test]
    fn text_part_no_schema_is_raw_string() {
        let parts = vec![Part::text("hello")];
        assert_eq!(conv().parts_to_input(&parts, None).unwrap(), json!("hello"));
    }

    #[test]
    fn text_part_object_schema_parses_json() {
        let parts = vec![Part::text("{\"a\":1}")];
        let schema = json!({ "type": "object" });
        assert_eq!(
            conv().parts_to_input(&parts, Some(&schema)).unwrap(),
            json!({ "a": 1 })
        );
    }

    #[test]
    fn empty_and_file_parts_error() {
        assert!(conv().parts_to_input(&[], None).is_err());
        let file = vec![Part::FileUrl {
            url: "http://x".into(),
        }];
        assert!(conv().parts_to_input(&file, None).is_err());
    }
}

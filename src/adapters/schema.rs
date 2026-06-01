//! SchemaConverter — converts apcore JSON Schemas for A2A usage.
//!
//! `$ref` resolution is delegated to the shared `apcore-toolkit` resolver
//! (`deep_resolve_refs`), the same helper used by apcore-mcp and apcore-cli. The
//! resolver is depth-capped and non-throwing: circular / missing / non-pointer
//! `$ref`s resolve to a partial schema or `{}` rather than erroring.

use apcore_toolkit::deep_resolve_refs;
use serde_json::{json, Value};

/// Converts apcore module input/output JSON Schema for A2A.
#[derive(Debug, Clone, Default)]
pub struct SchemaConverter;

impl SchemaConverter {
    pub fn new() -> Self {
        Self
    }

    pub fn convert_input_schema(&self, schema: &Value) -> Value {
        self.convert_schema(schema)
    }

    pub fn convert_output_schema(&self, schema: &Value) -> Value {
        self.convert_schema(schema)
    }

    /// Return `"string"`, `"object"`, or `"unknown"` for a schema's root type.
    pub fn detect_root_type(&self, schema: Option<&Value>) -> &'static str {
        let Some(schema) = schema else {
            return "unknown";
        };
        match schema.get("type").and_then(Value::as_str) {
            Some("string") => "string",
            Some("object") => "object",
            _ if schema.get("properties").is_some() => "object",
            _ => "unknown",
        }
    }

    fn convert_schema(&self, schema: &Value) -> Value {
        // Empty / non-object schema → empty object schema.
        let empty = match schema.as_object() {
            Some(obj) => obj.is_empty(),
            None => true,
        };
        if empty {
            return json!({ "type": "object", "properties": {} });
        }

        // Inline $refs (the schema is its own resolution document because
        // Pydantic emits self-contained "#/$defs/..." pointers), then strip $defs.
        let mut resolved = if schema.get("$defs").is_some() {
            let mut r = deep_resolve_refs(schema, schema, 0);
            if let Some(map) = r.as_object_mut() {
                map.remove("$defs");
            }
            r
        } else {
            schema.clone()
        };

        ensure_object_type(&mut resolved);
        resolved
    }
}

/// Ensure the root schema declares `type: object`.
fn ensure_object_type(schema: &mut Value) {
    let Some(map) = schema.as_object_mut() else {
        *schema = json!({ "type": "object", "properties": {} });
        return;
    };
    let has_props = map.contains_key("properties");
    let is_object = map.get("type").and_then(Value::as_str) == Some("object");
    if !map.contains_key("type") || (has_props && !is_object) {
        map.insert("type".to_string(), Value::String("object".to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_schema_becomes_object() {
        let out = SchemaConverter::new().convert_input_schema(&json!({}));
        assert_eq!(out, json!({ "type": "object", "properties": {} }));
    }

    #[test]
    fn refs_inlined_and_defs_stripped() {
        let schema = json!({
            "type": "object",
            "properties": { "item": { "$ref": "#/$defs/Item" } },
            "$defs": { "Item": { "type": "string" } }
        });
        let out = SchemaConverter::new().convert_input_schema(&schema);
        assert!(out.get("$defs").is_none());
        assert_eq!(out["properties"]["item"], json!({ "type": "string" }));
    }

    #[test]
    fn detect_root_type() {
        let c = SchemaConverter::new();
        assert_eq!(
            c.detect_root_type(Some(&json!({ "type": "string" }))),
            "string"
        );
        assert_eq!(
            c.detect_root_type(Some(&json!({ "type": "object" }))),
            "object"
        );
        assert_eq!(
            c.detect_root_type(Some(&json!({ "properties": {} }))),
            "object"
        );
        assert_eq!(c.detect_root_type(None), "unknown");
    }
}

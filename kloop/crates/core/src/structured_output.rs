//! Internal schema-specialized turn support used only by Workflow child agents.

use anyhow::anyhow;
use anyhow::Result;
use serde_json::Value;

use kloop_protocol::ToolDef;

const MAX_SCHEMA_BYTES: usize = 128 * 1024;
const MAX_RESULT_BYTES: usize = 1024 * 1024;
const MAX_DEPTH: usize = 64;
const MAX_ERROR_CHARS: usize = 1_000;

pub(crate) const TOOL_NAME: &str = "StructuredOutput";

pub(crate) fn validate_schema(schema: &Value) -> Result<()> {
    let bytes = serde_json::to_vec(schema)?;
    if bytes.len() > MAX_SCHEMA_BYTES {
        return Err(anyhow!(
            "structured output schema exceeds the {MAX_SCHEMA_BYTES}-byte limit"
        ));
    }
    inspect(schema, 0)?;
    jsonschema::validator_for(schema)
        .map(|_| ())
        .map_err(|error| anyhow!("invalid structured output schema: {error}"))
}

pub(crate) fn tool_def(schema: &Value) -> ToolDef {
    ToolDef {
        name: TOOL_NAME.into(),
        description: "Return the final result for this structured Workflow agent. Call this tool exactly once with a value matching its schema; invalid values are rejected and you must retry.".into(),
        schema: schema.clone(),
    }
}

pub(crate) fn validate_value(schema: &Value, value: &Value) -> Result<(), String> {
    let bytes = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    if bytes.len() > MAX_RESULT_BYTES {
        return Err(format!(
            "structured output exceeds the {MAX_RESULT_BYTES}-byte limit"
        ));
    }
    let validator = jsonschema::validator_for(schema)
        .map_err(|error| format!("invalid structured output schema: {error}"))?;
    match validator.validate(value) {
        Ok(()) => Ok(()),
        Err(error) => {
            let raw = error.to_string();
            let message: String = raw.chars().take(MAX_ERROR_CHARS).collect();
            Err(if raw.chars().count() > MAX_ERROR_CHARS {
                format!("{message}… (validation error truncated)")
            } else {
                message
            })
        }
    }
}

pub(crate) fn success_result(tool_use_id: String) -> kloop_protocol::ContentBlock {
    kloop_protocol::ContentBlock::ToolResult {
        tool_use_id,
        content: "Structured output accepted.".into(),
        is_error: false,
    }
}

pub(crate) fn error_result(tool_use_id: String, error: &str) -> kloop_protocol::ContentBlock {
    kloop_protocol::ContentBlock::ToolResult {
        tool_use_id,
        content: format!("Structured output rejected: {error}").into(),
        is_error: true,
    }
}

pub(crate) fn nudge() -> String {
    format!(
        "Your response did not call {TOOL_NAME}. Call {TOOL_NAME} now with the final value matching the required schema; do not return plain text."
    )
}

fn inspect(value: &Value, depth: usize) -> Result<()> {
    if depth > MAX_DEPTH {
        return Err(anyhow!(
            "structured output schema exceeds the maximum depth of {MAX_DEPTH}"
        ));
    }
    match value {
        Value::Object(object) => {
            if object.contains_key("$ref") {
                return Err(anyhow!(
                    "structured output schemas cannot contain $ref (remote resolution is disabled)"
                ));
            }
            for child in object.values() {
                inspect(child, depth + 1)?;
            }
        }
        Value::Array(array) => {
            for child in array {
                inspect(child, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validates_objects_arrays_scalars_and_rejects_refs() {
        let object = json!({
            "type": "object",
            "properties": {"name": {"type": "string"}},
            "required": ["name"],
            "additionalProperties": false
        });
        validate_schema(&object).unwrap();
        assert_eq!(validate_value(&object, &json!({"name": "x"})), Ok(()));
        assert!(validate_value(&object, &json!({"name": 1})).is_err());
        assert!(validate_value(&object, &json!({})).is_err());
        let nested = json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {"state": {"enum": ["ready", "done"]}},
                        "required": ["state"]
                    }
                }
            },
            "required": ["items"]
        });
        assert_eq!(
            validate_value(&nested, &json!({"items": [{"state": "ready"}]})),
            Ok(())
        );
        assert!(validate_value(&nested, &json!({"items": [{"state": "bad"}]})).is_err());
        assert_eq!(
            validate_value(
                &json!({"type": "array", "items": {"type": "integer"}}),
                &json!([1, 2])
            ),
            Ok(())
        );
        assert_eq!(
            validate_value(&json!({"type": "string", "enum": ["a"]}), &json!("a")),
            Ok(())
        );
        assert!(validate_schema(&json!({"$ref": "https://example.test/schema"})).is_err());
    }
}

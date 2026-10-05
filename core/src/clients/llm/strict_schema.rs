//! A conservative shared subset of provider strict tool schemas.
//!
//! This is a compatibility check, not a complete JSON Schema validator or a
//! catalogue of everything either provider supports. Unsupported or oversized
//! schemas keep their original meaning and use the caller's non-strict path.

use std::{collections::HashSet, io};

use serde_json::{Map, Value};

const MAX_DEPTH: usize = 10;
const MAX_PROPERTIES: usize = 100;
const MAX_ENUM_VALUES: usize = 100;
const MAX_SCHEMA_BYTES: usize = 16 * 1024;

/// Check without changing the schema. OpenAI requires every declared property
/// in `required`; Anthropic may preserve optional properties.
pub(super) fn supports_strict_schema(schema: &Value, require_all_properties: bool) -> bool {
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return false;
    }
    let mut budget = SchemaBudget::default();
    check_schema(schema, require_all_properties, 1, &mut budget)
        && serde_json::to_writer(
            ByteBudget {
                remaining: MAX_SCHEMA_BYTES,
            },
            schema,
        )
        .is_ok()
}

#[derive(Default)]
struct SchemaBudget {
    properties: usize,
    enum_values: usize,
}

fn check_schema(
    schema: &Value,
    require_all_properties: bool,
    depth: usize,
    budget: &mut SchemaBudget,
) -> bool {
    if depth > MAX_DEPTH {
        return false;
    }
    let Some(object) = schema.as_object() else {
        return false;
    };
    let Some((kind, nullable)) = schema_type(object.get("type")) else {
        return false;
    };
    for (keyword, value) in object {
        match keyword.as_str() {
            "type" => {}
            "title" | "description" if value.is_string() => {}
            "$schema" if depth == 1 && is_supported_draft(value) => {}
            "properties" | "required" | "additionalProperties" if kind == "object" => {}
            "items" if kind == "array" => {}
            "enum" if is_primitive(kind) => {}
            _ => return false,
        }
    }
    if let Some(values) = object.get("enum") {
        let Some(values) = values.as_array() else {
            return false;
        };
        if values.is_empty() || values.len() > MAX_ENUM_VALUES - budget.enum_values {
            return false;
        }
        budget.enum_values += values.len();
        for (index, value) in values.iter().enumerate() {
            if !matches_primitive(value, kind, nullable)
                || values[..index].iter().any(|other| {
                    other == value
                        || (other.is_number()
                            && value.is_number()
                            && other.as_f64() == value.as_f64())
                })
            {
                return false;
            }
        }
    }
    match kind {
        "object" => check_object(object, require_all_properties, depth, budget),
        "array" => object
            .get("items")
            .is_some_and(|items| check_schema(items, require_all_properties, depth + 1, budget)),
        _ => true,
    }
}

fn check_object(
    object: &Map<String, Value>,
    require_all_properties: bool,
    depth: usize,
    budget: &mut SchemaBudget,
) -> bool {
    if object.get("additionalProperties") != Some(&Value::Bool(false)) {
        return false;
    }
    let Some(properties) = object.get("properties").and_then(Value::as_object) else {
        return false;
    };
    if properties.len() > MAX_PROPERTIES - budget.properties {
        return false;
    }
    budget.properties += properties.len();
    let mut required = HashSet::new();
    if require_all_properties && !object.contains_key("required") {
        return false;
    }
    if let Some(value) = object.get("required") {
        let Some(names) = value.as_array() else {
            return false;
        };
        if names.len() > properties.len() {
            return false;
        }
        for name in names {
            let Some(name) = name.as_str() else {
                return false;
            };
            if !properties.contains_key(name) || !required.insert(name) {
                return false;
            }
        }
    }
    if require_all_properties && required.len() != properties.len() {
        return false;
    }
    properties
        .values()
        .all(|child| check_schema(child, require_all_properties, depth + 1, budget))
}

fn schema_type(value: Option<&Value>) -> Option<(&str, bool)> {
    match value? {
        Value::String(kind)
            if is_primitive(kind) || matches!(kind.as_str(), "object" | "array") =>
        {
            Some((kind, false))
        }
        Value::Array(types) if types.len() == 2 => {
            let first = types[0].as_str()?;
            let second = types[1].as_str()?;
            let kind = if first == "null" { second } else { first };
            (is_primitive(kind) && kind != "null" && (first == "null" || second == "null"))
                .then_some((kind, true))
        }
        _ => None,
    }
}

fn is_primitive(kind: &str) -> bool {
    matches!(kind, "string" | "number" | "integer" | "boolean" | "null")
}

fn matches_primitive(value: &Value, kind: &str, nullable: bool) -> bool {
    if value.is_null() {
        return nullable || kind == "null";
    }
    match kind {
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.as_f64().is_some_and(|number| number.fract() == 0.0),
        "boolean" => value.is_boolean(),
        _ => false,
    }
}

fn is_supported_draft(value: &Value) -> bool {
    matches!(
        value.as_str(),
        Some("https://json-schema.org/draft/2020-12/schema")
            | Some("https://json-schema.org/draft/2020-12/schema#")
    )
}

/// Limit serialization without allocating a second copy of a large schema.
/// Structural checks run first, so serialization also has bounded nesting.
struct ByteBudget {
    remaining: usize,
}

impl io::Write for ByteBudget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.remaining {
            return Err(io::Error::other("Strict schema size limit exceeded."));
        }
        self.remaining -= bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn closed_object(property: Value) -> Value {
        json!({
            "type": "object",
            "properties": {"value": property},
            "required": ["value"],
            "additionalProperties": false
        })
    }

    #[test]
    fn bundled_answer_is_supported_without_modification() {
        let schema: Value = serde_json::from_str(include_str!(
            "../../../resources/output_schemas/answer.json"
        ))
        .unwrap();
        let original = schema.clone();
        assert!(supports_strict_schema(&schema, true));
        assert!(supports_strict_schema(&schema, false));
        assert_eq!(schema, original);
    }

    #[test]
    fn optional_properties_depend_on_provider_policy_including_nested_objects() {
        let optional = json!({
            "type": "object",
            "properties": {"label": {"type": "string"}},
            "additionalProperties": false
        });
        for schema in [optional.clone(), closed_object(optional)] {
            assert!(!supports_strict_schema(&schema, true));
            assert!(supports_strict_schema(&schema, false));
        }
        let mut empty = json!({
            "type": "object", "properties": {}, "additionalProperties": false
        });
        assert!(!supports_strict_schema(&empty, true));
        assert!(supports_strict_schema(&empty, false));
        empty["required"] = json!([]);
        assert!(supports_strict_schema(&empty, true));
    }

    #[test]
    fn nested_objects_must_be_closed_even_inside_arrays() {
        let nested = json!({
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"]
        });
        for property in [nested.clone(), json!({"type": "array", "items": nested})] {
            for require_all in [true, false] {
                assert!(!supports_strict_schema(
                    &closed_object(property.clone()),
                    require_all
                ));
            }
        }
        assert!(supports_strict_schema(
            &closed_object(json!({
                "type": "array", "items": closed_object(json!({"type": "boolean"}))
            })),
            true
        ));
    }

    #[test]
    fn annotations_nullable_primitives_and_scalar_enums_are_supported() {
        for property in [
            json!({"type": "string", "title": "Label", "description": "A label", "enum": ["$ref", "default"]}),
            json!({"type": ["integer", "null"], "enum": [1, 2, null]}),
            json!({"type": ["null", "boolean"], "enum": [true, false, null]}),
            json!({"type": "number", "enum": [1.5, 2]}),
            json!({"type": "null", "enum": [null]}),
        ] {
            assert!(supports_strict_schema(&closed_object(property), true));
        }
    }

    #[test]
    fn unsupported_features_fall_back_without_rewriting_meaning() {
        for (keyword, value) in [
            ("default", json!("value")),
            ("format", json!("date-time")),
            ("pattern", json!(".*")),
            ("$ref", json!("#/$defs/text")),
            ("anyOf", json!([{"type": "string"}])),
            ("allOf", json!([{"type": "string"}])),
            ("unknown", json!(true)),
        ] {
            let mut property = json!({"type": "string"});
            property[keyword] = value;
            let schema = closed_object(property);
            let original = schema.clone();
            assert!(!supports_strict_schema(&schema, false));
            assert_eq!(schema, original);
        }
    }

    #[test]
    fn malformed_or_ambiguous_keywords_do_not_select_strict_mode() {
        for property in [
            json!({"type": "unknown"}),
            json!({"type": ["string", "integer"]}),
            json!({"type": ["null", "null"]}),
            json!({"type": "array", "items": [{"type": "string"}]}),
            json!({"type": "array"}),
            json!({"type": "string", "description": 1}),
            json!({"type": "string", "enum": []}),
            json!({"type": "string", "enum": "value"}),
            json!({"type": "string", "enum": ["value", "value"]}),
            json!({"type": "string", "enum": [{"$ref": "not a schema"}]}),
            json!({"type": "integer", "enum": [1.5]}),
            json!({"type": "boolean", "enum": ["true"]}),
        ] {
            assert!(!supports_strict_schema(&closed_object(property), false));
        }
        for (keyword, value) in [
            ("required", json!("value")),
            ("required", json!([1])),
            ("required", json!(["missing"])),
            ("required", json!(["value", "value"])),
            ("properties", json!([])),
            ("additionalProperties", json!(true)),
            ("$schema", json!(42)),
        ] {
            let mut schema = closed_object(json!({"type": "string"}));
            schema[keyword] = value;
            assert!(!supports_strict_schema(&schema, false));
        }
    }

    #[test]
    fn resource_limits_conservatively_disable_strict_mode() {
        let mut deep = json!({"type": "string"});
        for _ in 0..MAX_DEPTH {
            deep = json!({"type": "array", "items": deep});
        }
        assert!(!supports_strict_schema(&closed_object(deep), false));

        let properties: Map<String, Value> = (0..=MAX_PROPERTIES)
            .map(|index| (format!("p{index}"), json!({"type": "string"})))
            .collect();
        assert!(!supports_strict_schema(
            &json!({
                "type": "object", "properties": properties, "additionalProperties": false
            }),
            false
        ));

        let values: Vec<usize> = (0..=MAX_ENUM_VALUES).collect();
        assert!(!supports_strict_schema(
            &closed_object(json!({
                "type": "integer", "enum": values
            })),
            false
        ));

        assert!(!supports_strict_schema(
            &closed_object(json!({
                "type": "string", "description": "x".repeat(MAX_SCHEMA_BYTES)
            })),
            false
        ));
    }
}

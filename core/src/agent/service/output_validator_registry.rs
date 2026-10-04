//! Shared, compiled object schemas for Answer inputs and other tool arguments.

use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
};

use jsonschema::{Draft, Registry, Retrieve, Uri, Validator};
use serde_json::Value;

use crate::{
    agent::{AgentError, OutputSchemaSource},
    llm::ToolError,
};

use super::registry_files::read_resources;

const DRAFT_URI: &str = "https://json-schema.org/draft/2020-12/schema";
const SCHEMA_BASE_URI: &str = "https://murmeli.invalid/output-schema.json";

pub struct OutputValidator {
    schema: Value,
    compiled: Validator,
}

impl OutputValidator {
    pub fn new(schema: Value) -> Result<Self, AgentError> {
        if schema.get("type").and_then(Value::as_str) != Some("object") {
            return Err(AgentError::Configuration(
                "An output schema must declare an object at its root.",
            ));
        }
        if !jsonschema::draft202012::meta::is_valid(&schema) {
            return Err(AgentError::Configuration("The output schema is invalid."));
        }
        let registry = Registry::new()
            .draft(Draft::Draft202012)
            .retriever(DenyExternalReferences)
            .add(SCHEMA_BASE_URI, &schema)
            .and_then(|builder| builder.prepare())
            .map_err(|_| schema_reference_error())?;
        validate_schema_references(&schema, &registry)?;
        let compiled = jsonschema::draft202012::options()
            .with_base_uri(SCHEMA_BASE_URI)
            .with_registry(&registry)
            .with_retriever(DenyExternalReferences)
            .should_validate_formats(false)
            .build(&schema)
            .map_err(|_| AgentError::Configuration("The output schema could not be compiled."))?;
        drop(registry);
        Ok(Self { schema, compiled })
    }

    pub fn schema(&self) -> &Value {
        &self.schema
    }

    pub fn validate(&self, value: &Value) -> Result<(), ToolError> {
        self.compiled
            .validate(value)
            .map_err(|_| ToolError::InvalidArguments)
    }
}

struct DenyExternalReferences;

impl Retrieve for DenyExternalReferences {
    fn retrieve(
        &self,
        _uri: &Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Err("External schema retrieval is disabled.".into())
    }
}

fn schema_reference_error() -> AgentError {
    AgentError::Configuration("The output schema contains an unsupported reference.")
}

/// Visit schema positions and referenced targets, not unreferenced instance data.
/// The library resolver owns pointer, anchor, and embedded `$id` scope semantics.
fn validate_schema_references(schema: &Value, registry: &Registry<'_>) -> Result<(), AgentError> {
    let base_uri =
        jsonschema::uri::from_str(SCHEMA_BASE_URI).map_err(|_| schema_reference_error())?;
    let resolver = registry
        .resolver(base_uri)
        .in_subresource(Draft::Draft202012.create_resource_ref(schema))
        .map_err(|_| schema_reference_error())?;
    let mut pending = vec![(schema, resolver)];
    let mut visited = HashSet::new();
    while let Some((schema, resolver)) = pending.pop() {
        // Identity plus scope breaks recursive references without conflating
        // the same document node reached under different resource roots.
        if !visited.insert((schema as *const Value, resolver.base_uri())) {
            continue;
        }
        let Some(object) = schema.as_object() else {
            continue;
        };
        if let Some(draft) = object.get("$schema")
            && draft.as_str().map(|value| value.trim_end_matches('#')) != Some(DRAFT_URI)
        {
            return Err(AgentError::Configuration(
                "Output schemas must use JSON Schema Draft 2020-12.",
            ));
        }
        for keyword in ["$ref", "$dynamicRef"] {
            if let Some(reference) = object.get(keyword).and_then(Value::as_str) {
                if !reference.is_empty() && !reference.starts_with('#') {
                    return Err(AgentError::Configuration(
                        "Output schemas may reference only their own document.",
                    ));
                }
                let resolved = resolver
                    .lookup(reference)
                    .map_err(|_| schema_reference_error())?;
                pending.push((resolved.contents(), resolved.resolver().clone()));
            }
        }
        let mut children = Vec::new();
        for keyword in [
            "$defs",
            "definitions",
            "properties",
            "patternProperties",
            "dependentSchemas",
        ] {
            if let Some(values) = object.get(keyword).and_then(Value::as_object) {
                children.extend(values.values());
            }
        }
        for keyword in ["allOf", "anyOf", "oneOf", "prefixItems"] {
            if let Some(values) = object.get(keyword).and_then(Value::as_array) {
                children.extend(values);
            }
        }
        for keyword in [
            "additionalProperties",
            "unevaluatedProperties",
            "propertyNames",
            "items",
            "unevaluatedItems",
            "contains",
            "not",
            "if",
            "then",
            "else",
            "contentSchema",
        ] {
            if let Some(child) = object.get(keyword) {
                children.push(child);
            }
        }
        for child in children {
            let child_resolver = resolver
                .in_subresource(Draft::Draft202012.create_resource_ref(child))
                .map_err(|_| schema_reference_error())?;
            pending.push((child, child_resolver));
        }
    }
    Ok(())
}

pub struct OutputValidatorRegistry {
    validators: HashMap<String, Arc<OutputValidator>>,
}

impl OutputValidatorRegistry {
    pub fn from_directory(path: impl AsRef<Path>) -> Result<Self, AgentError> {
        let mut validators = HashMap::new();
        for (id, content) in read_resources(path.as_ref(), "json")? {
            let schema = serde_json::from_str(&content)
                .map_err(|_| AgentError::Configuration("An output schema is not valid JSON."))?;
            validators.insert(id, Arc::new(OutputValidator::new(schema)?));
        }
        Ok(Self { validators })
    }

    pub fn resolve(&self, source: &OutputSchemaSource) -> Result<Arc<OutputValidator>, AgentError> {
        match source {
            OutputSchemaSource::Reference(id) => {
                self.validators
                    .get(id)
                    .cloned()
                    .ok_or(AgentError::Configuration(
                        "The output schema reference is unknown.",
                    ))
            }
            OutputSchemaSource::Inline(schema) => {
                Ok(Arc::new(OutputValidator::new(schema.clone())?))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::agent::service::registry_files::tests::TestDirectory;

    fn answer_schema() -> Value {
        json!({
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
            "additionalProperties": false
        })
    }

    #[test]
    fn validates_the_configured_object_contract() {
        let schema = answer_schema();
        let validator = OutputValidator::new(schema.clone()).unwrap();
        assert_eq!(validator.schema(), &schema);
        assert!(validator.validate(&json!({"text": "Answer"})).is_ok());
        for invalid in [
            json!({}),
            json!({"text": 3}),
            json!({"text": "Answer", "extra": true}),
            json!("Answer"),
        ] {
            assert!(matches!(
                validator.validate(&invalid),
                Err(ToolError::InvalidArguments)
            ));
        }
    }

    #[test]
    fn rejects_invalid_or_unsupported_schema_contracts() {
        for invalid in [
            json!({"type": "string"}),
            json!({"type": "object", "properties": {"text": {"type": "typo"}}}),
            json!({"type": "object", "required": "text"}),
            json!({"type": "object", "$schema": "http://json-schema.org/draft-07/schema#"}),
        ] {
            assert!(OutputValidator::new(invalid).is_err());
        }
    }

    #[test]
    fn permits_local_references_and_treats_formats_as_annotations() {
        let validator = OutputValidator::new(json!({
            "$schema": DRAFT_URI,
            "$id": "https://example.invalid/answer.json",
            "type": "object",
            "$defs": {"text": {"type": "string", "format": "date-time"}},
            "properties": {"text": {"$ref": "#/$defs/text"}},
            "required": ["text"]
        }))
        .unwrap();
        assert!(
            validator
                .validate(&json!({"text": "This is not a timestamp"}))
                .is_ok()
        );
        assert!(validator.validate(&json!({"text": false})).is_err());
    }

    #[test]
    fn rejects_file_network_and_other_document_references() {
        for reference in [
            "file:///private/secret.json",
            "https://example.invalid/private.json",
            "other.json#/$defs/text",
            "https://json-schema.org/draft/2020-12/schema",
        ] {
            assert!(
                OutputValidator::new(json!({
                    "type": "object",
                    "properties": {"text": {"$ref": reference}}
                }))
                .is_err()
            );
        }
        assert!(
            OutputValidator::new(json!({
                "type": "object",
                "$defs": {"unused": {"$dynamicRef": "other.json#anchor"}}
            }))
            .is_err()
        );
    }

    #[test]
    fn does_not_interpret_instance_data_as_schema_instructions() {
        let data = json!({"$ref": "https://example.invalid/data", "$schema": "just data"});
        let validator = OutputValidator::new(json!({
            "type": "object",
            "properties": {"data": {"const": data, "default": data, "examples": [data], "enum": [data]}},
            "required": ["data"]
        })).unwrap();
        assert!(validator.validate(&json!({"data": data})).is_ok());
    }

    #[test]
    fn enforces_policy_on_local_reference_targets_outside_schema_keywords() {
        for target in [
            json!({"$schema": "http://json-schema.org/draft-07/schema#", "type": "string"}),
            json!({"$ref": "https://json-schema.org/draft/2020-12/schema"}),
        ] {
            assert!(
                OutputValidator::new(json!({
                    "type": "object",
                    "properties": {"data": {"$ref": "#/custom~1schema"}},
                    "custom/schema": target
                }))
                .is_err()
            );
        }
    }

    #[test]
    fn resolves_local_pointers_from_their_embedded_resource_scope() {
        let validator = OutputValidator::new(json!({
            "$id": "https://example.invalid/root/",
            "type": "object",
            "$defs": {
                "nested": {
                    "$id": "child/",
                    "type": "object",
                    "properties": {"text": {"$ref": "#/custom"}},
                    "custom": {"type": "string"}
                }
            },
            "properties": {"detail": {"$ref": "#/$defs/nested"}},
            "custom": {"$schema": "http://json-schema.org/draft-07/schema#"}
        }))
        .unwrap();
        assert!(
            validator
                .validate(&json!({"detail": {"text": "Answer"}}))
                .is_ok()
        );
        assert!(validator.validate(&json!({"detail": {"text": 7}})).is_err());
    }

    #[test]
    fn accepts_recursive_local_anchors_without_revisiting_forever() {
        let validator = OutputValidator::new(json!({
            "type": "object",
            "$defs": {"node": {
                "$anchor": "node",
                "type": "object",
                "properties": {"child": {"$ref": "#node"}}
            }},
            "properties": {"node": {"$ref": "#node"}}
        }))
        .unwrap();
        assert!(validator.validate(&json!({"node": {"child": {}}})).is_ok());
        assert!(
            validator
                .validate(&json!({"node": {"child": false}}))
                .is_err()
        );
    }

    #[test]
    fn registry_shares_compiled_snapshots_and_resolves_inline_schemas() {
        let dir = TestDirectory::new();
        dir.write("answer.json", &answer_schema().to_string());
        let registry = OutputValidatorRegistry::from_directory(&dir.0).unwrap();
        let source = OutputSchemaSource::Reference("answer".into());
        let first = registry.resolve(&source).unwrap();
        dir.write("answer.json", "Invalid JSON after startup");
        let second = registry.resolve(&source).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert!(second.validate(&json!({"text": "Answer"})).is_ok());
        assert!(
            registry
                .resolve(&OutputSchemaSource::Inline(answer_schema()))
                .is_ok()
        );
        assert!(
            registry
                .resolve(&OutputSchemaSource::Reference("../answer".into()))
                .is_err()
        );
        assert!(OutputValidatorRegistry::from_directory(&dir.0).is_err());
    }
}

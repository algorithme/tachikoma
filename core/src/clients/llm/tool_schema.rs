//! Select provider strict tool schemas without changing the caller's schema.

use serde_json::Value;

use crate::llm::{ToolDefinition, ToolSchemaMode};

use super::strict_schema::supports_strict_schema;

#[derive(Clone, Copy)]
pub(super) enum ToolProvider {
    OpenAI,
    Anthropic,
}

pub(super) fn strict_tools(
    provider: ToolProvider,
    model: &str,
    mode: ToolSchemaMode,
    tools: &[ToolDefinition],
) -> Vec<bool> {
    let known_model = supports_strict_tools(provider, model);
    let mut budget = AnthropicBudget::default();
    tools
        .iter()
        .enumerate()
        .map(|(tool_index, tool)| {
            let (strict, reason) = match mode {
                ToolSchemaMode::Off => (false, "disabled"),
                ToolSchemaMode::Required => (true, "required"),
                ToolSchemaMode::Auto if !known_model => (false, "unknown_model"),
                ToolSchemaMode::Auto
                    if !supports_strict_schema(
                        &tool.input_schema,
                        matches!(provider, ToolProvider::OpenAI),
                    ) =>
                {
                    (false, "unsupported_schema")
                }
                ToolSchemaMode::Auto
                    if matches!(provider, ToolProvider::Anthropic)
                        && !budget.reserve(&tool.input_schema) =>
                {
                    (false, "request_limit")
                }
                ToolSchemaMode::Auto => (true, "compatible"),
            };
            tracing::debug!(
                tool_index = tool_index + 1,
                strict,
                reason,
                "Selected tool schema mode"
            );
            strict
        })
        .collect()
}

fn supports_strict_tools(provider: ToolProvider, model: &str) -> bool {
    // Exact IDs checked against official documentation on 2026-10-05.
    // This deliberately small list is not a claim that other models lack support.
    // New models can use Required until their documented IDs are added here.
    // https://developers.openai.com/api/docs/models/{model}
    // https://platform.claude.com/docs/en/models/overview
    // https://platform.claude.com/docs/en/build-with-claude/structured-outputs#compatibility
    match provider {
        ToolProvider::OpenAI => matches!(
            model,
            "gpt-6-astra"
                | "gpt-6-sol"
                | "gpt-6.1-sol"
                | "gpt-6-luna"
                | "gpt-5.4-mini"
                | "gpt-5.4-mini-2026-03-17"
        ),
        ToolProvider::Anthropic => matches!(
            model,
            "claude-opus-5-5"
                | "claude-sonnet-5-5"
                | "claude-fable-5-1"
                | "claude-haiku-4-5"
                | "claude-haiku-4-5-20251001"
        ),
    }
}

#[derive(Default)]
struct AnthropicBudget {
    tools: usize,
    optional: usize,
    unions: usize,
}

impl AnthropicBudget {
    fn reserve(&mut self, schema: &Value) -> bool {
        // These are request-wide limits, not limits for individual tools:
        // https://platform.claude.com/docs/en/build-with-claude/structured-outputs#schema-complexity-limits
        let (optional, unions) = schema_cost(schema);
        if self.tools >= 20 || self.optional + optional > 24 || self.unions + unions > 16 {
            return false;
        }
        self.tools += 1;
        self.optional += optional;
        self.unions += unions;
        true
    }
}

/// Visit only schema locations recognized by supports_strict_schema. Property
/// names and annotation values are data, even when they resemble schema keywords.
fn schema_cost(schema: &Value) -> (usize, usize) {
    let mut optional = 0;
    let mut unions = usize::from(schema.get("type").is_some_and(Value::is_array));
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        let required = schema
            .get("required")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        // The compatibility check has already validated required names.
        optional += properties.len() - required;
        for property in properties.values() {
            let (child_optional, child_unions) = schema_cost(property);
            optional += child_optional;
            unions += child_unions;
        }
    }
    if let Some(items) = schema.get("items") {
        let (child_optional, child_unions) = schema_cost(items);
        optional += child_optional;
        unions += child_unions;
    }
    (optional, unions)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn tool(schema: Value) -> ToolDefinition {
        ToolDefinition {
            name: "answer".into(),
            description: "Return the answer.".into(),
            input_schema: schema,
        }
    }

    fn schema(optional: bool, nullable: bool) -> Value {
        json!({
            "type": "object",
            "properties": {"text": {
                "type": if nullable { json!(["string", "null"]) } else { json!("string") }
            }},
            "required": if optional { json!([]) } else { json!(["text"]) },
            "additionalProperties": false
        })
    }

    #[test]
    fn auto_uses_known_models_and_preserves_optional_properties() {
        let tools = vec![tool(schema(false, false)), tool(schema(true, false))];
        let original = tools.clone();
        assert_eq!(
            strict_tools(
                ToolProvider::OpenAI,
                "gpt-6-luna",
                ToolSchemaMode::Auto,
                &tools
            ),
            vec![true, false]
        );
        assert_eq!(
            strict_tools(
                ToolProvider::Anthropic,
                "claude-haiku-4-5",
                ToolSchemaMode::Auto,
                &tools
            ),
            vec![true, true]
        );
        assert_eq!(tools, original);
        for (provider, model) in [
            (ToolProvider::OpenAI, "gpt-6-luna-future"),
            (ToolProvider::OpenAI, "ft:gpt-6-luna:private"),
            (ToolProvider::Anthropic, "claude-haiku-4-5-future"),
        ] {
            assert_eq!(
                strict_tools(provider, model, ToolSchemaMode::Auto, &tools),
                vec![false, false]
            );
        }
    }

    #[test]
    fn off_and_required_are_explicit_and_do_not_rewrite_schemas() {
        let tools = vec![tool(schema(false, false)), tool(json!({"type": "object"}))];
        let original = tools.clone();
        for (provider, model) in [
            (ToolProvider::OpenAI, "gpt-6-luna"),
            (ToolProvider::Anthropic, "claude-haiku-4-5"),
        ] {
            assert_eq!(
                strict_tools(provider, model, ToolSchemaMode::Off, &tools),
                vec![false, false]
            );
            assert_eq!(
                strict_tools(provider, "unknown", ToolSchemaMode::Required, &tools),
                vec![true, true]
            );
            assert_eq!(
                strict_tools(provider, model, ToolSchemaMode::Auto, &tools),
                vec![true, false]
            );
        }
        assert_eq!(tools, original);
    }

    #[test]
    fn anthropic_limits_strict_tools_in_configuration_order() {
        let tools = vec![tool(schema(false, false)); 21];
        let flags = strict_tools(
            ToolProvider::Anthropic,
            "claude-haiku-4-5",
            ToolSchemaMode::Auto,
            &tools,
        );
        assert_eq!(flags, [vec![true; 20], vec![false]].concat());
        assert_eq!(
            strict_tools(
                ToolProvider::Anthropic,
                "claude-haiku-4-5",
                ToolSchemaMode::Required,
                &tools
            ),
            vec![true; 21]
        );
    }

    #[test]
    fn anthropic_optional_and_union_limits_cover_nested_properties_and_items() {
        let nested_optional = json!({
            "type": "object",
            "properties": {
                "items": {"type": "array", "items": schema(true, false)},
                "type": {"type": "string"}
            },
            "required": ["items"],
            "additionalProperties": false
        });
        // Each schema has two optional properties, including one below array items.
        let mut tools = vec![tool(nested_optional); 13];
        tools.push(tool(schema(false, false)));
        assert_eq!(
            strict_tools(
                ToolProvider::Anthropic,
                "claude-haiku-4-5",
                ToolSchemaMode::Auto,
                &tools
            ),
            [vec![true; 12], vec![false, true]].concat()
        );

        let nested_union = json!({
            "type": "object",
            "properties": {"items": {"type": "array", "items": schema(false, true)}},
            "required": ["items"],
            "additionalProperties": false
        });
        let mut tools = vec![tool(nested_union); 17];
        tools.push(tool(schema(false, false)));
        assert_eq!(
            strict_tools(
                ToolProvider::Anthropic,
                "claude-haiku-4-5",
                ToolSchemaMode::Auto,
                &tools
            ),
            [vec![true; 16], vec![false, true]].concat()
        );
    }
}

use std::sync::Arc;

use serde_json::Value;

use crate::agent::service::OutputValidator;

use super::{Tool, ToolDefinition, ToolFuture};

/// A side-effect-free tool whose input is the structured answer.
pub struct AnswerTool {
    definition: ToolDefinition,
    validator: Arc<OutputValidator>,
}

impl AnswerTool {
    pub fn new(validator: Arc<OutputValidator>) -> Self {
        Self {
            definition: ToolDefinition {
                name: "answer".into(),
                description: "The default terminal tool to call when the task is complete. Pass the answer itself as a dictionary matching the input schema; it is returned unchanged. If answer is listed among this agent's configured final tools, call it alone to finish the invocation. Otherwise, finish using one of the configured final tools.".into(),
                input_schema: validator.schema().clone(),
            },
            validator,
        }
    }
}

impl Tool for AnswerTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    fn execute(&self, arguments: Value) -> ToolFuture<'_> {
        Box::pin(async move {
            self.validator.validate(&arguments)?;
            Ok(arguments)
        })
    }
}

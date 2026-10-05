use std::{future::Future, pin::Pin};

use serde_json::Value;

pub mod answer;

pub use crate::llm::{ToolDefinition, ToolError};
pub use answer::AnswerTool;

pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = Result<Value, ToolError>> + Send + 'a>>;

/// Implementations must support concurrent calls and must not detach work from this future.
/// The agent validates arguments against the advertised schema before execution.
pub trait Tool: Send + Sync {
    fn definition(&self) -> &ToolDefinition;
    fn execute(&self, arguments: Value) -> ToolFuture<'_>;
}

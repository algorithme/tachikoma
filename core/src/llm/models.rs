//! Provider-independent decisions, with original assistant content kept for replay.

use std::fmt;

use serde_json::Value;

use super::LLMError;

pub type ConversationHistory = Vec<LLMMessage>;

/// Provider-side tool argument constraints; local validation remains authoritative.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ToolSchemaMode {
    /// Use strict mode for known models and an unchanged, conservatively supported schema.
    #[default]
    Auto,
    /// Preserve best-effort provider generation and validate arguments locally.
    Off,
    /// Request strict mode explicitly, returning provider errors instead of downgrading.
    Required,
}

#[derive(Clone, PartialEq)]
pub enum LLMMessage {
    User(String),
    Assistant(AssistantReplay),
    ToolResults(Vec<ToolResult>),
}

/// Opaque to the loop. Only the matching provider adapter interprets these items.
#[derive(Clone, PartialEq)]
pub enum AssistantReplay {
    OpenAI(Vec<Value>),
    Anthropic(Vec<Value>),
}

pub struct LLMRequest<'a> {
    pub system_prompt: &'a str,
    pub messages: &'a [LLMMessage],
    pub tools: &'a [ToolDefinition],
}

#[derive(Clone, PartialEq)]
pub struct LLMResponse {
    pub text: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub replay: AssistantReplay,
}

impl LLMResponse {
    /// Preserve the original text-only interface's rejection of unsolicited tools.
    pub fn into_text(self) -> Result<String, LLMError> {
        if !self.tool_calls.is_empty() {
            return Err(LLMError::InvalidResponse);
        }
        self.text
            .filter(|text| !text.is_empty())
            .ok_or(LLMError::InvalidResponse)
    }

    pub(crate) fn validate_call_ids(&self) -> Result<(), LLMError> {
        let mut ids = std::collections::HashSet::new();
        if self
            .tool_calls
            .iter()
            .any(|call| call.id.trim().is_empty() || !ids.insert(call.id.as_str()))
        {
            return Err(LLMError::InvalidResponse);
        }
        Ok(())
    }
}

#[derive(Clone, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: ToolArguments,
}

#[derive(Clone, PartialEq)]
pub enum ToolArguments {
    Json(Value),
    InvalidJson,
}

#[derive(Clone, PartialEq)]
pub struct ToolResult {
    pub call_id: String,
    pub outcome: Result<Value, ToolError>,
}

/// Only safe, bounded diagnostics cross the tool/model boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolError {
    InvalidArguments,
    UnknownTool,
    RejectedBatch,
    ExecutionFailed,
    Interrupted,
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidArguments => "Tool arguments must be a JSON object matching the tool's input schema.",
            Self::UnknownTool => "This tool is not available to this agent. Choose an advertised tool.",
            Self::RejectedBatch => "A final tool must be the only call in a response. No calls in this batch were executed.",
            Self::ExecutionFailed => "Tool execution failed.",
            Self::Interrupted => "Tool execution was interrupted; its outcome is unknown. Do not assume its effects were undone.",
        })
    }
}

impl std::error::Error for ToolError {}

// Debug formatting stays redacted even when content tracing is explicitly enabled.
macro_rules! redacted_debug {
    ($($ty:ty),+ $(,)?) => {$ (
        impl fmt::Debug for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(concat!(stringify!($ty), "([REDACTED])"))
            }
        }
    )+};
}

redacted_debug!(
    LLMMessage,
    AssistantReplay,
    LLMRequest<'_>,
    LLMResponse,
    ToolDefinition,
    ToolCall,
    ToolArguments,
    ToolResult
);

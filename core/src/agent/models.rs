use std::fmt;

use serde_json::Value;

use crate::llm::LLMError;

#[derive(Clone)]
pub enum PromptSource {
    Reference(String),
    Inline(String),
}

#[derive(Clone)]
pub enum OutputSchemaSource {
    Reference(String),
    Inline(Value),
}

#[derive(Clone)]
pub struct AgentConfig {
    pub system_prompt: PromptSource,
    pub output_schema: OutputSchemaSource,
    pub terminal_tools: Vec<String>,
    pub max_turns: usize,
}

impl AgentConfig {
    /// Prompt and Answer schema are explicit; completion and turn limits have safe defaults.
    pub fn new(system_prompt: PromptSource, output_schema: OutputSchemaSource) -> Self {
        Self {
            system_prompt,
            output_schema,
            terminal_tools: vec!["answer".into()],
            max_turns: 10,
        }
    }
}

#[derive(Clone, PartialEq)]
pub enum AgentOutcome {
    Completed { tool_name: String, output: Value },
    Unsatisfactory { text: Option<String> },
}

#[derive(Debug, PartialEq, Eq)]
pub enum AgentError {
    Configuration(&'static str),
    InvalidInput,
    Provider(LLMError),
    TurnLimitExceeded,
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration(message) => f.write_str(message),
            Self::InvalidInput => f.write_str("The agent input must contain text."),
            Self::Provider(error) => error.fmt(f),
            Self::TurnLimitExceeded => f.write_str(
                "The agent reached its model-turn limit without a successful final tool.",
            ),
        }
    }
}

impl std::error::Error for AgentError {}

impl From<LLMError> for AgentError {
    fn from(error: LLMError) -> Self {
        Self::Provider(error)
    }
}

macro_rules! redacted_debug {
    ($($ty:ty),+ $(,)?) => {$ (
        impl fmt::Debug for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(concat!(stringify!($ty), "([REDACTED])"))
            }
        }
    )+};
}

redacted_debug!(PromptSource, OutputSchemaSource, AgentConfig, AgentOutcome);

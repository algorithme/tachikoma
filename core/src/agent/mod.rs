//! Stateful, configured conversations with explicit tool-based completion.

mod configurable_agent;
mod models;
pub mod service;
pub mod tool;

pub use configurable_agent::ConfigurableAgent;
pub use models::{AgentConfig, AgentError, AgentOutcome, OutputSchemaSource, PromptSource};

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use crate::llm::{ConversationHistory, LLMChat, LLMMessage, ToolDefinition};

use super::{
    AgentConfig, AgentError, AgentOutcome,
    service::{
        OutputValidator, OutputValidatorRegistry, PromptRegistry,
        agent_loop::{AgentLoop, RunContext},
    },
    tool::{AnswerTool, Tool},
};

pub(crate) struct RegisteredTool {
    pub implementation: Arc<dyn Tool>,
    pub validator: Arc<OutputValidator>,
}

/// One configured conversation. Distinct instances share services, never history.
pub struct ConfigurableAgent<L: LLMChat> {
    llm: Arc<L>,
    system_prompt: Arc<str>,
    definitions: Vec<ToolDefinition>,
    tools: HashMap<String, RegisteredTool>,
    terminal_tools: HashSet<String>,
    max_turns: usize,
    history: ConversationHistory,
    agent_loop: AgentLoop,
}

impl<L: LLMChat> ConfigurableAgent<L> {
    pub fn new(
        config: AgentConfig,
        llm: Arc<L>,
        prompt_registry: &PromptRegistry,
        validator_registry: &OutputValidatorRegistry,
        tools: Vec<Arc<dyn Tool>>,
    ) -> Result<Self, AgentError> {
        if config.max_turns == 0 || config.terminal_tools.is_empty() {
            return Err(AgentError::Configuration(
                "An agent needs a positive turn limit and at least one final tool.",
            ));
        }
        let prompt = prompt_registry.resolve(&config.system_prompt)?;
        let answer_validator = validator_registry.resolve(&config.output_schema)?;
        let answer: Arc<dyn Tool> = Arc::new(AnswerTool::new(answer_validator.clone()));
        let mut registered = HashMap::new();
        let mut definitions = Vec::with_capacity(tools.len() + 1);
        for implementation in std::iter::once(answer).chain(tools) {
            let definition = implementation.definition().clone();
            if definition.name.is_empty()
                || definition.name.len() > 64
                || !definition
                    .name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
                || registered.contains_key(&definition.name)
            {
                return Err(AgentError::Configuration(
                    "Tool names must be unique, 1-64 ASCII letters, digits, underscores or hyphens; answer is reserved.",
                ));
            }
            let validator = if definition.name == "answer" {
                answer_validator.clone()
            } else {
                Arc::new(OutputValidator::new(definition.input_schema.clone())?)
            };
            registered.insert(
                definition.name.clone(),
                RegisteredTool {
                    implementation,
                    validator,
                },
            );
            definitions.push(definition);
        }
        let terminal_tools: HashSet<_> = config.terminal_tools.iter().cloned().collect();
        if terminal_tools
            .iter()
            .any(|name| !registered.contains_key(name))
        {
            return Err(AgentError::Configuration(
                "Every final tool must be available to this agent.",
            ));
        }
        let system_prompt = format!(
            "{}\n\nFinal tools for this agent: {}.\n\n{}",
            prompt_registry.prefix(),
            config.terminal_tools.join(", "),
            prompt,
        )
        .into();
        tracing::info!(
            tool_count = definitions.len(),
            final_tool_count = terminal_tools.len(),
            max_turns = config.max_turns,
            "Agent initialized"
        );
        Ok(Self {
            llm,
            system_prompt,
            definitions,
            tools: registered,
            terminal_tools,
            max_turns: config.max_turns,
            history: Vec::new(),
            agent_loop: AgentLoop,
        })
    }

    /// Exclusive access serializes invocations for this conversation.
    pub async fn invoke(&mut self, input: &str) -> Result<AgentOutcome, AgentError> {
        self.agent_loop
            .run(
                RunContext {
                    llm: self.llm.as_ref(),
                    system_prompt: &self.system_prompt,
                    definitions: &self.definitions,
                    tools: &self.tools,
                    terminal_tools: &self.terminal_tools,
                    max_turns: self.max_turns,
                    history: &mut self.history,
                },
                input,
            )
            .await
    }

    /// Sensitive content is available explicitly to the owner, never through Debug.
    pub fn history(&self) -> &[LLMMessage] {
        &self.history
    }

    pub fn clear_history(&mut self) {
        self.history.clear();
    }
}

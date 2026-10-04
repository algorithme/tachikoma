use std::collections::{HashMap, HashSet};

use futures_util::{StreamExt, stream::FuturesUnordered};

use crate::{
    agent::{AgentError, AgentOutcome, configurable_agent::RegisteredTool},
    llm::{
        ConversationHistory, LLMChat, LLMMessage, LLMRequest, ToolArguments, ToolDefinition,
        ToolError, ToolResult,
    },
};

/// Execution rules have no persistent state; the caller owns the conversation.
pub(crate) struct AgentLoop;

pub(crate) struct RunContext<'a, L> {
    pub llm: &'a L,
    pub system_prompt: &'a str,
    pub definitions: &'a [ToolDefinition],
    pub tools: &'a HashMap<String, RegisteredTool>,
    pub terminal_tools: &'a HashSet<String>,
    pub max_turns: usize,
    pub history: &'a mut ConversationHistory,
}

impl AgentLoop {
    pub(crate) async fn run<L: LLMChat>(
        &self,
        context: RunContext<'_, L>,
        input: &str,
    ) -> Result<AgentOutcome, AgentError> {
        if input.trim().is_empty() {
            return Err(AgentError::InvalidInput);
        }
        context.history.push(LLMMessage::User(input.to_owned()));
        for _ in 0..context.max_turns {
            let response = context
                .llm
                .complete(LLMRequest {
                    system_prompt: context.system_prompt,
                    messages: context.history,
                    tools: context.definitions,
                })
                .await?;
            response.validate_call_ids()?;
            context.history.push(LLMMessage::Assistant(response.replay));
            if response.tool_calls.is_empty() {
                return Ok(AgentOutcome::Unsatisfactory {
                    text: response.text,
                });
            }

            let final_name = response
                .tool_calls
                .iter()
                .find(|call| context.terminal_tools.contains(&call.name))
                .map(|call| call.name.clone());
            let rejected = final_name.is_some() && response.tool_calls.len() != 1;

            // Install matching result slots before the first tool await. If this future is
            // dropped, completed slots remain truthful and unresolved ones remain unknown.
            context.history.push(LLMMessage::ToolResults(
                response
                    .tool_calls
                    .iter()
                    .map(|call| ToolResult {
                        call_id: call.id.clone(),
                        outcome: Err(if rejected {
                            ToolError::RejectedBatch
                        } else {
                            ToolError::Interrupted
                        }),
                    })
                    .collect(),
            ));
            let Some(LLMMessage::ToolResults(results)) = context.history.last_mut() else {
                unreachable!("tool result slots were just appended");
            };
            if rejected {
                continue;
            }

            let mut pending = FuturesUnordered::new();
            for (index, call) in response.tool_calls.into_iter().enumerate() {
                let Some(tool) = context.tools.get(&call.name) else {
                    results[index].outcome = Err(ToolError::UnknownTool);
                    continue;
                };
                let arguments = match call.arguments {
                    ToolArguments::Json(arguments) => arguments,
                    ToolArguments::InvalidJson => {
                        results[index].outcome = Err(ToolError::InvalidArguments);
                        continue;
                    }
                };
                if let Err(error) = tool.validator.validate(&arguments) {
                    results[index].outcome = Err(error);
                    continue;
                }
                pending.push(async move { (index, tool.implementation.execute(arguments).await) });
            }
            while let Some((index, outcome)) = pending.next().await {
                results[index].outcome = outcome;
            }
            if let Some(tool_name) = final_name
                && let Ok(output) = &results[0].outcome
            {
                return Ok(AgentOutcome::Completed {
                    tool_name,
                    output: output.clone(),
                });
            }
        }
        Err(AgentError::TurnLimitExceeded)
    }
}

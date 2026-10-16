use std::{
    collections::{HashMap, HashSet},
    time::Instant,
};

use futures_util::{StreamExt, stream::FuturesUnordered};
use tracing::Instrument;

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
    // Explicit fields only: function arguments contain private conversation data.
    #[tracing::instrument(
        name = "agent_invocation",
        skip_all,
        fields(invocation_id = %uuid::Uuid::new_v4())
    )]
    pub(crate) async fn run<L: LLMChat>(
        &self,
        context: RunContext<'_, L>,
        input: &str,
    ) -> Result<AgentOutcome, AgentError> {
        if input.trim().is_empty() {
            tracing::warn!("Agent invocation rejected: empty input");
            return Err(AgentError::InvalidInput);
        }
        let started = Instant::now();
        tracing::info!(
            max_turns = context.max_turns,
            history_messages = context.history.len(),
            "Agent invocation started"
        );
        tracing::debug!(
            target: "tachikoma_core::agent::content",
            system_prompt = ?context.system_prompt,
            user_input = ?input,
            "Agent prompt"
        );
        for (index, tool) in context.definitions.iter().enumerate() {
            tracing::debug!(
                target: "tachikoma_core::agent::content",
                tool_index = index + 1,
                tool = ?tool.name,
                description = ?tool.description,
                input_schema = %tool.input_schema,
                final_tool = context.terminal_tools.contains(&tool.name),
                "Available tool"
            );
        }
        context.history.push(LLMMessage::User(input.to_owned()));
        for turn in 1..=context.max_turns {
            tracing::info!(turn, "Requesting model response");
            let model_started = Instant::now();
            let response = match context
                .llm
                .complete(LLMRequest {
                    system_prompt: context.system_prompt,
                    messages: context.history,
                    tools: context.definitions,
                })
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    tracing::warn!(
                        turn,
                        elapsed_ms = model_started.elapsed().as_millis(),
                        "Model request failed"
                    );
                    return Err(error.into());
                }
            };
            tracing::debug!(
                target: "tachikoma_core::agent::content",
                turn,
                text = ?response.text,
                "Model answer"
            );
            if tracing::enabled!(target: "tachikoma_core::agent::content", tracing::Level::DEBUG) {
                for (index, call) in response.tool_calls.iter().enumerate() {
                    let arguments = match &call.arguments {
                        ToolArguments::Json(value) => value.to_string(),
                        ToolArguments::InvalidJson => "<invalid JSON>".into(),
                    };
                    tracing::debug!(
                        target: "tachikoma_core::agent::content",
                        turn,
                        call_index = index + 1,
                        tool = ?call.name,
                        arguments = ?arguments,
                        "Model tool call"
                    );
                }
            }
            if let Err(error) = response.validate_call_ids() {
                tracing::warn!(turn, "Model response rejected: invalid tool call IDs");
                return Err(error.into());
            }
            tracing::info!(
                turn,
                tool_calls = response.tool_calls.len(),
                elapsed_ms = model_started.elapsed().as_millis(),
                "Model response received"
            );
            context.history.push(LLMMessage::Assistant(response.replay));
            if response.tool_calls.is_empty() {
                tracing::warn!(
                    turn,
                    elapsed_ms = started.elapsed().as_millis(),
                    "Agent invocation unsatisfactory: no tool calls"
                );
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
                tracing::warn!(
                    turn,
                    tool_calls = results.len(),
                    "Tool batch rejected: a final tool must be called alone"
                );
                continue;
            }

            let mut pending = FuturesUnordered::new();
            for (index, call) in response.tool_calls.into_iter().enumerate() {
                let Some(tool) = context.tools.get(&call.name) else {
                    // Unknown names and provider call IDs can contain arbitrary model text.
                    tracing::warn!(
                        turn,
                        call_index = index + 1,
                        "Tool call rejected: unknown tool"
                    );
                    results[index].outcome = Err(ToolError::UnknownTool);
                    continue;
                };
                let arguments = match call.arguments {
                    ToolArguments::Json(arguments) => arguments,
                    ToolArguments::InvalidJson => {
                        tracing::warn!(
                            turn,
                            call_index = index + 1,
                            "Tool call rejected: malformed argument JSON"
                        );
                        results[index].outcome = Err(ToolError::InvalidArguments);
                        continue;
                    }
                };
                if let Err(error) = tool.validator.validate(&arguments) {
                    tracing::warn!(
                        turn,
                        call_index = index + 1,
                        "Tool call rejected: arguments do not match schema"
                    );
                    results[index].outcome = Err(error);
                    continue;
                }
                let span = tracing::info_span!(
                    "tool_call", turn, call_index = index + 1, tool = %call.name,
                );
                pending.push(async move {
                    let tool_started = Instant::now();
                    tracing::info!("Executing tool");
                    tracing::debug!(
                        target: "tachikoma_core::agent::content",
                        input = %arguments,
                        "Tool input"
                    );
                    let outcome = tool.implementation.execute(arguments).await;
                    match &outcome {
                        Ok(output) => {
                            tracing::debug!(
                                target: "tachikoma_core::agent::content",
                                output = %output,
                                "Tool output"
                            );
                            tracing::info!(elapsed_ms = tool_started.elapsed().as_millis(), "Tool execution succeeded");
                        }
                        Err(error) => tracing::warn!(elapsed_ms = tool_started.elapsed().as_millis(), error = ?error, "Tool execution failed"),
                    }
                    (index, outcome)
                }.instrument(span));
            }
            while let Some((index, outcome)) = pending.next().await {
                results[index].outcome = outcome;
            }
            if let Some(tool_name) = final_name
                && let Ok(output) = &results[0].outcome
            {
                tracing::info!(turns = turn, tool = %tool_name, elapsed_ms = started.elapsed().as_millis(), "Agent invocation completed");
                return Ok(AgentOutcome::Completed {
                    tool_name,
                    output: output.clone(),
                });
            }
        }
        tracing::warn!(
            turns = context.max_turns,
            elapsed_ms = started.elapsed().as_millis(),
            "Agent invocation stopped: model-turn limit exceeded"
        );
        Err(AgentError::TurnLimitExceeded)
    }
}

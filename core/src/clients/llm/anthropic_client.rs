use serde::Deserialize;
use serde_json::{Value, json};

use crate::common::config::{AnthropicConfig, RetryConfig};

use crate::llm::{
    AssistantReplay, LLMChat, LLMError, LLMMessage, LLMRequest, LLMResponse, ToolArguments,
    ToolCall, http_client, read_limited, request_with_retry,
};

const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";

/// An Anthropic client; conversation history belongs to the caller.
pub struct AnthropicChat {
    http: reqwest::Client,
    config: AnthropicConfig,
    retry: RetryConfig,
    endpoint: String,
}

impl AnthropicChat {
    pub fn new(config: AnthropicConfig, retry: RetryConfig) -> Result<Self, LLMError> {
        Ok(Self {
            http: http_client(retry.timeout)?,
            config,
            retry,
            endpoint: MESSAGES_URL.to_owned(),
        })
    }
}

impl LLMChat for AnthropicChat {
    async fn complete(&self, request: LLMRequest<'_>) -> Result<LLMResponse, LLMError> {
        let messages = encode_messages(request.messages)?;
        let api_key = self
            .config
            .api_key
            .as_ref()
            .filter(|key| !key.expose().trim().is_empty())
            .ok_or(LLMError::Configuration("ANTHROPIC_API_KEY is required"))?;
        let model = self
            .config
            .model
            .as_deref()
            .filter(|model| !model.trim().is_empty())
            .ok_or(LLMError::Configuration(
                "TACHIKOMA_ANTHROPIC_MODEL is required",
            ))?;
        if self.config.max_tokens == 0 {
            return Err(LLMError::Configuration(
                "TACHIKOMA_ANTHROPIC_MAX_TOKENS must be greater than zero",
            ));
        }

        let mut body = json!({
            "model": model,
            "max_tokens": self.config.max_tokens,
            "messages": messages,
        });
        if !request.system_prompt.is_empty() {
            body["system"] = json!(request.system_prompt);
        }
        if !request.tools.is_empty() {
            body["tools"] = Value::Array(
                request
                    .tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "name": tool.name,
                            "description": tool.description,
                            "input_schema": tool.input_schema,
                            "strict": false,
                        })
                    })
                    .collect(),
            );
            body["tool_choice"] = json!({"type": "auto", "disable_parallel_tool_use": false});
        }

        request_with_retry(
            self.retry,
            || {
                let mut request = self
                    .http
                    .post(&self.endpoint)
                    .bearer_auth(api_key.expose())
                    .header("anthropic-version", "2023-06-01")
                    .json(&body);
                if let Some(workspace_id) = &self.config.workspace_id {
                    request = request.header("anthropic-workspace-id", workspace_id);
                }
                request.send()
            },
            |response| async { parse_message(&read_limited(response).await?) },
        )
        .await
    }
}

fn encode_messages(history: &[LLMMessage]) -> Result<Vec<Value>, LLMError> {
    if history.is_empty() {
        return Err(LLMError::InvalidPrompt);
    }
    let mut messages = Vec::with_capacity(history.len());
    for message in history {
        match message {
            LLMMessage::User(text) => append_user(&mut messages, json!(text)),
            LLMMessage::Assistant(AssistantReplay::Anthropic(content)) => {
                messages.push(json!({"role": "assistant", "content": content}));
            }
            LLMMessage::Assistant(AssistantReplay::OpenAI(_)) => {
                return Err(LLMError::Configuration(
                    "Anthropic requests require Anthropic assistant history",
                ));
            }
            LLMMessage::ToolResults(results) => {
                let content = results
                    .iter()
                    .map(|result| {
                        let (content, is_error) = match &result.outcome {
                            Ok(value) => (value.to_string(), false),
                            Err(error) => (error.to_string(), true),
                        };
                        json!({
                            "type": "tool_result",
                            "tool_use_id": result.call_id,
                            "content": content,
                            "is_error": is_error,
                        })
                    })
                    .collect();
                append_user(&mut messages, Value::Array(content));
            }
        }
    }
    Ok(messages)
}

fn append_user(messages: &mut Vec<Value>, content: Value) {
    let Some(previous) = messages
        .last_mut()
        .filter(|message| message["role"] == "user")
    else {
        messages.push(json!({"role": "user", "content": content}));
        return;
    };
    let mut blocks = into_user_blocks(previous["content"].take());
    blocks.extend(into_user_blocks(content));
    // Anthropic requires tool results before text in their enclosing user message.
    blocks.sort_by_key(|block| block["type"] != "tool_result");
    previous["content"] = Value::Array(blocks);
}

fn into_user_blocks(content: Value) -> Vec<Value> {
    match content {
        Value::Array(blocks) => blocks,
        Value::String(text) => vec![json!({"type": "text", "text": text})],
        _ => unreachable!("user content is constructed from text or tool results"),
    }
}

#[derive(Deserialize)]
struct MessageResponse {
    #[serde(rename = "type")]
    kind: String,
    role: String,
    stop_reason: Option<String>,
    content: Vec<Value>,
}

fn parse_message(body: &[u8]) -> Result<LLMResponse, LLMError> {
    let response: MessageResponse =
        serde_json::from_slice(body).map_err(|_| LLMError::InvalidResponse)?;
    if response.kind != "message" || response.role != "assistant" {
        return Err(LLMError::InvalidResponse);
    }
    match response.stop_reason.as_deref() {
        Some("end_turn" | "stop_sequence" | "tool_use") => {}
        Some("refusal") => return Err(LLMError::Refused),
        Some("max_tokens" | "model_context_window_exceeded" | "pause_turn") | None => {
            return Err(LLMError::IncompleteResponse);
        }
        _ => return Err(LLMError::InvalidResponse),
    }

    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for block in &response.content {
        match block["type"].as_str() {
            Some("text") => text.push_str(block["text"].as_str().ok_or(LLMError::InvalidResponse)?),
            Some("thinking" | "redacted_thinking") => {}
            Some("tool_use") => {
                let id = block["id"].as_str().ok_or(LLMError::InvalidResponse)?;
                let name = block["name"].as_str().ok_or(LLMError::InvalidResponse)?;
                let arguments = block.get("input").ok_or(LLMError::InvalidResponse)?;
                tool_calls.push(ToolCall {
                    id: id.to_owned(),
                    name: name.to_owned(),
                    arguments: ToolArguments::Json(arguments.clone()),
                });
            }
            _ => return Err(LLMError::InvalidResponse),
        }
    }
    if (response.stop_reason.as_deref() == Some("tool_use")) == tool_calls.is_empty() {
        return Err(LLMError::InvalidResponse);
    }
    let result = LLMResponse {
        text: (!text.is_empty()).then_some(text),
        tool_calls,
        replay: AssistantReplay::Anthropic(response.content),
    };
    result.validate_call_ids()?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use axum::{
        Json, Router,
        http::{HeaderMap, StatusCode},
        routing::post,
    };
    use serde_json::Value;
    use tokio::net::TcpListener;

    use crate::{
        common::config::SecretString,
        llm::{ToolDefinition, ToolError, ToolResult},
    };

    use super::*;

    fn retry_config() -> RetryConfig {
        RetryConfig {
            max_retries: 2,
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(1),
            timeout: Duration::from_secs(5),
        }
    }

    fn config() -> AnthropicConfig {
        AnthropicConfig {
            api_key: Some(SecretString::new("test-anthropic-key").unwrap()),
            model: Some("test-claude-model".to_owned()),
            max_tokens: 512,
            workspace_id: Some("test-workspace".to_owned()),
        }
    }

    fn message(stop_reason: Value, content: Value) -> Value {
        json!({
            "type": "message",
            "role": "assistant",
            "stop_reason": stop_reason,
            "content": content,
        })
    }

    #[tokio::test]
    async fn invoke_sends_one_user_message_and_combines_text_blocks() {
        let captured = Arc::new(Mutex::new(None));
        let capture = captured.clone();
        let router = Router::new().route(
            "/v1/messages",
            post(
                move |headers: HeaderMap, Json(body): Json<Value>| async move {
                    *capture.lock().unwrap() = Some((headers, body));
                    Json(message(
                        json!("end_turn"),
                        json!([
                            {"type": "thinking", "thinking": "private reasoning"},
                            {"type": "text", "text": "Hello "},
                            {"type": "text", "text": "world"},
                        ]),
                    ))
                },
            ),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let mut chat = AnthropicChat::new(config(), retry_config()).unwrap();
        chat.endpoint = format!("http://{address}/v1/messages");

        let result = chat.invoke("A prompt with \"quotes\" and\na newline").await;
        server.abort();
        assert_eq!(result.unwrap(), "Hello world");
        let (headers, body) = captured.lock().unwrap().take().unwrap();
        assert_eq!(headers["authorization"], "Bearer test-anthropic-key");
        assert_eq!(headers["anthropic-version"], "2023-06-01");
        assert_eq!(headers["anthropic-workspace-id"], "test-workspace");
        assert_eq!(
            body,
            json!({
                "model": "test-claude-model",
                "max_tokens": 512,
                "messages": [{
                    "role": "user",
                    "content": "A prompt with \"quotes\" and\na newline",
                }],
            }),
        );
    }

    #[tokio::test]
    async fn tool_loop_replays_assistant_blocks_and_groups_ordered_results() {
        let first_content = json!([
            {"type": "thinking", "thinking": "private reasoning", "signature": "opaque-signature"},
            {"type": "redacted_thinking", "data": "opaque-encrypted-content"},
            {"type": "text", "text": "Checking both values."},
            {"type": "tool_use", "id": "call-one", "name": "lookup", "input": {"key": "first"}},
            {"type": "tool_use", "id": "call-two", "name": "lookup", "input": {"key": "second"}},
        ]);
        let first_reply = first_content.clone();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let capture = captured.clone();
        let router = Router::new().route(
            "/v1/messages",
            post(move |Json(body): Json<Value>| {
                let first_reply = first_reply.clone();
                let mut requests = capture.lock().unwrap();
                requests.push(body);
                let first = requests.len() == 1;
                async move {
                    if first {
                        Json(message(json!("tool_use"), first_reply))
                    } else {
                        Json(message(
                            json!("end_turn"),
                            json!([
                                {"type": "text", "text": "Both values are available."},
                            ]),
                        ))
                    }
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let mut chat = AnthropicChat::new(config(), retry_config()).unwrap();
        chat.endpoint = format!("http://{address}/v1/messages");
        let tools = [ToolDefinition {
            name: "lookup".to_owned(),
            description: "Look up a value".to_owned(),
            input_schema: json!({"type": "object", "properties": {"key": {"type": "string"}}, "required": ["key"]}),
        }];
        let mut history = vec![LLMMessage::User("Find both values.".to_owned())];
        let first = chat
            .complete(LLMRequest {
                system_prompt: "Use the provided tools.",
                messages: &history,
                tools: &tools,
            })
            .await
            .unwrap();
        assert_eq!(first.text.as_deref(), Some("Checking both values."));
        assert_eq!(first.tool_calls.len(), 2);
        assert_eq!(first.tool_calls[0].id, "call-one");
        assert_eq!(first.tool_calls[1].id, "call-two");
        assert_eq!(
            first.tool_calls[0].arguments,
            ToolArguments::Json(json!({"key": "first"}))
        );
        history.push(LLMMessage::Assistant(first.replay));
        history.push(LLMMessage::ToolResults(vec![
            ToolResult {
                call_id: "call-one".to_owned(),
                outcome: Ok(json!({"value": 1})),
            },
            ToolResult {
                call_id: "call-two".to_owned(),
                outcome: Ok(json!({"value": 2})),
            },
        ]));
        history.push(LLMMessage::User("Continue with these results.".to_owned()));
        let second = chat
            .complete(LLMRequest {
                system_prompt: "Use the provided tools.",
                messages: &history,
                tools: &tools,
            })
            .await
            .unwrap();
        server.abort();
        assert_eq!(second.into_text().unwrap(), "Both values are available.");

        let requests = captured.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["system"], "Use the provided tools.");
        assert_eq!(
            requests[0]["tools"],
            json!([{
                "name": "lookup",
                "description": "Look up a value",
                "input_schema": tools[0].input_schema,
                "strict": false,
            }])
        );
        assert_eq!(
            requests[0]["tool_choice"],
            json!({"type": "auto", "disable_parallel_tool_use": false})
        );
        assert_eq!(
            requests[1]["messages"],
            json!([
                {"role": "user", "content": "Find both values."},
                {"role": "assistant", "content": first_content},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call-one", "content": "{\"value\":1}", "is_error": false},
                    {"type": "tool_result", "tool_use_id": "call-two", "content": "{\"value\":2}", "is_error": false},
                    {"type": "text", "text": "Continue with these results."},
                ]},
            ])
        );
    }

    #[tokio::test]
    async fn overloaded_request_is_retried_with_the_same_credentials_and_prompt() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let requests = attempts.clone();
        let router = Router::new().route(
            "/v1/messages",
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let attempt = requests.fetch_add(1, Ordering::SeqCst);
                async move {
                    assert_eq!(headers["authorization"], "Bearer test-anthropic-key");
                    assert_eq!(headers["anthropic-version"], "2023-06-01");
                    assert_eq!(headers["anthropic-workspace-id"], "test-workspace");
                    assert_eq!(
                        body,
                        json!({"model":"test-claude-model", "max_tokens":512,
                            "messages":[{"role":"user","content":"Hello"}]})
                    );
                    if attempt == 0 {
                        (
                            StatusCode::from_u16(529).unwrap(),
                            Json(json!({"error":"busy"})),
                        )
                    } else {
                        (
                            StatusCode::OK,
                            Json(message(
                                json!("end_turn"),
                                json!([{ "type":"text", "text":"Hello back" }]),
                            )),
                        )
                    }
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let mut chat = AnthropicChat::new(config(), retry_config()).unwrap();
        chat.endpoint = format!("http://{address}/v1/messages");

        let result = chat.invoke("Hello").await;
        server.abort();
        assert_eq!(result, Ok("Hello back".into()));
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn http_errors_exclude_the_provider_body() {
        let router = Router::new().route(
            "/v1/messages",
            post(|| async { (StatusCode::UNAUTHORIZED, "private prompt and secret key") }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let mut chat = AnthropicChat::new(config(), retry_config()).unwrap();
        chat.endpoint = format!("http://{address}/v1/messages");

        let error = chat.invoke("private prompt").await.unwrap_err();
        server.abort();
        assert!(matches!(error, LLMError::Http(401)));
        assert!(!error.to_string().contains("private"));
        assert!(!error.to_string().contains("secret"));
    }

    #[test]
    fn truncated_refused_and_non_text_responses_are_errors() {
        let text = json!([{ "type": "text", "text": "partial output" }]);
        for stop_reason in ["max_tokens", "model_context_window_exceeded", "pause_turn"] {
            let body = serde_json::to_vec(&message(json!(stop_reason), text.clone())).unwrap();
            assert!(matches!(
                parse_message(&body),
                Err(LLMError::IncompleteResponse)
            ));
        }
        let refused = serde_json::to_vec(&message(json!("refusal"), text)).unwrap();
        assert!(matches!(parse_message(&refused), Err(LLMError::Refused)));

        for content in [
            json!([{ "type": "tool_use", "name": "unexpected_tool" }]),
            json!([{ "type": "text", "text": 123 }]),
        ] {
            let body = serde_json::to_vec(&message(json!("end_turn"), content)).unwrap();
            assert!(matches!(
                parse_message(&body),
                Err(LLMError::InvalidResponse)
            ));
        }
        assert!(matches!(
            parse_message(b"not JSON"),
            Err(LLMError::InvalidResponse)
        ));
    }

    #[test]
    fn completed_empty_output_is_unsatisfactory_but_not_a_legacy_text_answer() {
        for content in [
            json!([]),
            json!([{"type": "text", "text": ""}]),
            json!([{"type": "thinking", "thinking": "opaque", "signature": "signed"}]),
        ] {
            let body = serde_json::to_vec(&message(json!("end_turn"), content.clone())).unwrap();
            let response = parse_message(&body).unwrap();
            assert!(response.text.is_none());
            assert!(response.tool_calls.is_empty());
            assert_eq!(
                response.replay,
                AssistantReplay::Anthropic(content.as_array().unwrap().clone())
            );
            assert_eq!(response.into_text(), Err(LLMError::InvalidResponse));
        }
    }

    #[test]
    fn malformed_tool_responses_do_not_expose_executable_calls() {
        let call = json!({"type": "tool_use", "id": "call-one", "name": "lookup", "input": {}});
        for content in [
            json!([]),
            json!([{"type": "text", "text": "No call was emitted."}]),
            json!([{"type": "tool_use", "id": "", "name": "lookup", "input": {}}]),
            json!([{"type": "tool_use", "id": "call-one", "input": {}}]),
            json!([{"type": "tool_use", "id": "call-one", "name": "lookup"}]),
            json!([call.clone(), call.clone()]),
            json!([call.clone(), {"type": "server_tool_use", "id": "server-call"}]),
        ] {
            let body = serde_json::to_vec(&message(json!("tool_use"), content)).unwrap();
            assert!(matches!(
                parse_message(&body),
                Err(LLMError::InvalidResponse)
            ));
        }
        let inconsistent =
            serde_json::to_vec(&message(json!("end_turn"), json!([call.clone()]))).unwrap();
        assert!(matches!(
            parse_message(&inconsistent),
            Err(LLMError::InvalidResponse)
        ));
        let refused = serde_json::to_vec(&message(json!("refusal"), json!([call]))).unwrap();
        assert!(matches!(parse_message(&refused), Err(LLMError::Refused)));

        // Argument shape and unknown names are repaired by the agent, with a matching tool error.
        let repairable = serde_json::to_vec(&message(
            json!("tool_use"),
            json!([
                {"type": "tool_use", "id": "repair-call", "name": "", "input": [1, 2]},
            ]),
        ))
        .unwrap();
        let response = parse_message(&repairable).unwrap();
        assert_eq!(response.text, None);
        assert_eq!(
            response.tool_calls[0].arguments,
            ToolArguments::Json(json!([1, 2]))
        );
    }

    #[test]
    fn consecutive_user_messages_are_merged_without_losing_text() {
        let messages = encode_messages(&[
            LLMMessage::User("First request.".to_owned()),
            LLMMessage::User("A correction.".to_owned()),
        ])
        .unwrap();
        assert_eq!(
            messages,
            vec![json!({"role": "user", "content": [
                {"type": "text", "text": "First request."},
                {"type": "text", "text": "A correction."},
            ]})]
        );
        assert!(matches!(
            encode_messages(&[LLMMessage::Assistant(AssistantReplay::OpenAI(vec![])),]),
            Err(LLMError::Configuration(_))
        ));
    }

    #[test]
    fn rejected_calls_have_error_results_with_matching_ids() {
        let messages = encode_messages(&[LLMMessage::ToolResults(vec![ToolResult {
            call_id: "rejected-call".to_owned(),
            outcome: Err(ToolError::RejectedBatch),
        }])])
        .unwrap();
        assert_eq!(
            messages,
            vec![json!({"role": "user", "content": [{
                "type": "tool_result",
                "tool_use_id": "rejected-call",
                "content": ToolError::RejectedBatch.to_string(),
                "is_error": true,
            }]})]
        );
    }

    #[tokio::test]
    async fn invalid_input_and_missing_configuration_fail_before_network() {
        let mut chat = AnthropicChat::new(config(), retry_config()).unwrap();
        chat.endpoint = "http://127.0.0.1:1/unreachable".to_owned();
        assert!(matches!(
            chat.invoke(" \n ").await,
            Err(LLMError::InvalidPrompt)
        ));
        chat.config.api_key = None;
        assert!(matches!(
            chat.invoke("hello").await,
            Err(LLMError::Configuration("ANTHROPIC_API_KEY is required"))
        ));
        chat.config = config();
        chat.config.model = None;
        assert!(matches!(
            chat.invoke("hello").await,
            Err(LLMError::Configuration(
                "TACHIKOMA_ANTHROPIC_MODEL is required"
            ))
        ));
    }
}

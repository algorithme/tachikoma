use serde_json::{Value, json};

use crate::{
    auth::OpenAISubscription,
    common::config::{OpenAIConfig, RetryConfig},
    llm::{
        AssistantReplay, LLMChat, LLMError, LLMMessage, LLMRequest, LLMResponse,
        MAX_RESPONSE_BYTES, ToolArguments, ToolCall, http_client, request_with_retry,
    },
};

/// An explicit choice: a failed subscription request never falls back to API billing.
#[derive(Clone)]
pub(crate) enum OpenAIAuthentication {
    ApiKey,
    Subscription(OpenAISubscription),
}

pub struct OpenAIChat {
    client: reqwest::Client,
    config: OpenAIConfig,
    authentication: OpenAIAuthentication,
    retry: RetryConfig,
    endpoint: String,
}

impl OpenAIChat {
    pub fn with_api_key(config: OpenAIConfig, retry: RetryConfig) -> Result<Self, LLMError> {
        Self::new(config, OpenAIAuthentication::ApiKey, retry)
    }

    /// Restore subscription credentials from the OS credential store only.
    pub async fn with_subscription(
        config: OpenAIConfig,
        retry: RetryConfig,
    ) -> Result<Self, LLMError> {
        let subscription = OpenAISubscription::restore()
            .await
            .map_err(LLMError::Authentication)?;
        Self::new(
            config,
            OpenAIAuthentication::Subscription(subscription),
            retry,
        )
    }

    pub(crate) fn new(
        config: OpenAIConfig,
        authentication: OpenAIAuthentication,
        retry: RetryConfig,
    ) -> Result<Self, LLMError> {
        Ok(Self {
            client: http_client(retry.timeout)?,
            config,
            authentication,
            retry,
            endpoint: "https://api.openai.com/v1/responses".into(),
        })
    }
}

impl LLMChat for OpenAIChat {
    async fn complete(&self, request: LLMRequest<'_>) -> Result<LLMResponse, LLMError> {
        if request.messages.is_empty() {
            return Err(LLMError::InvalidPrompt);
        }
        let model = self.config.model.as_deref().ok_or(LLMError::Configuration(
            "Set TACHIKOMA_OPENAI_MODEL to a model available to your selected account, then restart core.",
        ))?;
        let body = request_body(model, request)?;
        let token =
            match &self.authentication {
                OpenAIAuthentication::ApiKey => self
                    .config
                    .api_key
                    .as_ref()
                    .ok_or(LLMError::Configuration(
                        "Set OPENAI_API_KEY, then restart core.",
                    ))?
                    .expose()
                    .to_owned(),
                OpenAIAuthentication::Subscription(subscription) => subscription
                    .access_token()
                    .await
                    .map_err(LLMError::Authentication)?,
            };

        // History is supplied by the caller; provider-side conversation storage remains disabled.
        request_with_retry(
            self.retry,
            || {
                self.client
                    .post(&self.endpoint)
                    .bearer_auth(&token)
                    .header(reqwest::header::ACCEPT, "text/event-stream")
                    .json(&body)
                    .send()
            },
            read_response,
        )
        .await
    }
}

fn request_body(model: &str, request: LLMRequest<'_>) -> Result<Value, LLMError> {
    let mut input = Vec::new();
    for message in request.messages {
        match message {
            LLMMessage::User(text) => input.push(json!({"role": "user", "content": text})),
            LLMMessage::Assistant(AssistantReplay::OpenAI(items)) => {
                input.extend(items.iter().cloned());
            }
            LLMMessage::Assistant(AssistantReplay::Anthropic(_)) => {
                return Err(LLMError::InvalidResponse);
            }
            LLMMessage::ToolResults(results) => {
                for result in results {
                    let output = match &result.outcome {
                        Ok(value) => value.to_string(),
                        Err(error) => json!({"error": error.to_string()}).to_string(),
                    };
                    input.push(json!({
                        "type": "function_call_output",
                        "call_id": result.call_id,
                        "output": output,
                    }));
                }
            }
        }
    }
    let mut body = json!({
        "model": model,
        "input": input,
        "store": false,
        "stream": true,
    });
    if !request.system_prompt.is_empty() {
        body["instructions"] = json!(request.system_prompt);
    }
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(
            request
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.input_schema,
                        "strict": false,
                    })
                })
                .collect(),
        );
        body["tool_choice"] = json!("auto");
        body["parallel_tool_calls"] = json!(true);
    }
    if !request.system_prompt.is_empty()
        || !request.tools.is_empty()
        || !matches!(request.messages, [LLMMessage::User(_)])
    {
        // Retain opaque reasoning items so later stateless requests can replay them.
        body["include"] = json!(["reasoning.encrypted_content"]);
    }
    Ok(body)
}

async fn read_response(mut response: reqwest::Response) -> Result<LLMResponse, LLMError> {
    if !response.status().is_success() {
        return Err(LLMError::Http(response.status().as_u16()));
    }
    let mut parser = ResponseStream::default();
    while let Some(chunk) = response.chunk().await.map_err(|_| LLMError::Transport)? {
        if let Some(text) = parser.push(&chunk)? {
            return Ok(text);
        }
    }
    Err(LLMError::IncompleteResponse)
}

/// SSE is framed by blank lines, independent of HTTP chunk and UTF-8 boundaries.
#[derive(Default)]
struct ResponseStream {
    line: Vec<u8>,
    data: String,
    received: usize,
    previous_cr: bool,
}

impl ResponseStream {
    fn push(&mut self, bytes: &[u8]) -> Result<Option<LLMResponse>, LLMError> {
        self.received = self.received.saturating_add(bytes.len());
        if self.received > MAX_RESPONSE_BYTES {
            return Err(LLMError::ResponseTooLarge);
        }
        for &byte in bytes {
            if byte == b'\n' && self.previous_cr {
                self.previous_cr = false;
                continue;
            }
            self.previous_cr = byte == b'\r';
            if byte == b'\n' || byte == b'\r' {
                if let Some(text) = self.finish_line()? {
                    return Ok(Some(text));
                }
            } else {
                self.line.push(byte);
            }
        }
        Ok(None)
    }

    fn finish_line(&mut self) -> Result<Option<LLMResponse>, LLMError> {
        let line = String::from_utf8(std::mem::take(&mut self.line))
            .map_err(|_| LLMError::InvalidResponse)?;
        if line.is_empty() {
            if self.data.is_empty() {
                return Ok(None);
            }
            let data = std::mem::take(&mut self.data);
            if data.trim() == "[DONE]" {
                return Err(LLMError::IncompleteResponse);
            }
            let event: Value =
                serde_json::from_str(&data).map_err(|_| LLMError::InvalidResponse)?;
            return match event["type"].as_str() {
                Some("response.completed") => completed_response(&event["response"]).map(Some),
                Some("response.incomplete") => Err(LLMError::IncompleteResponse),
                Some("response.failed" | "error") => Err(LLMError::ProviderFailure),
                Some(_) => Ok(None),
                None => Err(LLMError::InvalidResponse),
            };
        }
        if let Some(data) = line.strip_prefix("data:") {
            if !self.data.is_empty() {
                self.data.push('\n');
            }
            self.data.push_str(data.strip_prefix(' ').unwrap_or(data));
        }
        Ok(None)
    }
}

fn completed_response(response: &Value) -> Result<LLMResponse, LLMError> {
    if response["status"] != "completed" {
        return Err(LLMError::IncompleteResponse);
    }
    let output = response["output"]
        .as_array()
        .ok_or(LLMError::InvalidResponse)?;
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for item in output {
        match item["type"].as_str() {
            Some("reasoning") => continue,
            Some("function_call") => {
                if item
                    .get("status")
                    .is_some_and(|status| status != "completed")
                {
                    return Err(LLMError::IncompleteResponse);
                }
                let id = item["call_id"].as_str().ok_or(LLMError::InvalidResponse)?;
                let name = item["name"].as_str().ok_or(LLMError::InvalidResponse)?;
                if name.trim().is_empty() {
                    return Err(LLMError::InvalidResponse);
                }
                let arguments = item["arguments"]
                    .as_str()
                    .ok_or(LLMError::InvalidResponse)?;
                tool_calls.push(ToolCall {
                    id: id.into(),
                    name: name.into(),
                    arguments: match serde_json::from_str(arguments) {
                        Ok(value) => ToolArguments::Json(value),
                        Err(_) => ToolArguments::InvalidJson,
                    },
                });
                continue;
            }
            Some("message") if item["role"] == "assistant" && item["status"] == "completed" => {}
            _ => return Err(LLMError::InvalidResponse),
        }
        for content in item["content"]
            .as_array()
            .ok_or(LLMError::InvalidResponse)?
        {
            match content["type"].as_str() {
                Some("output_text") => {
                    text.push_str(content["text"].as_str().ok_or(LLMError::InvalidResponse)?)
                }
                Some("refusal") => return Err(LLMError::Refused),
                _ => return Err(LLMError::InvalidResponse),
            }
        }
    }
    let response = LLMResponse {
        text: (!text.is_empty()).then_some(text),
        tool_calls,
        replay: AssistantReplay::OpenAI(output.clone()),
    };
    response.validate_call_ids()?;
    Ok(response)
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use super::*;
    use crate::{
        common::config::SecretString,
        llm::{ToolDefinition, ToolResult},
    };
    use axum::{
        Json, Router,
        http::{HeaderMap, StatusCode},
        routing::post,
    };

    fn retry_config() -> RetryConfig {
        RetryConfig {
            max_retries: 2,
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(1),
            timeout: Duration::from_secs(5),
        }
    }

    fn completed(text: &str) -> String {
        format!(
            "data: {}\n\n",
            json!({"type":"response.completed", "response": {
                "status":"completed", "output":[{"type":"message", "role":"assistant", "status":"completed",
                    "content":[{"type":"output_text", "text":text}]}]
            }})
        )
    }

    #[tokio::test]
    async fn invoke_sends_private_stateless_request_and_returns_completed_text() {
        let app = Router::new().route(
            "/responses",
            post(|headers: HeaderMap, Json(body): Json<Value>| async move {
                assert_eq!(headers["authorization"], "Bearer fake-test-key");
                assert_eq!(
                    body,
                    json!({"model":"test-model", "input":[{"role":"user","content":"Hello"}],
                "store":false,"stream":true})
                );
                completed("Hello back")
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut provider = OpenAIChat::new(
            OpenAIConfig {
                model: Some("test-model".into()),
                preferred_auth: None,
                api_key: Some(SecretString::new("fake-test-key").unwrap()),
            },
            OpenAIAuthentication::ApiKey,
            retry_config(),
        )
        .unwrap();
        provider.endpoint = format!("http://{address}/responses");
        let result = provider.invoke("Hello").await;
        server.abort();
        assert_eq!(result, Ok("Hello back".into()));
    }

    #[tokio::test]
    async fn rejected_request_is_retried_with_the_same_credentials_and_prompt() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let requests = attempts.clone();
        let app = Router::new().route(
            "/responses",
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let attempt = requests.fetch_add(1, Ordering::SeqCst);
                async move {
                    assert_eq!(headers["authorization"], "Bearer fake-test-key");
                    assert_eq!(
                        body,
                        json!({"model":"test-model", "input":[{"role":"user","content":"Hello"}],
                            "store":false,"stream":true})
                    );
                    if attempt == 0 {
                        (StatusCode::TOO_MANY_REQUESTS, "busy".to_owned())
                    } else {
                        (StatusCode::OK, completed("Hello back"))
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut provider = OpenAIChat::new(
            OpenAIConfig {
                model: Some("test-model".into()),
                preferred_auth: None,
                api_key: Some(SecretString::new("fake-test-key").unwrap()),
            },
            OpenAIAuthentication::ApiKey,
            retry_config(),
        )
        .unwrap();
        provider.endpoint = format!("http://{address}/responses");

        let result = provider.invoke("Hello").await;
        server.abort();
        assert_eq!(result, Ok("Hello back".into()));
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn sse_handles_utf8_chunk_boundaries_and_requires_successful_completion() {
        for newline in ["\n", "\r\n", "\r"] {
            let stream = format!(": keepalive\n\ndata: {{\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}}\n\n{}", completed("Grüezi"))
                .replace('\n', newline);
            let mut parser = ResponseStream::default();
            let mut result = None;
            for byte in stream.as_bytes() {
                if let Some(text) = parser.push(&[*byte]).unwrap() {
                    result = Some(text);
                }
            }
            assert_eq!(
                result.and_then(|response| response.text).as_deref(),
                Some("Grüezi")
            );
        }
        for (kind, error) in [
            ("response.failed", LLMError::ProviderFailure),
            ("response.incomplete", LLMError::IncompleteResponse),
        ] {
            let stream = format!(
                "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}}\n\ndata: {{\"type\":\"{kind}\"}}\n\n"
            );
            assert_eq!(
                ResponseStream::default().push(stream.as_bytes()),
                Err(error)
            );
        }
        assert_eq!(
            ResponseStream::default().push(b"data: [DONE]\n\n"),
            Err(LLMError::IncompleteResponse)
        );
    }

    #[tokio::test]
    async fn subscription_does_not_fall_back_to_available_api_key() {
        let provider = OpenAIChat::new(
            OpenAIConfig {
                model: Some("test-model".into()),
                preferred_auth: None,
                api_key: Some(SecretString::new("fake-api-key").unwrap()),
            },
            OpenAIAuthentication::Subscription(OpenAISubscription::empty_for_test()),
            retry_config(),
        )
        .unwrap();
        assert_eq!(
            provider.invoke("Hello").await,
            Err(LLMError::Authentication(
                crate::auth::AuthError::NotConnected
            ))
        );
    }

    #[tokio::test]
    async fn interrupted_stream_does_not_return_partial_text_or_retry() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let requests = attempts.clone();
        let app = Router::new().route("/responses", post(move || {
            requests.fetch_add(1, Ordering::SeqCst);
            async {
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\ndata: {\"type\":\"response.completed\""
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut provider = OpenAIChat::new(
            OpenAIConfig {
                model: Some("test-model".into()),
                preferred_auth: None,
                api_key: Some(SecretString::new("fake-test-key").unwrap()),
            },
            OpenAIAuthentication::ApiKey,
            retry_config(),
        )
        .unwrap();
        provider.endpoint = format!("http://{address}/responses");
        let result = provider.invoke("Hello").await;
        server.abort();
        assert_eq!(result, Err(LLMError::IncompleteResponse));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn final_response_rejects_refusal_and_unrequested_tools() {
        assert_eq!(
            completed_response(&json!({"status":"completed", "output":[{
                "type":"message", "role":"assistant", "status":"completed",
                "content":[{"type":"refusal", "refusal":"Cannot answer"}]
            }]})),
            Err(LLMError::Refused)
        );
        assert_eq!(
            completed_response(&json!({"status":"completed", "output":[{
                "type":"function_call", "name":"unexpected"
            }]})),
            Err(LLMError::InvalidResponse)
        );
    }

    #[tokio::test]
    async fn complete_replays_tool_calls_and_reasoning_with_local_history() {
        let output = json!([
            {"id":"reasoning-1", "type":"reasoning", "summary":[], "encrypted_content":"opaque-test"},
            {"id":"function-1", "type":"function_call", "status":"completed",
             "call_id":"call-1", "name":"lookup", "arguments":"{\"query\":\"hello\"}"}
        ]);
        let schema = json!({
            "type":"object", "properties":{"query":{"type":"string"}}, "required":["query"]
        });
        let attempts = Arc::new(AtomicUsize::new(0));
        let requests = attempts.clone();
        let expected_output = output.clone();
        let expected_schema = schema.clone();
        let app = Router::new().route(
            "/responses",
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let attempt = requests.fetch_add(1, Ordering::SeqCst);
                let output = expected_output.clone();
                let schema = expected_schema.clone();
                async move {
                    assert_eq!(headers["authorization"], "Bearer fake-test-key");
                    assert_eq!(body["store"], false);
                    assert_eq!(body["stream"], true);
                    assert_eq!(body["instructions"], "Use tools when useful.");
                    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
                    assert_eq!(body["tool_choice"], "auto");
                    assert_eq!(body["parallel_tool_calls"], true);
                    assert_eq!(body["tools"], json!([{
                        "type":"function", "name":"lookup", "description":"Look up a value.",
                        "parameters":schema, "strict":false
                    }]));
                    if attempt == 0 {
                        assert_eq!(body["input"], json!([{"role":"user", "content":"Hello"}]));
                        format!("data: {}\n\n", json!({
                            "type":"response.completed",
                            "response":{"status":"completed", "output":output}
                        }))
                    } else {
                        assert_eq!(attempt, 1);
                        assert_eq!(body["input"], json!([
                            {"role":"user", "content":"Hello"},
                            output[0], output[1],
                            {"type":"function_call_output", "call_id":"call-1", "output":"{\"found\":true}"}
                        ]));
                        completed("Found it")
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut provider = OpenAIChat::new(
            OpenAIConfig {
                model: Some("test-model".into()),
                preferred_auth: None,
                api_key: Some(SecretString::new("fake-test-key").unwrap()),
            },
            OpenAIAuthentication::ApiKey,
            retry_config(),
        )
        .unwrap();
        provider.endpoint = format!("http://{address}/responses");
        let tools = [ToolDefinition {
            name: "lookup".into(),
            description: "Look up a value.".into(),
            input_schema: schema,
        }];
        let mut history = vec![LLMMessage::User("Hello".into())];
        let first = provider
            .complete(LLMRequest {
                system_prompt: "Use tools when useful.",
                messages: &history,
                tools: &tools,
            })
            .await
            .unwrap();
        assert_eq!(first.text, None);
        assert_eq!(
            first.tool_calls,
            vec![ToolCall {
                id: "call-1".into(),
                name: "lookup".into(),
                arguments: ToolArguments::Json(json!({"query":"hello"})),
            }]
        );
        assert_eq!(
            first.replay,
            AssistantReplay::OpenAI(output.as_array().unwrap().clone())
        );
        history.push(LLMMessage::Assistant(first.replay));
        history.push(LLMMessage::ToolResults(vec![ToolResult {
            call_id: "call-1".into(),
            outcome: Ok(json!({"found":true})),
        }]));
        let second = provider
            .complete(LLMRequest {
                system_prompt: "Use tools when useful.",
                messages: &history,
                tools: &tools,
            })
            .await;
        server.abort();
        assert_eq!(second.unwrap().into_text(), Ok("Found it".into()));
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn tool_arguments_can_be_invalid_but_tool_identity_must_be_valid() {
        let call = json!({
            "type":"function_call", "status":"completed", "id":"function-1",
            "call_id":"call-1", "name":"answer", "arguments":"{private-invalid-json"
        });
        let response = completed_response(&json!({"status":"completed", "output":[call]})).unwrap();
        assert_eq!(response.tool_calls[0].arguments, ToolArguments::InvalidJson);
        assert_eq!(response.replay, AssistantReplay::OpenAI(vec![call.clone()]));
        assert_eq!(response.into_text(), Err(LLMError::InvalidResponse));

        for invalid in [
            json!({"call_id":""}),
            json!({"call_id":"   "}),
            json!({"call_id":null}),
            json!({"name":""}),
            json!({"arguments":{}}),
        ] {
            let mut invalid_call = call.clone();
            for (key, value) in invalid.as_object().unwrap() {
                invalid_call[key] = value.clone();
            }
            assert_eq!(
                completed_response(&json!({"status":"completed", "output":[invalid_call]})),
                Err(LLMError::InvalidResponse)
            );
        }
        assert_eq!(
            completed_response(&json!({"status":"completed", "output":[call, call]})),
            Err(LLMError::InvalidResponse)
        );
        let mut incomplete = call;
        incomplete["status"] = json!("in_progress");
        assert_eq!(
            completed_response(&json!({"status":"completed", "output":[incomplete]})),
            Err(LLMError::IncompleteResponse)
        );
    }

    #[test]
    fn provider_specific_history_cannot_be_sent_to_another_provider() {
        assert_eq!(
            request_body(
                "test-model",
                LLMRequest {
                    system_prompt: "",
                    messages: &[LLMMessage::Assistant(AssistantReplay::Anthropic(vec![]))],
                    tools: &[],
                }
            ),
            Err(LLMError::InvalidResponse)
        );
    }

    #[test]
    fn completed_empty_or_reasoning_only_outputs_remain_structured_responses() {
        for output in [
            json!([]),
            json!([{"type":"reasoning", "id":"reasoning-1", "summary":[], "encrypted_content":"opaque-test"}]),
            json!([{"type":"message", "role":"assistant", "status":"completed", "content":[]}]),
        ] {
            let response = completed_response(&json!({
                "status":"completed", "output":output
            }))
            .unwrap();
            assert_eq!(response.text, None);
            assert!(response.tool_calls.is_empty());
            assert_eq!(
                response.replay,
                AssistantReplay::OpenAI(output.as_array().unwrap().clone())
            );
            assert_eq!(response.into_text(), Err(LLMError::InvalidResponse));
        }
    }

    #[tokio::test]
    async fn legacy_invoke_rejects_a_completed_empty_answer() {
        let app = Router::new().route("/responses", post(|| async { completed("") }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut provider = OpenAIChat::with_api_key(
            OpenAIConfig {
                model: Some("test-model".into()),
                preferred_auth: None,
                api_key: Some(SecretString::new("fake-test-key").unwrap()),
            },
            retry_config(),
        )
        .unwrap();
        provider.endpoint = format!("http://{address}/responses");
        let result = provider.invoke("Hello").await;
        server.abort();
        assert_eq!(result, Err(LLMError::InvalidResponse));
    }
}

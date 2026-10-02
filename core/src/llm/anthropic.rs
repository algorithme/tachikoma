use serde::Deserialize;
use serde_json::json;

use crate::common::config::{AnthropicConfig, RetryConfig};

use super::{LLMChat, LLMError, http_client, read_limited, request_with_retry};

const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";

/// A stateless text client using an Anthropic API key.
pub(crate) struct AnthropicChat {
    http: reqwest::Client,
    config: AnthropicConfig,
    retry: RetryConfig,
    endpoint: String,
}

impl AnthropicChat {
    pub(crate) fn new(config: AnthropicConfig, retry: RetryConfig) -> Result<Self, LLMError> {
        Ok(Self {
            http: http_client(retry.timeout)?,
            config,
            retry,
            endpoint: MESSAGES_URL.to_owned(),
        })
    }
}

impl LLMChat for AnthropicChat {
    async fn invoke(&self, prompt: &str) -> Result<String, LLMError> {
        if prompt.trim().is_empty() {
            return Err(LLMError::InvalidPrompt);
        }
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

        request_with_retry(
            self.retry,
            || {
                let mut request = self
                    .http
                    .post(&self.endpoint)
                    .bearer_auth(api_key.expose())
                    .header("anthropic-version", "2023-06-01")
                    .json(&json!({
                        "model": model,
                        "max_tokens": self.config.max_tokens,
                        "messages": [{"role": "user", "content": prompt}],
                    }));
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

#[derive(Deserialize)]
struct MessageResponse {
    #[serde(rename = "type")]
    kind: String,
    role: String,
    stop_reason: Option<String>,
    content: Vec<ContentBlock>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text {
        text: String,
    },
    Thinking,
    RedactedThinking,
    #[serde(other)]
    Unsupported,
}

fn parse_message(body: &[u8]) -> Result<String, LLMError> {
    let response: MessageResponse =
        serde_json::from_slice(body).map_err(|_| LLMError::InvalidResponse)?;
    if response.kind != "message" || response.role != "assistant" {
        return Err(LLMError::InvalidResponse);
    }
    match response.stop_reason.as_deref() {
        Some("end_turn" | "stop_sequence") => {}
        Some("refusal") => return Err(LLMError::Refused),
        Some("max_tokens" | "model_context_window_exceeded" | "pause_turn") | None => {
            return Err(LLMError::IncompleteResponse);
        }
        _ => return Err(LLMError::InvalidResponse),
    }

    let mut text = String::new();
    for block in response.content {
        match block {
            ContentBlock::Text { text: block_text } => text.push_str(&block_text),
            ContentBlock::Thinking | ContentBlock::RedactedThinking => {}
            ContentBlock::Unsupported => return Err(LLMError::InvalidResponse),
        }
    }
    if text.is_empty() {
        return Err(LLMError::InvalidResponse);
    }
    Ok(text)
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

    use crate::common::config::SecretString;

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
            json!([]),
            json!([{ "type": "text", "text": "" }]),
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

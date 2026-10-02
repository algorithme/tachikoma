use serde_json::{Value, json};

use crate::{
    auth::OpenAISubscription,
    common::config::{OpenAIConfig, RetryConfig},
    llm::{LLMChat, LLMError, MAX_RESPONSE_BYTES, http_client, request_with_retry},
};

/// An explicit choice: a failed subscription request never falls back to API billing.
#[derive(Clone)]
pub(crate) enum OpenAIAuthentication {
    ApiKey,
    Subscription(OpenAISubscription),
}

pub(crate) struct OpenAIChat {
    client: reqwest::Client,
    config: OpenAIConfig,
    authentication: OpenAIAuthentication,
    retry: RetryConfig,
    endpoint: String,
}

impl OpenAIChat {
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
    async fn invoke(&self, prompt: &str) -> Result<String, LLMError> {
        if prompt.trim().is_empty() {
            return Err(LLMError::InvalidPrompt);
        }
        let model = self.config.model.as_deref().ok_or(LLMError::Configuration(
            "Set TACHIKOMA_OPENAI_MODEL to a model available to your selected account, then restart core.",
        ))?;
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

        // The common subset works for API keys and subscription OAuth. No conversation is stored.
        request_with_retry(
            self.retry,
            || {
                self.client
                    .post(&self.endpoint)
                    .bearer_auth(&token)
                    .header(reqwest::header::ACCEPT, "text/event-stream")
                    .json(&json!({
                        "model": model,
                        "input": [{"role": "user", "content": prompt}],
                        "store": false,
                        "stream": true,
                    }))
                    .send()
            },
            read_response,
        )
        .await
    }
}

async fn read_response(mut response: reqwest::Response) -> Result<String, LLMError> {
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
    fn push(&mut self, bytes: &[u8]) -> Result<Option<String>, LLMError> {
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

    fn finish_line(&mut self) -> Result<Option<String>, LLMError> {
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
                Some("response.completed") => completed_text(&event["response"]).map(Some),
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

fn completed_text(response: &Value) -> Result<String, LLMError> {
    if response["status"] != "completed" {
        return Err(LLMError::IncompleteResponse);
    }
    let output = response["output"]
        .as_array()
        .ok_or(LLMError::InvalidResponse)?;
    let mut text = String::new();
    for item in output {
        match item["type"].as_str() {
            Some("reasoning") => continue,
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
    if text.is_empty() {
        Err(LLMError::InvalidResponse)
    } else {
        Ok(text)
    }
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
    use crate::common::config::SecretString;
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
            assert_eq!(result.as_deref(), Some("Grüezi"));
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
            completed_text(&json!({"status":"completed", "output":[{
                "type":"message", "role":"assistant", "status":"completed",
                "content":[{"type":"refusal", "refusal":"Cannot answer"}]
            }]})),
            Err(LLMError::Refused)
        );
        assert_eq!(
            completed_text(&json!({"status":"completed", "output":[{
                "type":"function_call", "name":"unexpected"
            }]})),
            Err(LLMError::InvalidResponse)
        );
    }
}

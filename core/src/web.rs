use std::{net::SocketAddr, sync::Arc};

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Query, Request, State},
    http::{HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{OnceCell, Semaphore};
use uuid::Uuid;

use crate::{
    auth::{AuthError, OpenAISubscription},
    clients::llm::openai_client::{OpenAIAuthentication, OpenAIChat},
    common::config::{LLMProviderConfig, OpenAIAuthMode, ProviderKind},
    llm::{LLMChat, LLMError, anthropic::AnthropicChat},
};

struct AppState {
    config: LLMProviderConfig,
    host: String,
    origin: String,
    csrf_token: String,
    subscription: OnceCell<OpenAISubscription>,
    invocation: Semaphore,
}

impl AppState {
    async fn subscription(&self) -> Result<&OpenAISubscription, AuthError> {
        self.subscription
            .get_or_try_init(OpenAISubscription::restore)
            .await
    }
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Connection {
    OpenaiApiKey,
    OpenaiSubscription,
    AnthropicApiKey,
}

pub(crate) fn build_router(config: LLMProviderConfig, address: SocketAddr) -> Router {
    router(Arc::new(AppState {
        config,
        host: address.to_string(),
        origin: format!("http://{address}"),
        csrf_token: Uuid::new_v4().to_string(),
        subscription: OnceCell::new(),
        invocation: Semaphore::new(1),
    }))
}

fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(javascript))
        .route("/style.css", get(stylesheet))
        .route("/api/status", get(status))
        .route("/api/invoke", post(invoke))
        .route("/api/openai/login", post(login))
        .route("/api/openai/disconnect", post(disconnect))
        .route("/auth/callback", get(callback))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            protect_local_app,
        ))
        .with_state(state)
}

async fn protect_local_app(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let allowed = {
        let exact_header = |name: &str, expected: &str| {
            let mut values = request.headers().get_all(name).iter();
            values.next().and_then(|value| value.to_str().ok()) == Some(expected)
                && values.next().is_none()
        };
        exact_header("host", &state.host)
            && (request.method() != Method::POST
                || (exact_header("origin", &state.origin)
                    && exact_header("x-csrf-token", &state.csrf_token)))
    };
    let mut response = if allowed {
        next.run(request).await
    } else {
        error(
            StatusCode::FORBIDDEN,
            "This request is not from the local Tachikoma page.",
        )
    };
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("content-security-policy", HeaderValue::from_static(
        "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'"));
    response
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../web/index.html"))
}
async fn javascript() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../web/app.js"),
    )
}
async fn stylesheet() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../web/style.css"),
    )
}

async fn status(State(state): State<Arc<AppState>>) -> Json<Value> {
    let (subscription, subscription_error) = match state.subscription().await {
        Ok(subscription) => (Some(subscription.status().await), None),
        Err(error) => (None, Some(error.to_string())),
    };
    let preferred = match state.config.preferred_provider {
        Some(ProviderKind::Anthropic) => Some(Connection::AnthropicApiKey),
        Some(ProviderKind::OpenAI) => match state.config.openai.preferred_auth {
            Some(OpenAIAuthMode::ApiKey) => Some(Connection::OpenaiApiKey),
            Some(OpenAIAuthMode::Subscription) => Some(Connection::OpenaiSubscription),
            None => None,
        },
        None => None,
    };
    Json(json!({
        "csrf_token": state.csrf_token,
        "preferred_connection": preferred,
        "openai": {
            "model": state.config.openai.model,
            "api_key_present": state.config.openai.api_key.is_some(),
            "subscription": subscription,
            "subscription_error": subscription_error,
        },
        "anthropic": {
            "model": state.config.anthropic.model,
            "api_key_present": state.config.anthropic.api_key.is_some(),
        },
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InvokeRequest {
    connection: Connection,
    prompt: String,
}

async fn invoke(
    State(state): State<Arc<AppState>>,
    Json(request): Json<InvokeRequest>,
) -> Response {
    let Ok(_permit) = state.invocation.try_acquire() else {
        return error(
            StatusCode::CONFLICT,
            "An invocation is already running. Wait for it to finish.",
        );
    };
    if request.prompt.trim().is_empty() {
        return llm_error(LLMError::InvalidPrompt);
    }
    let result = match request.connection {
        Connection::OpenaiApiKey => {
            match OpenAIChat::new(
                state.config.openai.clone(),
                OpenAIAuthentication::ApiKey,
                state.config.retry,
            ) {
                Ok(chat) => chat.invoke(&request.prompt).await,
                Err(error) => Err(error),
            }
        }
        Connection::OpenaiSubscription => {
            let subscription = match state.subscription().await {
                Ok(subscription) => subscription.clone(),
                Err(auth_error) => {
                    return error(StatusCode::SERVICE_UNAVAILABLE, &auth_error.to_string());
                }
            };
            match OpenAIChat::new(
                state.config.openai.clone(),
                OpenAIAuthentication::Subscription(subscription),
                state.config.retry,
            ) {
                Ok(chat) => chat.invoke(&request.prompt).await,
                Err(error) => Err(error),
            }
        }
        Connection::AnthropicApiKey => {
            match AnthropicChat::new(state.config.anthropic.clone(), state.config.retry) {
                Ok(chat) => chat.invoke(&request.prompt).await,
                Err(error) => Err(error),
            }
        }
    };
    match result {
        Ok(text) => Json(json!({"text": text})).into_response(),
        Err(error) => llm_error(error),
    }
}

async fn login(State(state): State<Arc<AppState>>) -> Response {
    let result = match state.subscription().await {
        Ok(subscription) => {
            subscription
                .begin_login(&format!("{}/auth/callback", state.origin))
                .await
        }
        Err(error) => Err(error),
    };
    match result {
        Ok(url) => Json(json!({"url": url})).into_response(),
        Err(auth_error) => error(StatusCode::SERVICE_UNAVAILABLE, &auth_error.to_string()),
    }
}

#[derive(Deserialize)]
struct Callback {
    code: Option<String>,
    state: Option<String>,
    client_id: Option<String>,
    error: Option<String>,
}

async fn callback(
    State(state): State<Arc<AppState>>,
    query: Result<Query<Callback>, axum::extract::rejection::QueryRejection>,
) -> Redirect {
    let complete = match (state.subscription.get(), query) {
        (Some(subscription), Ok(Query(callback))) => {
            if let Some(callback_state) = callback.state {
                if callback.error.is_some() {
                    let _ = subscription.cancel_login(&callback_state).await;
                    false
                } else if let Some(code) = callback.code {
                    subscription
                        .complete_login(&code, &callback_state, callback.client_id.as_deref())
                        .await
                        .is_ok()
                } else {
                    let _ = subscription.cancel_login(&callback_state).await;
                    false
                }
            } else {
                false
            }
        }
        _ => false,
    };
    Redirect::to(if complete {
        "/?login=complete"
    } else {
        "/?login=failed"
    })
}

async fn disconnect(State(state): State<Arc<AppState>>) -> Response {
    let result = match state.subscription().await {
        Ok(subscription) => subscription.disconnect().await,
        Err(error) => Err(error),
    };
    match result {
        Ok(()) => Json(json!({"disconnected": true})).into_response(),
        Err(auth_error) => error(StatusCode::SERVICE_UNAVAILABLE, &auth_error.to_string()),
    }
}

fn llm_error(cause: LLMError) -> Response {
    let status = match cause {
        LLMError::Configuration(_) | LLMError::InvalidPrompt => StatusCode::BAD_REQUEST,
        LLMError::Authentication(
            AuthError::ProviderUnavailable | AuthError::StorageUnavailable,
        ) => StatusCode::SERVICE_UNAVAILABLE,
        LLMError::Authentication(_) => StatusCode::UNAUTHORIZED,
        _ => StatusCode::BAD_GATEWAY,
    };
    error(status, &cause.to_string())
}
fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error": message}))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::config::{AnthropicConfig, OpenAIConfig, SecretString};
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tower::ServiceExt;

    fn app(with_keys: bool) -> Router {
        router(Arc::new(AppState {
            config: LLMProviderConfig {
                preferred_provider: None,
                retry: Default::default(),
                openai: OpenAIConfig {
                    model: None,
                    preferred_auth: None,
                    api_key: with_keys
                        .then(|| SecretString::new("private-openai-test-key").unwrap()),
                },
                anthropic: AnthropicConfig {
                    model: None,
                    max_tokens: 1024,
                    workspace_id: None,
                    api_key: with_keys
                        .then(|| SecretString::new("private-anthropic-test-key").unwrap()),
                },
            },
            host: "127.0.0.1:3000".into(),
            origin: "http://127.0.0.1:3000".into(),
            csrf_token: "test-csrf".into(),
            subscription: OnceCell::new_with(Some(OpenAISubscription::empty_for_test())),
            invocation: Semaphore::new(1),
        }))
    }

    #[tokio::test]
    async fn setup_exposes_status_without_credentials_and_protects_every_response() {
        let app = app(true);
        for path in ["/", "/api/status", "/app.js", "/style.css"] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .header("host", "127.0.0.1:3000")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            assert_eq!(response.headers()["referrer-policy"], "no-referrer");
            assert!(
                response.headers()["content-security-policy"]
                    .to_str()
                    .unwrap()
                    .contains("frame-ancestors 'none'")
            );
            let body = to_bytes(response.into_body(), 65536).await.unwrap();
            let text = std::str::from_utf8(&body).unwrap();
            assert!(!text.contains("private-openai-test-key"));
            assert!(!text.contains("private-anthropic-test-key"));
            if path == "/api/status" {
                let status: Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(status["csrf_token"], "test-csrf");
                assert_eq!(status["preferred_connection"], Value::Null);
                assert_eq!(status["openai"]["api_key_present"], true);
            }
        }
    }

    #[tokio::test]
    async fn forged_requests_cannot_read_status_or_start_billable_work() {
        for (method, path, host, origin, csrf) in [
            ("GET", "/api/status", "evil.example", "", ""),
            (
                "POST",
                "/api/invoke",
                "127.0.0.1:3000",
                "https://evil.example",
                "test-csrf",
            ),
            (
                "POST",
                "/api/openai/login",
                "127.0.0.1:3000",
                "http://127.0.0.1:3000",
                "invalid",
            ),
            (
                "POST",
                "/api/openai/disconnect",
                "127.0.0.1:3000",
                "",
                "test-csrf",
            ),
        ] {
            let response = app(false)
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .header("host", host)
                        .header("origin", origin)
                        .header("x-csrf-token", csrf)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        }
    }

    #[tokio::test]
    async fn missing_configuration_and_invalid_callbacks_fail_locally() {
        for connection in ["openai_api_key", "openai_subscription", "anthropic_api_key"] {
            let response = app(false)
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/invoke")
                        .header("host", "127.0.0.1:3000")
                        .header("origin", "http://127.0.0.1:3000")
                        .header("x-csrf-token", "test-csrf")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(
                            json!({"connection": connection,"prompt":"Hello"}).to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
        let response = app(false)
            .oneshot(
                Request::builder()
                    .uri("/auth/callback?code=private-code&state=forged-state")
                    .header("host", "127.0.0.1:3000")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()[header::LOCATION], "/?login=failed");
    }
}

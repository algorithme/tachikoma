//! Stateless text invocation. Providers own their credentials; callers supply only a prompt.

use std::{fmt, future::Future, time::Duration};

pub(crate) mod anthropic;
mod retry;

pub(super) use retry::request_with_retry;

pub(crate) trait LLMChat: Send + Sync {
    /// Return a completed text response. Each invocation is an independent conversation.
    fn invoke(&self, prompt: &str) -> impl Future<Output = Result<String, LLMError>> + Send;
}

/// Deliberately excludes provider bodies and transport diagnostics, which can contain secrets.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LLMError {
    Configuration(&'static str),
    InvalidPrompt,
    Authentication(crate::auth::AuthError),
    Transport,
    Timeout,
    Http(u16),
    ProviderFailure,
    InvalidResponse,
    IncompleteResponse,
    Refused,
    ResponseTooLarge,
}

impl fmt::Display for LLMError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration(message) => f.write_str(message),
            Self::InvalidPrompt => f.write_str("The prompt must contain text."),
            Self::Authentication(error) => error.fmt(f),
            Self::Transport => f.write_str("Could not complete the provider request."),
            Self::Timeout => f.write_str("The provider request exceeded its time limit."),
            Self::Http(status) => write!(
                f,
                "The provider returned HTTP {status}. Check your selected credentials, model, and usage limits."
            ),
            Self::ProviderFailure => {
                f.write_str("The provider could not finish generation. Check your usage limits.")
            }
            Self::InvalidResponse => f.write_str("The provider returned an unexpected response."),
            Self::IncompleteResponse => f.write_str(
                "The provider did not complete its response. No partial answer was returned.",
            ),
            Self::Refused => f.write_str("The provider declined this request."),
            Self::ResponseTooLarge => f.write_str("The provider response exceeded the size limit."),
        }
    }
}

impl std::error::Error for LLMError {}

pub(super) const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

pub(super) fn http_client(timeout: Duration) -> Result<reqwest::Client, LLMError> {
    reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .no_proxy()
        .build()
        .map_err(|_| LLMError::Transport)
}

pub(super) async fn read_limited(mut response: reqwest::Response) -> Result<Vec<u8>, LLMError> {
    if !response.status().is_success() {
        return Err(LLMError::Http(response.status().as_u16()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| LLMError::Transport)? {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(LLMError::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

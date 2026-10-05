//! Native public-client OAuth for ChatGPT plan usage.
//!
//! Provider settings remain immutable; this module owns renewable credentials.
//! One core process owns the saved registration. Nothing here reads environment
//! variables, writes plaintext credentials, or logs provider responses.

use std::{
    fmt,
    future::Future,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use reqwest::{Client, StatusCode, Url};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use uuid::Uuid;

const ISSUER: &str = "https://auth.openai.com";
const RESOURCE: &str = "https://api.openai.com/v1";
const DYNAMIC_CLIENT: &str = "dynamic_agent_client";
const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const LOGIN_LIFETIME: Duration = Duration::from_secs(600);
const MAX_RESPONSE_BYTES: usize = 128 * 1024;

#[derive(Clone)]
pub(crate) struct OpenAISubscription {
    inner: Arc<Inner>,
}

struct Inner {
    client: Client,
    endpoints: Endpoints,
    store: CredentialStore,
    state: Mutex<AuthState>,
}

struct Endpoints {
    authorize: String,
    token: String,
    jwks: String,
    discovery: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            authorize: format!("{ISSUER}/api/accounts/authorize"),
            token: format!("{ISSUER}/api/accounts/oauth/token"),
            jwks: format!("{ISSUER}/.well-known/jwks.json"),
            discovery: format!("{ISSUER}/.well-known/openid-configuration"),
        }
    }
}

struct AuthState {
    saved: SavedAuth,
    pending: Option<PendingLogin>,
    dirty: bool,
}

#[derive(Clone, Serialize, Deserialize)]
struct SavedAuth {
    host_id: String,
    registration: Option<Registration>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Registration {
    client_id: String,
    // An issued client ID is retained even if the initial code exchange fails.
    // Tokens are never installed until the subject has been verified.
    subject: Option<String>,
    email: Option<String>,
    callback_path: String,
    tokens: Option<Tokens>,
}

// Deliberately no Debug implementations on credential-bearing types.
#[derive(Clone, Serialize, Deserialize)]
struct Tokens {
    access_token: String,
    refresh_token: Option<String>,
    id_token: String,
    nonce: String,
    expires_at: u64,
    scopes: String,
}

impl Tokens {
    fn plan_usage_enabled(&self) -> bool {
        ["chatgpt.tokens.use.direct", "resource.invoke"]
            .iter()
            .all(|required| {
                self.scopes
                    .split_ascii_whitespace()
                    .any(|scope| scope == *required)
            })
    }
}

struct PendingLogin {
    state: String,
    nonce: String,
    verifier: String,
    callback_uri: String,
    client_id: Option<String>,
    created_at: Instant,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SubscriptionStatus {
    pub(crate) connected: bool,
    pub(crate) plan_usage_enabled: bool,
    pub(crate) email: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    StorageUnavailable,
    InvalidCallback,
    ExpiredLogin,
    NotConnected,
    ProviderUnavailable,
    SignInRequired,
    InvalidToken,
    PermissionDenied,
    RemoteRevocationUnconfirmed,
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::StorageUnavailable => "The operating system credential store is unavailable; unlock it and retry.",
            Self::InvalidCallback => "The sign-in callback is invalid; start sign-in again.",
            Self::ExpiredLogin => "The sign-in attempt expired; start sign-in again.",
            Self::NotConnected => "Continue with ChatGPT before using subscription inference.",
            Self::ProviderUnavailable => "OpenAI authentication is temporarily unavailable; retry later.",
            Self::SignInRequired => "The ChatGPT session is no longer usable; sign in again.",
            Self::InvalidToken => "OpenAI sign-in could not be verified; start sign-in again.",
            Self::PermissionDenied => "This sign-in does not permit ChatGPT plan usage; authorize plan usage before chatting.",
            Self::RemoteRevocationUnconfirmed => "Signed out locally, but remote revocation was not confirmed. Disconnect Tachikoma in ChatGPT Settings.",
        })
    }
}

impl std::error::Error for AuthError {}

enum CredentialStore {
    Os(Arc<keyring::Entry>),
    #[cfg(test)]
    Memory(std::sync::Mutex<Option<String>>),
}

impl CredentialStore {
    async fn read(&self) -> Result<Option<SavedAuth>, AuthError> {
        let value = match self {
            Self::Os(entry) => {
                let entry = Arc::clone(entry);
                tokio::task::spawn_blocking(move || match entry.get_password() {
                    Ok(value) => Ok(Some(value)),
                    Err(keyring::Error::NoEntry) => Ok(None),
                    Err(_) => Err(AuthError::StorageUnavailable),
                })
                .await
                .map_err(|_| AuthError::StorageUnavailable)??
            }
            #[cfg(test)]
            Self::Memory(value) => value.lock().unwrap().clone(),
        };
        value
            .map(|value| serde_json::from_str(&value).map_err(|_| AuthError::StorageUnavailable))
            .transpose()
    }

    async fn write(&self, value: &SavedAuth) -> Result<(), AuthError> {
        let value = serde_json::to_string(value).map_err(|_| AuthError::StorageUnavailable)?;
        match self {
            Self::Os(entry) => {
                let entry = Arc::clone(entry);
                tokio::task::spawn_blocking(move || {
                    entry
                        .set_password(&value)
                        .map_err(|_| AuthError::StorageUnavailable)
                })
                .await
                .map_err(|_| AuthError::StorageUnavailable)?
            }
            #[cfg(test)]
            Self::Memory(saved) => {
                *saved.lock().unwrap() = Some(value);
                Ok(())
            }
        }
    }
}

impl OpenAISubscription {
    #[cfg(test)]
    pub(crate) fn empty_for_test() -> Self {
        Self {
            inner: Arc::new(Inner {
                client: Client::builder()
                    .no_proxy()
                    .retry(reqwest::retry::never())
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .unwrap(),
                endpoints: Endpoints::default(),
                store: CredentialStore::Memory(std::sync::Mutex::new(None)),
                state: Mutex::new(AuthState {
                    saved: SavedAuth {
                        host_id: Uuid::new_v4().urn().to_string(),
                        registration: None,
                    },
                    pending: None,
                    dirty: false,
                }),
            }),
        }
    }

    /// Restore only from the OS credential store. Never fall back to plaintext.
    pub(crate) async fn restore() -> Result<Self, AuthError> {
        let entry = keyring::Entry::new("tachikoma-core", "openai-subscription-v1")
            .map_err(|_| AuthError::StorageUnavailable)?;
        let client = Client::builder()
            .https_only(true)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| AuthError::ProviderUnavailable)?;
        Self::with_store(
            client,
            Endpoints::default(),
            CredentialStore::Os(Arc::new(entry)),
        )
        .await
    }

    async fn with_store(
        client: Client,
        endpoints: Endpoints,
        store: CredentialStore,
    ) -> Result<Self, AuthError> {
        let saved = store.read().await?.unwrap_or_else(|| SavedAuth {
            host_id: Uuid::new_v4().urn().to_string(),
            registration: None,
        });
        if Uuid::parse_str(&saved.host_id).is_err()
            || saved.registration.as_ref().is_some_and(|r| {
                !valid_client_id(&r.client_id)
                    || r.subject.as_ref().is_some_and(String::is_empty)
                    || !r.callback_path.starts_with('/')
            })
        {
            return Err(AuthError::StorageUnavailable);
        }
        Ok(Self {
            inner: Arc::new(Inner {
                client,
                endpoints,
                store,
                state: Mutex::new(AuthState {
                    saved,
                    pending: None,
                    dirty: false,
                }),
            }),
        })
    }

    pub(crate) async fn status(&self) -> SubscriptionStatus {
        let state = self.inner.state.lock().await;
        let registration = state.saved.registration.as_ref();
        let tokens = registration.and_then(|r| r.tokens.as_ref());
        SubscriptionStatus {
            connected: tokens.is_some(),
            plan_usage_enabled: tokens.is_some_and(Tokens::plan_usage_enabled),
            email: registration.and_then(|r| r.email.clone()),
        }
    }

    /// Call only after the exact loopback callback listener is bound.
    /// Authorization URLs may contain an ID-token hint: do not log them.
    pub(crate) async fn begin_login(&self, callback_uri: &str) -> Result<String, AuthError> {
        let this = self.clone();
        let callback_uri = callback_uri.to_owned();
        finish_owned(async move { this.begin_login_inner(&callback_uri).await }).await
    }

    async fn begin_login_inner(&self, callback_uri: &str) -> Result<String, AuthError> {
        let callback = validate_callback_uri(callback_uri)?;
        let mut state = self.inner.state.lock().await;
        if state
            .saved
            .registration
            .as_ref()
            .is_some_and(|r| r.callback_path != callback.path())
        {
            return Err(AuthError::InvalidCallback);
        }
        // Persist the stable host ID before the first browser redirect.
        self.inner.store.write(&state.saved).await?;
        state.dirty = false;
        let registration = state.saved.registration.as_ref();
        let pending = PendingLogin {
            state: random_secret(),
            nonce: random_secret(),
            verifier: random_secret(),
            callback_uri: callback_uri.to_owned(),
            client_id: registration.map(|r| r.client_id.clone()),
            created_at: Instant::now(),
        };
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(pending.verifier.as_bytes()));
        let mut url = Url::parse(&self.inner.endpoints.authorize)
            .map_err(|_| AuthError::ProviderUnavailable)?;
        {
            let mut query = url.query_pairs_mut();
            query.extend_pairs([
                (
                    "client_id",
                    pending.client_id.as_deref().unwrap_or(DYNAMIC_CLIENT),
                ),
                ("ext_agent_host_id", state.saved.host_id.as_str()),
                ("response_type", "code"),
                ("redirect_uri", callback_uri),
                ("scope", SCOPES),
                ("resource", RESOURCE),
                ("state", pending.state.as_str()),
                ("nonce", pending.nonce.as_str()),
                ("code_challenge_method", "S256"),
                ("code_challenge", challenge.as_str()),
            ]);
            if let Some(registration) = registration {
                if let Some(tokens) = &registration.tokens {
                    query.append_pair("id_token_hint", &tokens.id_token);
                    if !tokens.plan_usage_enabled() {
                        // This method is reached through the user's sign-in action.
                        query.append_pair("prompt", "consent");
                    }
                }
            } else {
                query.append_pair("agent_name_hint", "Tachikoma");
            }
        }
        state.pending = Some(pending);
        Ok(url.into())
    }

    pub(crate) async fn cancel_login(&self, callback_state: &str) -> Result<(), AuthError> {
        let mut state = self.inner.state.lock().await;
        take_pending(&mut state, callback_state).map(|_| ())
    }

    pub(crate) async fn complete_login(
        &self,
        code: &str,
        callback_state: &str,
        client_id: Option<&str>,
    ) -> Result<(), AuthError> {
        let this = self.clone();
        let (code, callback_state, client_id) = (
            code.to_owned(),
            callback_state.to_owned(),
            client_id.map(str::to_owned),
        );
        finish_owned(async move {
            this.complete_login_inner(&code, &callback_state, client_id.as_deref())
                .await
        })
        .await
    }

    async fn complete_login_inner(
        &self,
        code: &str,
        callback_state: &str,
        client_id: Option<&str>,
    ) -> Result<(), AuthError> {
        let mut state = self.inner.state.lock().await;
        let pending = take_pending(&mut state, callback_state)?;
        if code.is_empty() || code.len() > 16 * 1024 {
            return Err(AuthError::InvalidCallback);
        }
        let client_id = match (pending.client_id.as_deref(), client_id) {
            (Some(expected), None) => expected,
            (Some(expected), Some(actual)) if expected == actual => expected,
            (None, Some(actual)) if valid_client_id(actual) => actual,
            _ => return Err(AuthError::InvalidCallback),
        };
        if state.saved.registration.is_none() {
            state.saved.registration = Some(Registration {
                client_id: client_id.to_owned(),
                subject: None,
                email: None,
                callback_path: validate_callback_uri(&pending.callback_uri)?
                    .path()
                    .to_owned(),
                tokens: None,
            });
            self.persist(&mut state).await?;
        }
        let response = self
            .token_request(&[
                ("grant_type", "authorization_code"),
                ("client_id", client_id),
                ("code", code),
                ("code_verifier", &pending.verifier),
                ("redirect_uri", &pending.callback_uri),
                ("resource", RESOURCE),
            ])
            .await?;
        let id_token = response
            .id_token
            .as_deref()
            .ok_or(AuthError::InvalidToken)?;
        let identity = self
            .verify_identity(id_token, client_id, &pending.nonce, true)
            .await?;
        if state
            .saved
            .registration
            .as_ref()
            .and_then(|r| r.subject.as_deref())
            .is_some_and(|subject| subject != identity.sub)
        {
            return Err(AuthError::InvalidToken);
        }
        let tokens = response.into_tokens(None, &pending.nonce)?;
        state.saved.registration = Some(Registration {
            client_id: client_id.to_owned(),
            subject: Some(identity.sub),
            email: identity.email,
            callback_path: validate_callback_uri(&pending.callback_uri)?
                .path()
                .to_owned(),
            tokens: Some(tokens),
        });
        self.persist(&mut state).await
    }

    /// Refreshes are serialized with sign-in and sign-out, including persistence.
    /// A failed refresh never changes to API-key billing.
    pub(crate) async fn access_token(&self) -> Result<String, AuthError> {
        let this = self.clone();
        finish_owned(async move { this.access_token_inner().await }).await
    }

    async fn access_token_inner(&self) -> Result<String, AuthError> {
        let mut state = self.inner.state.lock().await;
        if state.dirty {
            self.persist(&mut state).await?;
        }
        let registration = state
            .saved
            .registration
            .as_ref()
            .ok_or(AuthError::NotConnected)?;
        let tokens = registration
            .tokens
            .as_ref()
            .ok_or(AuthError::NotConnected)?;
        if !tokens.plan_usage_enabled() {
            return Err(AuthError::PermissionDenied);
        }
        if tokens.expires_at > unix_time()?.saturating_add(60) {
            return Ok(tokens.access_token.clone());
        }
        let Some(refresh_token) = tokens.refresh_token.as_deref() else {
            return Err(AuthError::SignInRequired);
        };
        // Obtain verification keys before consuming a rotating refresh token.
        // A temporary discovery outage must leave the old session retryable.
        let jwks: JwkSet = self.get_json(&self.inner.endpoints.jwks).await?;
        let response = self
            .token_request(&[
                ("grant_type", "refresh_token"),
                ("client_id", &registration.client_id),
                ("refresh_token", refresh_token),
                ("resource", RESOURCE),
            ])
            .await;
        let response = match response {
            Ok(response) => response,
            Err(AuthError::SignInRequired | AuthError::InvalidToken) => {
                state.saved.registration.as_mut().unwrap().tokens = None;
                self.persist(&mut state).await?;
                return Err(AuthError::SignInRequired);
            }
            Err(error) => return Err(error),
        };
        let replacement = (|| {
            if let Some(id_token) = response.id_token.as_deref() {
                let identity = validate_id_token(
                    id_token,
                    &jwks,
                    &registration.client_id,
                    &tokens.nonce,
                    false,
                )?;
                if Some(identity.sub.as_str()) != registration.subject.as_deref() {
                    return Err(AuthError::InvalidToken);
                }
            }
            // Rotation must supply its replacement; never reuse the old token.
            if response
                .refresh_token
                .as_ref()
                .is_none_or(|token| token.is_empty())
            {
                return Err(AuthError::InvalidToken);
            }
            response.into_tokens(Some(tokens), &tokens.nonce)
        })();
        let replacement = match replacement {
            Ok(replacement) => replacement,
            Err(_) => {
                // A successful exchange has consumed the old rotating token.
                // If its replacement cannot be verified, require sign-in instead
                // of replaying the consumed token on the next call.
                state.saved.registration.as_mut().unwrap().tokens = None;
                self.persist(&mut state).await?;
                return Err(AuthError::SignInRequired);
            }
        };
        let allowed = replacement.plan_usage_enabled();
        let access_token = replacement.access_token.clone();
        // Keep the replacement in memory if storage fails. The next call retries
        // persistence before sending anything, rather than rotating the old token.
        state.saved.registration.as_mut().unwrap().tokens = Some(replacement);
        self.persist(&mut state).await?;
        if allowed {
            Ok(access_token)
        } else {
            Err(AuthError::PermissionDenied)
        }
    }

    pub(crate) async fn disconnect(&self) -> Result<(), AuthError> {
        let this = self.clone();
        finish_owned(async move { this.disconnect_inner().await }).await
    }

    async fn disconnect_inner(&self) -> Result<(), AuthError> {
        let mut state = self.inner.state.lock().await;
        state.pending = None;
        let revocation = if let Some(registration) = &state.saved.registration {
            if let Some(refresh) = registration
                .tokens
                .as_ref()
                .and_then(|t| t.refresh_token.as_deref())
            {
                self.revoke(&registration.client_id, refresh).await
            } else {
                Ok(())
            }
        } else {
            Ok(())
        };
        if let Some(registration) = state.saved.registration.as_mut() {
            registration.tokens = None;
        }
        // Retain the host, issued client ID and verified identity for reauthorization.
        self.persist(&mut state).await?;
        revocation
    }

    async fn persist(&self, state: &mut AuthState) -> Result<(), AuthError> {
        state.dirty = true;
        self.inner.store.write(&state.saved).await?;
        state.dirty = false;
        Ok(())
    }

    async fn token_request(&self, form: &[(&str, &str)]) -> Result<TokenResponse, AuthError> {
        let response = self
            .inner
            .client
            .post(&self.inner.endpoints.token)
            .form(form)
            .send()
            .await
            .map_err(|_| AuthError::ProviderUnavailable)?;
        let status = response.status();
        let body = read_bounded(response).await?;
        if status != StatusCode::OK {
            let terminal = serde_json::from_slice::<OAuthFailure>(&body)
                .ok()
                .is_some_and(|error| {
                    matches!(
                        error.error.as_str(),
                        "invalid_grant"
                            | "invalid_refresh_token"
                            | "token_expired"
                            | "refresh_token_expired"
                            | "refresh_token_invalidated"
                            | "refresh_token_reused"
                    )
                });
            return Err(if terminal {
                AuthError::SignInRequired
            } else {
                AuthError::ProviderUnavailable
            });
        }
        serde_json::from_slice(&body).map_err(|_| AuthError::InvalidToken)
    }

    async fn verify_identity(
        &self,
        token: &str,
        client_id: &str,
        nonce: &str,
        require_nonce: bool,
    ) -> Result<IdClaims, AuthError> {
        let jwks: JwkSet = self.get_json(&self.inner.endpoints.jwks).await?;
        validate_id_token(token, &jwks, client_id, nonce, require_nonce)
    }

    async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T, AuthError> {
        let response = self
            .inner
            .client
            .get(url)
            .send()
            .await
            .map_err(|_| AuthError::ProviderUnavailable)?;
        if response.status() != StatusCode::OK {
            return Err(AuthError::ProviderUnavailable);
        }
        serde_json::from_slice(&read_bounded(response).await?)
            .map_err(|_| AuthError::ProviderUnavailable)
    }

    async fn revoke(&self, client_id: &str, refresh_token: &str) -> Result<(), AuthError> {
        let metadata: Discovery = self
            .get_json(&self.inner.endpoints.discovery)
            .await
            .map_err(|_| AuthError::RemoteRevocationUnconfirmed)?;
        let url = Url::parse(&metadata.revocation_endpoint)
            .map_err(|_| AuthError::RemoteRevocationUnconfirmed)?;
        let authorize = Url::parse(&self.inner.endpoints.authorize)
            .map_err(|_| AuthError::RemoteRevocationUnconfirmed)?;
        if metadata.issuer != ISSUER
            || url.origin() != authorize.origin()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(AuthError::RemoteRevocationUnconfirmed);
        }
        for attempt in 0..2 {
            let response = self
                .inner
                .client
                .post(url.clone())
                .form(&[
                    ("token", refresh_token),
                    ("token_type_hint", "refresh_token"),
                    ("client_id", client_id),
                ])
                .send()
                .await;
            match response {
                Ok(response) if response.status() == StatusCode::OK => return Ok(()),
                Ok(response) if !response.status().is_server_error() => break,
                _ if attempt == 0 => tokio::time::sleep(Duration::from_millis(200)).await,
                _ => break,
            }
        }
        Err(AuthError::RemoteRevocationUnconfirmed)
    }
}

/// Dropping a caller (for example, a closed browser tab) detaches this task.
/// It must retain its lock through rotation/revocation and blocking keyring writes.
async fn finish_owned<T: Send + 'static>(
    operation: impl Future<Output = Result<T, AuthError>> + Send + 'static,
) -> Result<T, AuthError> {
    tokio::spawn(operation)
        .await
        .map_err(|_| AuthError::ProviderUnavailable)?
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    token_type: String,
    expires_in: u64,
    scope: Option<String>,
}

impl TokenResponse {
    fn into_tokens(self, previous: Option<&Tokens>, nonce: &str) -> Result<Tokens, AuthError> {
        if !self.token_type.eq_ignore_ascii_case("bearer")
            || self.access_token.is_empty()
            || self.expires_in == 0
            || self.expires_in > 86400
            || self
                .access_token
                .bytes()
                .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        {
            return Err(AuthError::InvalidToken);
        }
        let id_token = self
            .id_token
            .or_else(|| previous.map(|t| t.id_token.clone()))
            .ok_or(AuthError::InvalidToken)?;
        let scopes = self
            .scope
            .or_else(|| previous.map(|t| t.scopes.clone()))
            .ok_or(AuthError::InvalidToken)?;
        Ok(Tokens {
            access_token: self.access_token,
            refresh_token: self.refresh_token.filter(|token| !token.is_empty()),
            id_token,
            nonce: nonce.to_owned(),
            expires_at: unix_time()?.saturating_add(self.expires_in),
            scopes,
        })
    }
}

#[derive(Deserialize)]
struct OAuthFailure {
    error: String,
}

#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    revocation_endpoint: String,
}

#[derive(Clone, Deserialize)]
struct IdClaims {
    sub: String,
    aud: Audience,
    nonce: Option<String>,
    email: Option<String>,
    azp: Option<String>,
}

#[derive(Clone, Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

fn validate_id_token(
    token: &str,
    jwks: &JwkSet,
    client_id: &str,
    nonce: &str,
    require_nonce: bool,
) -> Result<IdClaims, AuthError> {
    let header = decode_header(token).map_err(|_| AuthError::InvalidToken)?;
    if header.alg != Algorithm::RS256 {
        return Err(AuthError::InvalidToken);
    }
    let kid = header.kid.as_deref().ok_or(AuthError::InvalidToken)?;
    let jwk = jwks.find(kid).ok_or(AuthError::InvalidToken)?;
    let key = DecodingKey::from_jwk(jwk).map_err(|_| AuthError::InvalidToken)?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_issuer(&[ISSUER]);
    validation.set_audience(&[client_id]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    validation.validate_nbf = true;
    validation.leeway = 0;
    let claims = decode::<IdClaims>(token, &key, &validation)
        .map_err(|_| AuthError::InvalidToken)?
        .claims;
    let multiple_audiences = matches!(&claims.aud, Audience::Many(values) if values.len() > 1);
    let correct_audience = match &claims.aud {
        Audience::One(value) => value == client_id,
        Audience::Many(values) => values.iter().any(|value| value == client_id),
    };
    if claims.sub.is_empty()
        || !correct_audience
        || (multiple_audiences && claims.azp.as_deref() != Some(client_id))
        || claims.azp.as_deref().is_some_and(|azp| azp != client_id)
        || claims
            .nonce
            .as_deref()
            .is_some_and(|actual| actual != nonce)
        || (require_nonce && claims.nonce.is_none())
    {
        return Err(AuthError::InvalidToken);
    }
    Ok(claims)
}

fn take_pending(state: &mut AuthState, callback_state: &str) -> Result<PendingLogin, AuthError> {
    let pending = state.pending.as_ref().ok_or(AuthError::InvalidCallback)?;
    if pending.state != callback_state {
        return Err(AuthError::InvalidCallback);
    }
    // Matching callbacks consume the attempt even if a later check fails.
    let pending = state.pending.take().unwrap();
    if pending.created_at.elapsed() >= LOGIN_LIFETIME {
        return Err(AuthError::ExpiredLogin);
    }
    Ok(pending)
}

fn validate_callback_uri(value: &str) -> Result<Url, AuthError> {
    let url = Url::parse(value).map_err(|_| AuthError::InvalidCallback)?;
    if url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || url.port_or_known_default() == Some(0)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(AuthError::InvalidCallback);
    }
    Ok(url)
}

fn valid_client_id(value: &str) -> bool {
    !value.is_empty()
        && value != DYNAMIC_CLIENT
        && value.len() <= 512
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn random_secret() -> String {
    // Two OS-random UUIDs provide 244 random bits; the 64-character string also
    // satisfies PKCE's verifier alphabet and length constraints.
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

fn unix_time() -> Result<u64, AuthError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|time| time.as_secs())
        .map_err(|_| AuthError::InvalidToken)
}

async fn read_bounded(mut response: reqwest::Response) -> Result<Vec<u8>, AuthError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| AuthError::ProviderUnavailable)?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(AuthError::InvalidToken);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        body::Bytes,
        extract::State,
        routing::{get, post},
    };
    use jsonwebtoken::{EncodingKey, Header, encode};
    use serde_json::{Value, json};
    use std::collections::HashMap;

    // Test-only RSA key generated for this module; never used by the application.
    const TEST_KEY: &str = r#"-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQCZBjMWo/kKQg6A
gW0tvvZG3UzTEl7I/wztSg2hzKBfNtR/kGaJyJ2ToBJ6HcPh85Dow2W2Ye6LDKXP
vYUF1fhHVlEcv0d4RnlfxRfhVWDhxjZrvotVrXr36weUNbPe6UzC+isRn/rC2Uy9
W0rpxASWjk0qY8H3FnvxphwxhUpHlWTsFdt/7PkuinRo83oep6EhfU9xBPCfiZa4
bw8QEX0kKSYt/bPGAMwEAKnxEOqvFHVhxRo7ENhhN6JCMQlrmsP1sNTpz/W/nMlT
+RsCsvN3rF66sLvQSwOFGPSTh732x65uAmLEErL41RUmfcfhEiEQ18LicrZRoB81
P9ku/2cTAgMBAAECggEAC5lIMw2fuVLTdZMZ2u9QDIQGqDMKwzJt4OiNivePKlhm
F/sygnwV0ZQ/AkmSSOSk1bfF1GKs6ZY+VTY4yH308TdOM58eBDRXS9aNHlPR4Zsc
EeOh/Q8GtJb4fpfpd9p4Kok6zX2yTKWa5vlt/rUQYfcMS0u/boKiR2k/VtOTE0TW
D86t4Aw5BRcgUQLnjWASs10ZPioTXMDAa01ybyBXmPWfQYkWVnosQyiExijIqUBu
rFW3WdwmOPnjbC9rhh1mvt4JxoZxmeOra/fDwAL4IYTpk84qBtW4GKl4Y4Jhc+/P
ZDvMbYri87+Z4RnTUbAPNiu+04w4zVqHM8SPS8cB7QKBgQDSPbV2HPKInHwANfrF
7jQh8YsTr46oXpbTFeqv2Yse4KuRVMMAbKdT3Yr8OusELyEZE9nVzQZZK1OhG/ZT
2K0AT3Quu1YYUX/rwnCo23wZ7OVQcWUY46EPJ0WKqzjczX+m97WWTjmYdtv4UxJ4
KyH9hqYLb7gHx0iz+tc4QYexPwKBgQC6VHYbzwrF2EPtHmxliE/UqUuxUbfBjNPi
yPF8K/9cXffc4uZyilvT3YyJrezcRK468o2eYVZt8JidMYYK6gA/KjQPq54P0G1z
Pqe3ZuZypzJl76NOWwoE8O3SoKkeheblAjvCDvfRlCmF68jgkyh/XQAQEr4rC4XM
JnN/zIwBLQKBgQCtvnGH+sQ0QZ34TqO/jutmYccdraenXQQnzaT1TkK6cdFXeUTt
xN1DYYLKj6ZI/unLX6xWE+l7J2MlcYZQJHBUIVVSiV7NGg0tOLMB2ODmS+/2Wr/B
odwoObwurPGBCBLGWU+nt13Bsfh3b6+Dv6GHcw8lxF3Bj991XYce5ojM4QKBgE2j
3AMCZo7B6qvnPWuhPQJz+KJOVq9DqxaYfGctPh6S2TElKrkXhePmMSU9X7D4vVC6
uOAB1y3ATxdRB3WMw8WOJFtTO22Df9fBh8kYC3ZCzPFExVtk++vLKI3XfCDH0soi
AByAeCrP/e1brjxFlV5ctiX8aVkuV/YVH5Nk/AGRAoGAJBtFF822LBF0pviy4x7g
Rg80tTA4XtYKwpJLQxWt6gFzbu4NaBWpQ3yqSM7i91G3g5DMqNzye7NHNwVQW97u
z5G73ucG9oKW+5DwGIZcqPzL35ciCd/o0kZLUc2ym1EIBb4xUhw8tttuotWlwfSn
jrO5NqXRRuF6G6Lqsiju11g=
-----END PRIVATE KEY-----
"#;
    const TEST_MODULUS: &str = "mQYzFqP5CkIOgIFtLb72Rt1M0xJeyP8M7UoNocygXzbUf5Bmicidk6ASeh3D4fOQ6MNltmHuiwylz72FBdX4R1ZRHL9HeEZ5X8UX4VVg4cY2a76LVa169-sHlDWz3ulMwvorEZ_6wtlMvVtK6cQElo5NKmPB9xZ78aYcMYVKR5Vk7BXbf-z5Lop0aPN6HqehIX1PcQTwn4mWuG8PEBF9JCkmLf2zxgDMBACp8RDqrxR1YcUaOxDYYTeiQjEJa5rD9bDU6c_1v5zJU_kbArLzd6xeurC70EsDhRj0k4e99seubgJixBKy-NUVJn3H4RIhENfC4nK2UaAfNT_ZLv9nEw";

    fn jwks() -> Value {
        json!({"keys": [{"kty": "RSA", "kid": "test-key", "use": "sig", "alg": "RS256", "n": TEST_MODULUS, "e": "AQAB"}]})
    }

    fn claims(nonce: &str, subject: &str) -> Value {
        json!({"iss": ISSUER, "aud": "oaiapp_test", "sub": subject,
            "exp": unix_time().unwrap() + 3600, "nonce": nonce, "email": "person@example.test"})
    }

    fn sign(claims: &Value) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("test-key".to_owned());
        encode(
            &header,
            claims,
            &EncodingKey::from_rsa_pem(TEST_KEY.as_bytes()).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn id_tokens_require_signature_issuer_audience_expiry_and_nonce() {
        let keys = serde_json::from_value(jwks()).unwrap();
        let valid = claims("nonce", "account");
        assert!(validate_id_token(&sign(&valid), &keys, "oaiapp_test", "nonce", true).is_ok());
        for (field, value) in [
            ("iss", json!("https://attacker.example")),
            ("aud", json!("other-client")),
            ("exp", json!(1)),
            ("nonce", json!("other-attempt")),
            ("sub", json!("")),
            ("azp", json!("other-client")),
        ] {
            let mut invalid = valid.clone();
            invalid[field] = value;
            assert!(
                matches!(
                    validate_id_token(&sign(&invalid), &keys, "oaiapp_test", "nonce", true),
                    Err(AuthError::InvalidToken)
                ),
                "accepted invalid {field}"
            );
        }
        let mut missing_nonce = valid.clone();
        missing_nonce.as_object_mut().unwrap().remove("nonce");
        assert!(
            validate_id_token(&sign(&missing_nonce), &keys, "oaiapp_test", "nonce", true).is_err()
        );
        let token = sign(&valid);
        let mut tampered = token.into_bytes();
        let signature = tampered.iter().rposition(|byte| *byte == b'.').unwrap() + 1;
        tampered[signature] = if tampered[signature] == b'A' {
            b'B'
        } else {
            b'A'
        };
        assert!(
            validate_id_token(
                std::str::from_utf8(&tampered).unwrap(),
                &keys,
                "oaiapp_test",
                "nonce",
                true
            )
            .is_err()
        );
    }

    #[derive(Default)]
    struct MockData {
        nonce: String,
        subject: String,
        forms: Vec<HashMap<String, String>>,
        exchange_error: bool,
        refresh_error: Option<(StatusCode, String)>,
        refresh_gate: Option<Arc<RefreshGate>>,
        refresh_id_token: bool,
        jwks_unavailable: bool,
        limited_scope: bool,
        revoked: bool,
    }

    #[derive(Default)]
    struct RefreshGate {
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    struct MockOAuth {
        auth: OpenAISubscription,
        data: Arc<Mutex<MockData>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for MockOAuth {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    fn form_pairs(body: &[u8]) -> HashMap<String, String> {
        let url = Url::parse(&format!(
            "http://example.test/?{}",
            std::str::from_utf8(body).unwrap()
        ))
        .unwrap();
        url.query_pairs().into_owned().collect()
    }

    async fn token_handler(
        State(data): State<Arc<Mutex<MockData>>>,
        body: Bytes,
    ) -> (StatusCode, Json<Value>) {
        let form = form_pairs(&body);
        let mut data = data.lock().await;
        let refresh = form
            .get("grant_type")
            .is_some_and(|grant| grant == "refresh_token");
        data.forms.push(form);
        if refresh {
            if let Some(gate) = &data.refresh_gate {
                gate.entered.notify_one();
                gate.release.notified().await;
            }
            if let Some((status, error)) = &data.refresh_error {
                return (
                    *status,
                    Json(json!({"error": error, "error_description": "DO-NOT-EXPOSE"})),
                );
            }
            let mut response = json!({"access_token": "refreshed-access", "refresh_token": "rotated-refresh",
                "token_type": "Bearer", "expires_in": 3600});
            if data.refresh_id_token {
                response["id_token"] = json!(sign(&claims(&data.nonce, &data.subject)));
            }
            return (StatusCode::OK, Json(response));
        }
        if data.exchange_error {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "invalid_grant"})),
            );
        }
        let scope = if data.limited_scope {
            "openid profile email offline_access"
        } else {
            SCOPES
        };
        (
            StatusCode::OK,
            Json(
                json!({"access_token": "first-access", "refresh_token": "first-refresh",
            "id_token": sign(&claims(&data.nonce, &data.subject)), "token_type": "Bearer", "expires_in": 3600, "scope": scope}),
            ),
        )
    }

    impl MockOAuth {
        async fn new() -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let data = Arc::new(Mutex::new(MockData {
                subject: "account".to_owned(),
                ..MockData::default()
            }));
            let metadata =
                json!({"issuer": ISSUER, "revocation_endpoint": format!("{base}/revoke")});
            let router = Router::new()
                .route("/token", post(token_handler))
                .route(
                    "/jwks",
                    get(|State(data): State<Arc<Mutex<MockData>>>| async move {
                        if data.lock().await.jwks_unavailable {
                            (
                                StatusCode::SERVICE_UNAVAILABLE,
                                Json(json!({"error": "temporary"})),
                            )
                        } else {
                            (StatusCode::OK, Json(jwks()))
                        }
                    }),
                )
                .route(
                    "/discovery",
                    get(move || {
                        let metadata = metadata.clone();
                        async move { Json(metadata) }
                    }),
                )
                .route(
                    "/revoke",
                    post(
                        |State(data): State<Arc<Mutex<MockData>>>, body: Bytes| async move {
                            let form = form_pairs(&body);
                            assert_eq!(form["token_type_hint"], "refresh_token");
                            assert_eq!(form["client_id"], "oaiapp_test");
                            data.lock().await.revoked = true;
                            StatusCode::OK
                        },
                    ),
                )
                .with_state(data.clone());
            let task = tokio::spawn(async move {
                axum::serve(listener, router).await.unwrap();
            });
            let client = Client::builder()
                .no_proxy()
                .retry(reqwest::retry::never())
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap();
            let auth = OpenAISubscription::with_store(
                client,
                Endpoints {
                    authorize: format!("{base}/authorize"),
                    token: format!("{base}/token"),
                    jwks: format!("{base}/jwks"),
                    discovery: format!("{base}/discovery"),
                },
                CredentialStore::Memory(std::sync::Mutex::new(None)),
            )
            .await
            .unwrap();
            Self { auth, data, task }
        }

        async fn begin(&self) -> HashMap<String, String> {
            let url = self
                .auth
                .begin_login("http://127.0.0.1:3000/auth/callback")
                .await
                .unwrap();
            let query: HashMap<String, String> = Url::parse(&url)
                .unwrap()
                .query_pairs()
                .into_owned()
                .collect();
            self.data.lock().await.nonce = query["nonce"].clone();
            query
        }

        async fn login(&self) -> HashMap<String, String> {
            let query = self.begin().await;
            self.auth
                .complete_login("test-code", &query["state"], Some("oaiapp_test"))
                .await
                .unwrap();
            query
        }

        async fn expire(&self) {
            self.auth
                .inner
                .state
                .lock()
                .await
                .saved
                .registration
                .as_mut()
                .unwrap()
                .tokens
                .as_mut()
                .unwrap()
                .expires_at = 1;
        }
    }

    #[tokio::test]
    async fn dynamic_login_uses_pkce_and_reuses_registration_after_logout() {
        let mock = MockOAuth::new().await;
        let query = mock.login().await;
        assert_eq!(query["client_id"], DYNAMIC_CLIENT);
        assert_eq!(query["agent_name_hint"], "Tachikoma");
        assert_eq!(query["resource"], RESOURCE);
        assert_eq!(query["code_challenge_method"], "S256");
        let data = mock.data.lock().await;
        let exchange = &data.forms[0];
        assert_eq!(exchange["client_id"], "oaiapp_test");
        assert_eq!(exchange["redirect_uri"], query["redirect_uri"]);
        assert_eq!(
            URL_SAFE_NO_PAD.encode(Sha256::digest(exchange["code_verifier"].as_bytes())),
            query["code_challenge"]
        );
        assert!(!exchange.contains_key("client_secret"));
        drop(data);
        assert_eq!(mock.auth.access_token().await.unwrap(), "first-access");
        assert_eq!(
            mock.auth
                .complete_login("test-code", &query["state"], Some("oaiapp_test"))
                .await,
            Err(AuthError::InvalidCallback)
        );
        mock.auth.disconnect().await.unwrap();
        assert!(mock.data.lock().await.revoked);
        assert!(!mock.auth.status().await.connected);
        let returning = mock.begin().await;
        assert_eq!(returning["client_id"], "oaiapp_test");
        assert_eq!(returning["ext_agent_host_id"], query["ext_agent_host_id"]);
        assert!(!returning.contains_key("agent_name_hint"));
        assert!(!returning.contains_key("id_token_hint"));
        mock.auth
            .complete_login("returning-code", &returning["state"], None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn failed_initial_exchange_retains_issued_client_without_an_authenticated_account() {
        let mock = MockOAuth::new().await;
        mock.data.lock().await.exchange_error = true;
        let initial = mock.begin().await;
        assert_eq!(
            mock.auth
                .complete_login("expired-code", &initial["state"], Some("oaiapp_test"))
                .await,
            Err(AuthError::SignInRequired)
        );
        assert!(!mock.auth.status().await.connected);
        assert_eq!(mock.auth.access_token().await, Err(AuthError::NotConnected));
        let saved = mock
            .auth
            .inner
            .store
            .read()
            .await
            .unwrap()
            .unwrap()
            .registration
            .unwrap();
        assert_eq!(saved.client_id, "oaiapp_test");
        assert!(saved.subject.is_none());
        assert!(saved.tokens.is_none());
        let retry = mock.begin().await;
        assert_eq!(retry["client_id"], "oaiapp_test");
        assert!(!retry.contains_key("agent_name_hint"));
        mock.data.lock().await.exchange_error = false;
        mock.auth
            .complete_login("fresh-code", &retry["state"], None)
            .await
            .unwrap();
        assert!(mock.auth.status().await.connected);
    }

    #[tokio::test]
    async fn callbacks_reject_mismatch_expiry_and_changed_account_without_overwriting() {
        let mock = MockOAuth::new().await;
        let first = mock.begin().await;
        assert_eq!(
            mock.auth
                .complete_login("code", "wrong-state", Some("oaiapp_test"))
                .await,
            Err(AuthError::InvalidCallback)
        );
        assert!(mock.data.lock().await.forms.is_empty());
        mock.auth
            .complete_login("code", &first["state"], Some("oaiapp_test"))
            .await
            .unwrap();
        let returning = mock.begin().await;
        assert_eq!(
            mock.auth
                .complete_login("code", &returning["state"], Some("other-client"))
                .await,
            Err(AuthError::InvalidCallback)
        );
        let returning = mock.begin().await;
        mock.data.lock().await.subject = "different-account".to_owned();
        assert_eq!(
            mock.auth
                .complete_login("code", &returning["state"], None)
                .await,
            Err(AuthError::InvalidToken)
        );
        assert_eq!(mock.auth.access_token().await.unwrap(), "first-access");
        let expired = mock.begin().await;
        mock.auth
            .inner
            .state
            .lock()
            .await
            .pending
            .as_mut()
            .unwrap()
            .created_at = Instant::now() - LOGIN_LIFETIME;
        assert_eq!(
            mock.auth.cancel_login(&expired["state"]).await,
            Err(AuthError::ExpiredLogin)
        );
        assert_eq!(
            mock.auth.cancel_login(&expired["state"]).await,
            Err(AuthError::InvalidCallback)
        );
    }

    #[tokio::test]
    async fn concurrent_access_rotates_once_and_persists_the_replacement() {
        let mock = MockOAuth::new().await;
        mock.login().await;
        mock.expire().await;
        let (first, second) = tokio::join!(mock.auth.access_token(), mock.auth.access_token());
        assert_eq!(first.unwrap(), "refreshed-access");
        assert_eq!(second.unwrap(), "refreshed-access");
        let data = mock.data.lock().await;
        assert_eq!(data.forms.len(), 2);
        let refresh = &data.forms[1];
        assert_eq!(refresh["grant_type"], "refresh_token");
        assert_eq!(refresh["refresh_token"], "first-refresh");
        assert_eq!(refresh["resource"], RESOURCE);
        assert!(!refresh.contains_key("scope"));
        drop(data);
        let saved = mock.auth.inner.store.read().await.unwrap().unwrap();
        assert_eq!(
            saved
                .registration
                .unwrap()
                .tokens
                .unwrap()
                .refresh_token
                .as_deref(),
            Some("rotated-refresh")
        );
    }

    #[tokio::test]
    async fn canceling_a_caller_does_not_interrupt_rotation_or_persistence() {
        let mock = MockOAuth::new().await;
        mock.login().await;
        mock.expire().await;
        let gate = Arc::new(RefreshGate::default());
        {
            let mut data = mock.data.lock().await;
            data.refresh_gate = Some(gate.clone());
            data.refresh_id_token = true;
        }
        let auth = mock.auth.clone();
        let caller = tokio::spawn(async move { auth.access_token().await });
        tokio::time::timeout(Duration::from_secs(5), gate.entered.notified())
            .await
            .unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        gate.release.notify_one();
        assert_eq!(mock.auth.access_token().await.unwrap(), "refreshed-access");
        assert_eq!(mock.data.lock().await.forms.len(), 2);
        let saved = mock.auth.inner.store.read().await.unwrap().unwrap();
        assert_eq!(
            saved
                .registration
                .unwrap()
                .tokens
                .unwrap()
                .refresh_token
                .as_deref(),
            Some("rotated-refresh")
        );
    }

    #[tokio::test]
    async fn jwks_outage_does_not_consume_the_refresh_token() {
        let mock = MockOAuth::new().await;
        mock.login().await;
        mock.expire().await;
        {
            let mut data = mock.data.lock().await;
            data.jwks_unavailable = true;
            data.refresh_id_token = true;
        }
        assert_eq!(
            mock.auth.access_token().await,
            Err(AuthError::ProviderUnavailable)
        );
        assert!(mock.auth.status().await.connected);
        assert_eq!(mock.data.lock().await.forms.len(), 1);
        mock.data.lock().await.jwks_unavailable = false;
        assert_eq!(mock.auth.access_token().await.unwrap(), "refreshed-access");
        assert_eq!(mock.data.lock().await.forms.len(), 2);
    }

    #[tokio::test]
    async fn temporary_refresh_errors_preserve_credentials_but_revocation_clears_them() {
        let mock = MockOAuth::new().await;
        mock.login().await;
        mock.expire().await;
        mock.data.lock().await.refresh_error =
            Some((StatusCode::SERVICE_UNAVAILABLE, "temporary".to_owned()));
        assert_eq!(
            mock.auth.access_token().await,
            Err(AuthError::ProviderUnavailable)
        );
        assert!(mock.auth.status().await.connected);
        mock.data.lock().await.refresh_error =
            Some((StatusCode::BAD_REQUEST, "refresh_token_reused".to_owned()));
        assert_eq!(
            mock.auth.access_token().await,
            Err(AuthError::SignInRequired)
        );
        assert!(!mock.auth.status().await.connected);
        let query = mock.begin().await;
        assert_eq!(query["client_id"], "oaiapp_test");
    }

    #[tokio::test]
    async fn identity_without_plan_permission_is_saved_but_cannot_invoke() {
        let mock = MockOAuth::new().await;
        mock.data.lock().await.limited_scope = true;
        mock.login().await;
        let status = mock.auth.status().await;
        assert!(status.connected);
        assert!(!status.plan_usage_enabled);
        assert_eq!(
            mock.auth.access_token().await,
            Err(AuthError::PermissionDenied)
        );
        let status_json = serde_json::to_string(&status).unwrap();
        assert!(!status_json.contains("token"));
        assert!(!status_json.contains("first-access"));
        assert_eq!(mock.begin().await["prompt"], "consent");
    }

    #[test]
    fn callback_must_be_an_unadorned_ipv4_loopback_http_url() {
        for uri in [
            "http://localhost:3000/auth/callback",
            "https://127.0.0.1/auth/callback",
            "http://127.0.0.1:0/auth/callback",
            "http://user@127.0.0.1/auth/callback",
            "http://127.0.0.1/auth/callback?code=old",
            "http://127.0.0.1/auth/callback#fragment",
            "http://example.test/auth/callback",
        ] {
            assert!(validate_callback_uri(uri).is_err(), "accepted {uri}");
        }
        assert!(validate_callback_uri("http://127.0.0.1:54321/auth/callback").is_ok());
    }
}

use std::{
    env, fmt, io,
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};

pub(crate) struct Config {
    port: u16,
    pub(crate) llm: LLMProviderConfig,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProviderKind {
    OpenAI,
    Anthropic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenAIAuthMode {
    ApiKey,
    Subscription,
}

#[derive(Clone, Debug)]
pub(crate) struct LLMProviderConfig {
    pub(crate) preferred_provider: Option<ProviderKind>,
    pub(crate) openai: OpenAIConfig,
    pub(crate) anthropic: AnthropicConfig,
    pub(crate) retry: RetryConfig,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetryConfig {
    pub max_retries: u32,
    pub initial_delay: Duration,
    pub max_delay: Duration,
    pub timeout: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 2,
            initial_delay: Duration::from_millis(500),
            max_delay: Duration::from_millis(5000),
            timeout: Duration::from_millis(120_000),
        }
    }
}

impl RetryConfig {
    pub(crate) fn is_valid(&self) -> bool {
        !self.initial_delay.is_zero()
            && self.initial_delay <= self.max_delay
            && self.max_delay <= self.timeout
            && self.timeout <= Duration::from_secs(3600)
    }

    fn from_lookup(
        lookup: &mut impl FnMut(&str) -> io::Result<Option<String>>,
    ) -> io::Result<Self> {
        let defaults = Self::default();
        let max_retries = match lookup("TACHIKOMA_LLM_MAX_RETRIES")? {
            None => defaults.max_retries,
            Some(value) => value.parse::<u32>().map_err(|_| {
                invalid_input("TACHIKOMA_LLM_MAX_RETRIES must be a nonnegative integer")
            })?,
        };
        let config = Self {
            max_retries,
            initial_delay: parse_milliseconds(
                "TACHIKOMA_LLM_RETRY_INITIAL_DELAY_MS",
                lookup("TACHIKOMA_LLM_RETRY_INITIAL_DELAY_MS")?,
                defaults.initial_delay,
            )?,
            max_delay: parse_milliseconds(
                "TACHIKOMA_LLM_RETRY_MAX_DELAY_MS",
                lookup("TACHIKOMA_LLM_RETRY_MAX_DELAY_MS")?,
                defaults.max_delay,
            )?,
            timeout: parse_milliseconds(
                "TACHIKOMA_LLM_TIMEOUT_MS",
                lookup("TACHIKOMA_LLM_TIMEOUT_MS")?,
                defaults.timeout,
            )?,
        };
        if config.initial_delay > config.max_delay {
            return Err(invalid_input(
                "TACHIKOMA_LLM_RETRY_INITIAL_DELAY_MS must not exceed TACHIKOMA_LLM_RETRY_MAX_DELAY_MS",
            ));
        }
        if config.max_delay > config.timeout {
            return Err(invalid_input(
                "TACHIKOMA_LLM_RETRY_MAX_DELAY_MS must not exceed TACHIKOMA_LLM_TIMEOUT_MS",
            ));
        }
        if config.timeout > Duration::from_secs(3600) {
            return Err(invalid_input(
                "TACHIKOMA_LLM_TIMEOUT_MS must not exceed one hour",
            ));
        }
        Ok(config)
    }
}

#[derive(Clone, Debug)]
pub struct OpenAIConfig {
    pub model: Option<String>,
    pub preferred_auth: Option<OpenAIAuthMode>,
    pub api_key: Option<SecretString>,
}

#[derive(Clone, Debug)]
pub struct AnthropicConfig {
    pub model: Option<String>,
    pub api_key: Option<SecretString>,
    pub max_tokens: u32,
    pub workspace_id: Option<String>,
}

/// A backend-only credential whose debug output never contains its value.
#[derive(Clone)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> io::Result<Self> {
        let value = value.into();
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(invalid_input(
                "credential must contain only printable ASCII characters without whitespace",
            ));
        }
        Ok(Self(value))
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretString([REDACTED])")
    }
}

impl Config {
    pub(crate) fn from_env() -> io::Result<Self> {
        Self::from_lookup(|name| match env::var(name) {
            Ok(value) => Ok(Some(value)),
            Err(env::VarError::NotPresent) => Ok(None),
            Err(env::VarError::NotUnicode(_)) => {
                Err(invalid_input(format!("{name} must be valid Unicode")))
            }
        })
    }

    fn from_lookup(mut lookup: impl FnMut(&str) -> io::Result<Option<String>>) -> io::Result<Self> {
        let port = parse_port(lookup("TACHIKOMA_PORT")?.as_deref())?;
        let preferred_provider = match lookup("TACHIKOMA_LLM_PROVIDER")?.as_deref() {
            None | Some("") => None,
            Some("openai") => Some(ProviderKind::OpenAI),
            Some("anthropic") => Some(ProviderKind::Anthropic),
            Some(_) => {
                return Err(invalid_input(
                    "TACHIKOMA_LLM_PROVIDER must be openai or anthropic",
                ));
            }
        };
        let preferred_auth = match lookup("TACHIKOMA_OPENAI_AUTH_MODE")?.as_deref() {
            None | Some("") => None,
            Some("api_key") => Some(OpenAIAuthMode::ApiKey),
            Some("subscription") => Some(OpenAIAuthMode::Subscription),
            Some(_) => {
                return Err(invalid_input(
                    "TACHIKOMA_OPENAI_AUTH_MODE must be api_key or subscription",
                ));
            }
        };
        let max_tokens = match lookup("TACHIKOMA_ANTHROPIC_MAX_TOKENS")? {
            None => 1024,
            Some(value) => value
                .parse::<u32>()
                .ok()
                .filter(|value| *value > 0)
                .ok_or_else(|| {
                    invalid_input("TACHIKOMA_ANTHROPIC_MAX_TOKENS must be a positive integer")
                })?,
        };
        let retry = RetryConfig::from_lookup(&mut lookup)?;

        Ok(Self {
            port,
            llm: LLMProviderConfig {
                preferred_provider,
                retry,
                openai: OpenAIConfig {
                    model: parse_optional_text(
                        "TACHIKOMA_OPENAI_MODEL",
                        lookup("TACHIKOMA_OPENAI_MODEL")?,
                    )?,
                    preferred_auth,
                    api_key: parse_api_key("OPENAI_API_KEY", lookup("OPENAI_API_KEY")?)?,
                },
                anthropic: AnthropicConfig {
                    model: parse_optional_text(
                        "TACHIKOMA_ANTHROPIC_MODEL",
                        lookup("TACHIKOMA_ANTHROPIC_MODEL")?,
                    )?,
                    api_key: parse_api_key("ANTHROPIC_API_KEY", lookup("ANTHROPIC_API_KEY")?)?,
                    max_tokens,
                    workspace_id: parse_optional_text(
                        "ANTHROPIC_WORKSPACE_ID",
                        lookup("ANTHROPIC_WORKSPACE_ID")?,
                    )?,
                },
            },
        })
    }

    pub(crate) fn bind_addr(&self) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, self.port))
    }
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn parse_milliseconds(
    name: &str,
    value: Option<String>,
    default: Duration,
) -> io::Result<Duration> {
    let Some(value) = value else {
        return Ok(default);
    };
    value
        .parse::<u64>()
        .ok()
        .filter(|milliseconds| *milliseconds > 0)
        .map(Duration::from_millis)
        .ok_or_else(|| invalid_input(format!("{name} must be a positive integer in milliseconds")))
}

fn parse_optional_text(name: &str, value: Option<String>) -> io::Result<Option<String>> {
    match value {
        None => Ok(None),
        Some(value) if value.is_empty() => Ok(None),
        Some(value) if value.bytes().all(|byte| byte.is_ascii_graphic()) => Ok(Some(value)),
        Some(_) => Err(invalid_input(format!(
            "{name} must contain only printable ASCII characters without whitespace"
        ))),
    }
}

fn parse_api_key(name: &str, value: Option<String>) -> io::Result<Option<SecretString>> {
    parse_optional_text(name, value)?
        .map(SecretString::new)
        .transpose()
}

fn parse_port(value: Option<&str>) -> io::Result<u16> {
    let Some(value) = value else {
        return Ok(3000);
    };

    value
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "TACHIKOMA_PORT must be a port from 1 to 65535",
            )
        })
}

#[cfg(test)]
mod tests {
    use super::{Config, OpenAIAuthMode, ProviderKind, RetryConfig, SecretString, parse_port};
    use std::{
        io::{self, ErrorKind},
        time::Duration,
    };

    fn config(values: &[(&str, &str)]) -> io::Result<Config> {
        Config::from_lookup(|name| {
            Ok(values
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned()))
        })
    }

    #[test]
    fn retries_have_bounded_defaults_and_explicit_overrides() {
        assert_eq!(
            config(&[]).unwrap().llm.retry,
            RetryConfig {
                max_retries: 2,
                initial_delay: Duration::from_millis(500),
                max_delay: Duration::from_millis(5000),
                timeout: Duration::from_millis(120_000),
            }
        );
        assert_eq!(
            config(&[
                ("TACHIKOMA_LLM_MAX_RETRIES", "3"),
                ("TACHIKOMA_LLM_RETRY_INITIAL_DELAY_MS", "100"),
                ("TACHIKOMA_LLM_RETRY_MAX_DELAY_MS", "1000"),
                ("TACHIKOMA_LLM_TIMEOUT_MS", "10000"),
            ])
            .unwrap()
            .llm
            .retry,
            RetryConfig {
                max_retries: 3,
                initial_delay: Duration::from_millis(100),
                max_delay: Duration::from_millis(1000),
                timeout: Duration::from_millis(10000),
            }
        );
        assert_eq!(
            config(&[("TACHIKOMA_LLM_MAX_RETRIES", "0")])
                .unwrap()
                .llm
                .retry,
            RetryConfig {
                max_retries: 0,
                ..RetryConfig::default()
            }
        );
    }

    #[test]
    fn retry_delays_must_fit_within_the_timeout() {
        for (name, value) in [
            ("TACHIKOMA_LLM_RETRY_INITIAL_DELAY_MS", "6000"),
            ("TACHIKOMA_LLM_RETRY_MAX_DELAY_MS", "400"),
            ("TACHIKOMA_LLM_RETRY_MAX_DELAY_MS", "120001"),
            ("TACHIKOMA_LLM_TIMEOUT_MS", "4999"),
            ("TACHIKOMA_LLM_TIMEOUT_MS", "3600001"),
        ] {
            let error = config(&[(name, value)]).err().unwrap();
            assert_eq!(error.kind(), ErrorKind::InvalidInput);
            assert!(error.to_string().contains(name));
            assert!(!error.to_string().contains(value));
        }
        let upper_bound = config(&[
            ("TACHIKOMA_LLM_RETRY_INITIAL_DELAY_MS", "3600000"),
            ("TACHIKOMA_LLM_RETRY_MAX_DELAY_MS", "3600000"),
            ("TACHIKOMA_LLM_TIMEOUT_MS", "3600000"),
        ])
        .unwrap()
        .llm
        .retry;
        assert_eq!(upper_bound.initial_delay, upper_bound.max_delay);
        assert_eq!(upper_bound.max_delay, upper_bound.timeout);
        assert_eq!(upper_bound.timeout, Duration::from_secs(3600));
    }

    #[test]
    fn malformed_retry_settings_are_rejected_without_echoing_values() {
        for (name, value) in [
            ("TACHIKOMA_LLM_MAX_RETRIES", "-1"),
            ("TACHIKOMA_LLM_MAX_RETRIES", "4294967296"),
            ("TACHIKOMA_LLM_MAX_RETRIES", "1.5"),
            ("TACHIKOMA_LLM_MAX_RETRIES", ""),
            ("TACHIKOMA_LLM_RETRY_INITIAL_DELAY_MS", "0"),
            ("TACHIKOMA_LLM_RETRY_MAX_DELAY_MS", "-1"),
            ("TACHIKOMA_LLM_TIMEOUT_MS", ""),
            ("TACHIKOMA_LLM_TIMEOUT_MS", " 10000 "),
            ("TACHIKOMA_LLM_TIMEOUT_MS", "not-a-number"),
            ("TACHIKOMA_LLM_TIMEOUT_MS", "18446744073709551616"),
        ] {
            let error = config(&[(name, value)]).err().unwrap();
            assert_eq!(error.kind(), ErrorKind::InvalidInput);
            assert!(error.to_string().contains(name));
            if !value.is_empty() {
                assert!(!error.to_string().contains(value));
            }
        }
    }

    #[test]
    fn missing_credentials_allow_onboarding_without_choosing_a_provider_or_model() {
        let config = config(&[]).unwrap();
        assert_eq!(config.llm.preferred_provider, None);
        assert_eq!(config.llm.openai.preferred_auth, None);
        assert!(config.llm.openai.api_key.is_none());
        assert!(config.llm.openai.model.is_none());
        assert!(config.llm.anthropic.api_key.is_none());
        assert!(config.llm.anthropic.model.is_none());
        assert_eq!(config.llm.anthropic.max_tokens, 1024);
    }

    #[test]
    fn an_api_key_does_not_select_or_override_openai_subscription_auth() {
        let unselected = config(&[("OPENAI_API_KEY", "test-openai-key")]).unwrap();
        assert_eq!(unselected.llm.openai.preferred_auth, None);
        let selected = config(&[
            ("TACHIKOMA_LLM_PROVIDER", "openai"),
            ("TACHIKOMA_OPENAI_AUTH_MODE", "subscription"),
            ("TACHIKOMA_OPENAI_MODEL", "test-openai-model"),
            ("OPENAI_API_KEY", "test-openai-key"),
            ("TACHIKOMA_ANTHROPIC_MODEL", "test-anthropic-model"),
            ("ANTHROPIC_API_KEY", "test-anthropic-key"),
            ("TACHIKOMA_ANTHROPIC_MAX_TOKENS", "2048"),
            ("ANTHROPIC_WORKSPACE_ID", "test-workspace"),
        ])
        .unwrap();
        assert_eq!(selected.llm.preferred_provider, Some(ProviderKind::OpenAI));
        assert_eq!(
            selected.llm.openai.preferred_auth,
            Some(OpenAIAuthMode::Subscription)
        );
        assert_eq!(
            selected.llm.openai.api_key.as_ref().unwrap().expose(),
            "test-openai-key"
        );
        assert_eq!(selected.llm.anthropic.max_tokens, 2048);
        assert_eq!(
            selected.llm.anthropic.workspace_id.as_deref(),
            Some("test-workspace")
        );
        let debug = format!("{:?}", selected.llm);
        assert!(!debug.contains("test-openai-key"));
        assert!(!debug.contains("test-anthropic-key"));
    }

    #[test]
    fn invalid_configuration_is_rejected_without_echoing_values() {
        for (name, value) in [
            ("TACHIKOMA_LLM_PROVIDER", "invalid-provider"),
            ("TACHIKOMA_OPENAI_AUTH_MODE", "invalid-auth"),
            ("OPENAI_API_KEY", "secret with whitespace"),
            ("ANTHROPIC_API_KEY", "secret\r\ninjected-header"),
            ("TACHIKOMA_OPENAI_MODEL", "model\ninjected"),
            ("ANTHROPIC_WORKSPACE_ID", "workspace\ninjected"),
            ("TACHIKOMA_ANTHROPIC_MAX_TOKENS", "0"),
        ] {
            let error = config(&[(name, value)]).err().unwrap();
            assert_eq!(error.kind(), ErrorKind::InvalidInput);
            assert!(error.to_string().contains(name));
            assert!(!error.to_string().contains(value));
        }
        assert!(SecretString::new("").is_err());
        assert!(SecretString::new("secret\n").is_err());
    }

    #[test]
    fn port_defaults_and_validation() {
        let cases = [
            (None, Ok(3000)),
            (Some("1"), Ok(1)),
            (Some("3000"), Ok(3000)),
            (Some("65535"), Ok(65535)),
            (Some(""), Err(ErrorKind::InvalidInput)),
            (Some("abc"), Err(ErrorKind::InvalidInput)),
            (Some("-1"), Err(ErrorKind::InvalidInput)),
            (Some("0"), Err(ErrorKind::InvalidInput)),
            (Some("65536"), Err(ErrorKind::InvalidInput)),
            (Some(" 3000 "), Err(ErrorKind::InvalidInput)),
        ];

        for (input, expected) in cases {
            assert_eq!(
                parse_port(input).map_err(|error| error.kind()),
                expected,
                "TACHIKOMA_PORT={input:?}",
            );
        }
    }
}

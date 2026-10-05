//! Retry initial HTTP rejections only. Reading a response is never replayed.

use std::{
    future::Future,
    time::{Duration, SystemTime},
};

use backon::{ExponentialBuilder, Retryable};
use reqwest::{
    Response,
    header::{HeaderMap, RETRY_AFTER},
};
use tokio::time::{Instant, timeout_at};

use crate::common::config::RetryConfig;

use super::LLMError;

struct AttemptError {
    attempt: u64,
    error: LLMError,
    retry_after: Option<Duration>,
}

/// One deadline covers all HTTP attempts, backoff waits, and response reading.
pub(crate) async fn request_with_retry<F, Fut, R, ReadFut, T>(
    config: RetryConfig,
    mut send: F,
    read: R,
) -> Result<T, LLMError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Response, reqwest::Error>>,
    R: FnOnce(Response) -> ReadFut,
    ReadFut: Future<Output = Result<T, LLMError>>,
{
    if !config.is_valid() {
        return Err(LLMError::Configuration(
            "Retry delays must be positive and ordered within a timeout of at most one hour.",
        ));
    }
    let started = Instant::now();
    let deadline = started + config.timeout;
    let mut attempts = 0_u64;
    tracing::debug!(
        max_retries = config.max_retries,
        timeout_ms = config.timeout.as_secs_f64() * 1000.0,
        "Starting provider request"
    );
    let result = timeout_at(deadline, async {
        let backoff = ExponentialBuilder::default()
            .with_min_delay(config.initial_delay)
            .with_max_delay(config.max_delay)
            .with_max_times(config.max_retries as usize)
            .with_jitter();

        let response = (|| {
            attempts += 1;
            let attempt_number = attempts;
            let attempt_started = Instant::now();
            tracing::debug!(attempt = attempt_number, "Starting provider HTTP attempt");
            let attempt = send();
            async move {
                let response = attempt.await.map_err(|error| {
                    let timed_out = error.is_timeout();
                    tracing::debug!(
                        attempt = attempt_number,
                        error_kind = if timed_out { "timeout" } else { "transport" },
                        elapsed_ms = attempt_started.elapsed().as_secs_f64() * 1000.0,
                        "Provider HTTP attempt failed"
                    );
                    AttemptError {
                        attempt: attempt_number,
                        error: if timed_out {
                            LLMError::Timeout
                        } else {
                            LLMError::Transport
                        },
                        retry_after: None,
                    }
                })?;
                tracing::debug!(
                    attempt = attempt_number,
                    status = response.status().as_u16(),
                    elapsed_ms = attempt_started.elapsed().as_secs_f64() * 1000.0,
                    "Received provider HTTP response"
                );
                if response.status().is_success() {
                    Ok(response)
                } else {
                    // Keep only status and retry timing, never the provider body or raw diagnostics.
                    Err(AttemptError {
                        attempt: attempt_number,
                        error: LLMError::Http(response.status().as_u16()),
                        retry_after: retry_after(response.headers(), SystemTime::now()),
                    })
                }
            }
        })
        .retry(backoff)
        .when(|error| matches!(error.error, LLMError::Http(429 | 503 | 529)))
        .adjust(|error, delay| {
            // `None` means the retry count is exhausted; a header must not revive it.
            // BackON adds jitter after its cap, so cap that result before honoring the server.
            let Some(delay) = delay else {
                tracing::debug!(attempts = error.attempt, "Provider retry limit reached");
                return None;
            };
            let delay = delay
                .min(config.max_delay)
                .max(error.retry_after.unwrap_or_default());
            // A server delay may exceed our backoff cap. Never retry earlier to fit the deadline.
            let remaining = deadline.saturating_duration_since(Instant::now());
            if delay >= remaining {
                tracing::debug!(
                    attempts = error.attempt,
                    delay_ms = delay.as_secs_f64() * 1000.0,
                    remaining_ms = remaining.as_secs_f64() * 1000.0,
                    "Provider retry wait exceeds remaining deadline"
                );
                return None;
            }
            if let LLMError::Http(status) = error.error {
                tracing::info!(
                    next_attempt = error.attempt + 1,
                    status,
                    delay_ms = delay.as_secs_f64() * 1000.0,
                    "Waiting before retrying provider request"
                );
            }
            Some(delay)
        })
        .await
        .map_err(|error| error.error)?;

        read(response).await
    })
    .await;
    tracing::debug!(
        attempts,
        elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
        succeeded = matches!(&result, Ok(Ok(_))),
        deadline_exceeded = result.is_err(),
        "Provider request finished"
    );
    result.map_err(|_| LLMError::Timeout)?
}

fn retry_after(headers: &HeaderMap, now: SystemTime) -> Option<Duration> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    httpdate::parse_http_date(value)
        .ok()
        .map(|date| date.duration_since(now).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        future::{pending, ready},
    };

    use super::*;

    fn config(max_retries: u32) -> RetryConfig {
        RetryConfig {
            max_retries,
            initial_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(100),
            timeout: Duration::from_secs(30),
        }
    }

    fn response(status: u16, retry_after: Option<&str>) -> Response {
        let mut response = axum::http::Response::builder().status(status);
        if let Some(value) = retry_after {
            response = response.header(RETRY_AFTER, value);
        }
        response.body(String::new()).unwrap().into()
    }

    async fn read_ok(_: Response) -> Result<String, LLMError> {
        Ok("completed".into())
    }

    #[tokio::test(start_paused = true)]
    async fn invalid_library_retry_settings_fail_before_sending() {
        for config in [
            RetryConfig {
                initial_delay: Duration::ZERO,
                ..config(2)
            },
            RetryConfig {
                max_delay: Duration::from_secs(31),
                ..config(2)
            },
            RetryConfig {
                timeout: Duration::from_secs(3601),
                ..config(2)
            },
        ] {
            let attempts = Cell::new(0);
            let result = request_with_retry(
                config,
                || {
                    attempts.set(attempts.get() + 1);
                    ready(Ok(response(200, None)))
                },
                read_ok,
            )
            .await;
            assert!(matches!(result, Err(LLMError::Configuration(_))));
            assert_eq!(attempts.get(), 0);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn retry_limits_hold_even_when_the_server_supplies_retry_after() {
        for status in [429, 503, 529] {
            for max_retries in [0, 2] {
                let attempts = Cell::new(0);
                let result = request_with_retry(
                    config(max_retries),
                    || {
                        attempts.set(attempts.get() + 1);
                        ready(Ok(response(status, Some("0"))))
                    },
                    read_ok,
                )
                .await;
                assert_eq!(result, Err(LLMError::Http(status)));
                assert_eq!(attempts.get(), max_retries + 1);
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn other_http_errors_and_transport_errors_are_not_retried() {
        for status in [400, 401, 403, 404, 500, 502] {
            let attempts = Cell::new(0);
            let result = request_with_retry(
                config(2),
                || {
                    attempts.set(attempts.get() + 1);
                    ready(Ok(response(status, Some("0"))))
                },
                read_ok,
            )
            .await;
            assert_eq!(result, Err(LLMError::Http(status)));
            assert_eq!(attempts.get(), 1);
        }
        let attempts = Cell::new(0);
        let http = reqwest::Client::new();
        let result = request_with_retry(
            config(2),
            || {
                attempts.set(attempts.get() + 1);
                ready(Err(http.get("://private-invalid-url").build().unwrap_err()))
            },
            read_ok,
        )
        .await;
        assert_eq!(result, Err(LLMError::Transport));
        assert_eq!(attempts.get(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_after_is_a_minimum_wait_even_above_the_backoff_cap() {
        let start = Instant::now();
        let attempts = Cell::new(0);
        let result = request_with_retry(
            config(2),
            || {
                attempts.set(attempts.get() + 1);
                ready(Ok(if attempts.get() == 1 {
                    response(429, Some("10"))
                } else {
                    response(200, None)
                }))
            },
            read_ok,
        )
        .await;
        assert_eq!(result, Ok("completed".into()));
        assert_eq!(attempts.get(), 2);
        assert_eq!(start.elapsed(), Duration::from_secs(10));
    }

    #[tokio::test(start_paused = true)]
    async fn server_delay_that_cannot_fit_the_deadline_stops_retrying() {
        let attempts = Cell::new(0);
        let result = request_with_retry(
            config(2),
            || {
                attempts.set(attempts.get() + 1);
                ready(Ok(response(429, Some("18446744073709551615"))))
            },
            read_ok,
        )
        .await;
        assert_eq!(result, Err(LLMError::Http(429)));
        assert_eq!(attempts.get(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn response_reading_is_not_replayed() {
        let attempts = Cell::new(0);
        let result = request_with_retry(
            config(2),
            || {
                attempts.set(attempts.get() + 1);
                ready(Ok(response(200, None)))
            },
            |_| ready(Err::<String, _>(LLMError::IncompleteResponse)),
        )
        .await;
        assert_eq!(result, Err(LLMError::IncompleteResponse));
        assert_eq!(attempts.get(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn one_deadline_covers_requests_waits_and_response_reading() {
        let config = RetryConfig {
            timeout: Duration::from_secs(10),
            ..config(2)
        };
        let attempts = Cell::new(0);
        let start = Instant::now();
        let result = request_with_retry(
            config,
            || {
                attempts.set(attempts.get() + 1);
                let status = if attempts.get() == 1 { 503 } else { 200 };
                async move {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    Ok(response(status, Some("1")))
                }
            },
            |_| pending::<Result<String, LLMError>>(),
        )
        .await;
        assert_eq!(result, Err(LLMError::Timeout));
        assert_eq!(attempts.get(), 2);
        assert_eq!(start.elapsed(), config.timeout);
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_during_backoff_does_not_send_another_request() {
        let attempts = Cell::new(0);
        let call = request_with_retry(
            config(2),
            || {
                attempts.set(attempts.get() + 1);
                ready(Ok(response(503, Some("10"))))
            },
            read_ok,
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(1), call)
                .await
                .is_err()
        );
        tokio::time::advance(Duration::from_secs(30)).await;
        assert_eq!(attempts.get(), 1);
    }

    #[test]
    fn retry_after_accepts_seconds_and_http_dates_and_ignores_invalid_values() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let future = httpdate::fmt_http_date(now + Duration::from_secs(10));
        let past = httpdate::fmt_http_date(now - Duration::from_secs(10));
        for (value, expected) in [
            ("5", Some(Duration::from_secs(5))),
            (future.as_str(), Some(Duration::from_secs(10))),
            (past.as_str(), Some(Duration::ZERO)),
            ("invalid", None),
            ("-1", None),
        ] {
            assert_eq!(
                retry_after(response(429, Some(value)).headers(), now),
                expected
            );
        }
        assert_eq!(retry_after(response(429, None).headers(), now), None);
    }
}

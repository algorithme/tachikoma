# Tachikoma core

Core serves a local provider setup page and a stateless text prompt form.

```text
main
  |-- Config::from_env()         reads configuration once
  |-- web                       local setup and invocation
        |-- OpenAIChat          Responses API
        |     |-- API key       environment snapshot
        |     +-- subscription  native OAuth and OS credential store
        +-- AnthropicChat       Messages API, API key

LLMChat::invoke(&self, prompt: &str) -> Future<Result<String, LLMError>>
```

Each invocation sends one user prompt, with no conversation history or tools.
OpenAI's response stream is collected internally; both providers return a
completed `String`. Interrupted, refused, and truncated responses return errors.
Initial HTTP 429, 503, and 529 rejections can be retried with bounded exponential
backoff and jitter. Calls never switch to another billing method. Transport
failures and interrupted, refused, or truncated responses are not retried.

## Run

Configure the models you intend to use, then run from the workspace:

```sh
export TACHIKOMA_OPENAI_MODEL="<model-available-to-your-openai-account>"
export TACHIKOMA_ANTHROPIC_MODEL="<model-available-to-your-anthropic-account>"
cargo run -p tachikoma-core
```

Open the `http://127.0.0.1:3000/` address printed by core and choose a connection.
Configure only the providers you need. No model or billing method is selected
implicitly. Changing an environment setting requires restarting core.

For API-key access, supply `OPENAI_API_KEY` or `ANTHROPIC_API_KEY` in core's
environment before launch. Keys are never entered into or returned to the web
page. API usage is billed separately from a ChatGPT or Claude subscription.

For OpenAI subscription access, choose **OpenAI — ChatGPT subscription** and
**Continue with ChatGPT**. The browser completes OpenAI's native authorization
flow and returns to core. An available API key is never used as a fallback.

## Configuration

All application environment reads live in `src/common/config.rs`.

| Variable | Meaning | Default |
| --- | --- | --- |
| `TACHIKOMA_PORT` | Local HTTP port, 1–65535 | `3000` |
| `TACHIKOMA_LLM_PROVIDER` | Initial selection: `openai` or `anthropic` | Ask in setup |
| `TACHIKOMA_OPENAI_AUTH_MODE` | Initial OpenAI method: `api_key` or `subscription` | Ask in setup |
| `TACHIKOMA_OPENAI_MODEL` | OpenAI model available to the selected account | Unset |
| `OPENAI_API_KEY` | OpenAI API credential | Unset |
| `TACHIKOMA_ANTHROPIC_MODEL` | Anthropic model available to the selected account | Unset |
| `ANTHROPIC_API_KEY` | Anthropic API credential | Unset |
| `TACHIKOMA_ANTHROPIC_MAX_TOKENS` | Maximum generated tokens per Anthropic invocation | `1024` |
| `ANTHROPIC_WORKSPACE_ID` | Workspace for an Anthropic multi-workspace key | Unset |
| `TACHIKOMA_LLM_MAX_RETRIES` | Retries after the first HTTP attempt; `0` disables retries | `2` |
| `TACHIKOMA_LLM_RETRY_INITIAL_DELAY_MS` | Initial backoff before jitter, in milliseconds | `500` |
| `TACHIKOMA_LLM_RETRY_MAX_DELAY_MS` | Backoff cap including jitter, in milliseconds | `5000` |
| `TACHIKOMA_LLM_TIMEOUT_MS` | Overall HTTP deadline across attempts, waits, and response reading, in milliseconds | `120000` |

Missing credentials allow setup to load. Invocation requires the selected
provider's model and credentials. Nonempty malformed configuration is rejected
without printing secret values. API-key presence does not select API billing.

The retry settings are shared by both providers and loaded once at startup.
With the defaults there are at most three HTTP attempts. Backoff doubles on each
retry, adds jitter, and is capped by the configured maximum. A valid
`Retry-After` header (seconds or HTTP date) sets a minimum wait and may exceed
that cap. If the required wait cannot fit the remaining deadline, the last HTTP
error is returned without another attempt. Malformed headers use normal backoff.

Delay values and the timeout must be positive integers, with
`initial delay <= maximum delay <= timeout`; the timeout is limited to one hour.
The deadline begins after credential acquisition and does not change OAuth
registration or refresh behavior. Response reading stays outside the retry loop,
so a timeout or broken response stream cannot restart generation. HTTP transport
retries remain disabled to avoid multiplying attempts.

## Module boundaries

- `common/config.rs`: immutable provider configuration and redacted API keys.
- `llm/mod.rs`: `LLMChat`, safe errors, bounded HTTP reads and client policy.
- `llm/retry.rs`: BackON retry policy, `Retry-After` parsing and shared HTTP deadline.
- `clients/llm/openai_client.rs`: explicit OpenAI authentication choice and Responses SSE parsing.
- `llm/anthropic.rs`: Anthropic Messages request and completed-text parsing.
- `auth.rs`: OpenAI OAuth, token validation, refresh, persistence and disconnect.
- `web.rs`: local setup/status/login/invoke routes and browser request protection.
- `web/`: small HTML/CSS/JavaScript interface; model output is rendered as text.

Subscription tokens are kept in the OS credential store, separately from
configuration; the implementation does not fall back to plaintext files. Run
one core process per OS user for the saved subscription registration. This first
version keeps one ChatGPT account/workspace registration, including after
disconnect, so subsequent sign-ins return to that identity. A working OS
credential store is required for subscription access; API-key access remains
available without it. On Linux this requires a Secret Service implementation.

The server binds only to `127.0.0.1`, checks the exact host, and protects browser
POST actions with same-origin and CSRF checks. Credentials and provider error
bodies are excluded from UI responses and logs. OpenAI requests use
`store: false`; prompts are still sent to the chosen provider and subject to its
data policies. Core does not save prompt/response history. The browser may save
the nonsecret connection preference.

## Verification

```sh
cargo test -p tachikoma-core
cargo clippy -p tachikoma-core --all-targets -- -D warnings
cargo fmt --all -- --check
```

Tests use local HTTP servers, fake API keys, test JWTs, and in-memory credential
storage. They do not call provider services or use your OS-stored credentials.

Wire contracts: [OpenAI subscription sign-in](https://developers.openai.com/siwc/token-sharing-open-source/sign-in),
[OpenAI subscription inference](https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference),
[Anthropic authentication](https://platform.claude.com/docs/en/manage-claude/authentication).

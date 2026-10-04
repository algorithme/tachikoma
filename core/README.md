# Tachikoma core

Core provides a reusable agent library and a local provider setup page. A
`ConfigurableAgent` remembers its conversation and calls tools until it produces
a structured answer. The existing web prompt form remains a separate, stateless
text interface.

```text
main --> tachikoma_core::serve()
           |-- Config::from_env()     reads server configuration once
           +-- web                   local setup and stateless text invocation

Library caller --> ConfigurableAgent --> LLMChat::complete(history, tools)
                                            |
                         +------------------+-------------------+
                         |                                      |
                     OpenAIChat                            AnthropicChat
                  Responses API                          Messages API
                   |-- API key                            +-- API key
                   +-- ChatGPT subscription
```

## Agent models and responsibilities

Here, **model** means a Rust data structure; an **LLM** is the remote language
model. A **model turn** is one call to the LLM, which can request several tools.
An **invocation** is one user request and all the model turns needed to handle it.

| Type | Responsibility |
| --- | --- |
| `AgentConfig` | System prompt source, Answer schema source, final-tool names and maximum turns. |
| `PromptSource` / `OutputSchemaSource` | An asset reference or content supplied inline. |
| `ConfigurableAgent<L>` | Initialized configuration and the history for one conversation. |
| `AgentLoop` | Private, stateless service that applies the execution and stopping rules. |
| `AgentOutcome` | `Completed { tool_name, output }` or `Unsatisfactory { text }`. |
| `AgentError` | Invalid configuration/input, provider failure or exhausted turn limit. |
| `LLMRequest` / `LLMResponse` | Borrowed prompt/history/tool definitions sent to a client; returned text, tool calls and assistant replay data. |
| `LLMMessage` | User text, an assistant response, or a batch of tool results. |
| `AssistantReplay` | Original provider content retained for the next request, including opaque reasoning data. |
| `ToolDefinition` / `ToolCall` | A tool's name, description and input schema; one requested call with an ID and arguments. |
| `ToolResult` / `ToolError` | A result matched to its call ID, or a safe diagnostic the LLM can use to correct the call. |
| `Tool` / `AnswerTool` | A concurrent tool interface; the built-in `answer` tool validates and returns its input dictionary unchanged. |

`terminal_tools` means **final tools**, not shell commands. `answer` is registered
automatically and is the default final tool. Additional tools are supplied at
initialization. Each name must be unique; `answer` is reserved. Every configured
final tool must exist. Names contain 1-64 ASCII letters, digits, underscores or
hyphens.

```text
PromptRegistry                 OutputValidatorRegistry
 resolve(prompt)                resolve(schema)
       |                              |
       +----------- initialization ---+
                              |
                   +----------v---------------+
                   | ConfigurableAgent<L>     |
                   | owns: history, config    |
                   | invoke(&mut self, input) |
                   | history(), clear_history |
                   +------------+-------------+
                                | uses
                         +------v------+
                         | AgentLoop   |
                         | run()       |
                         +---+-----+---+
                             |     |
                   model call|     |tool execution
                             v     v
                         LLMChat   Tool *
                         complete  definition(), execute()
```

The LLM client owns credentials and HTTP connections. The agent owns conversation
state. Separate agents can share an `Arc` client and tool implementations while
keeping their histories independent. `invoke(&mut self, ...)` prevents overlapping
invocations on the same agent.

## Initialize and use an agent

Supply the resource directory explicitly. The checked-in `core/resources`
directory contains `templates/*.md` and `output_schemas/*.json`; a deployed
application must provide these assets at its chosen location. References are
top-level filenames without the extension, such as `default_agent` or `answer`.
Resource files must be regular files; symbolic links are rejected.

The example accepts an already configured client and performs no environment or
credential discovery itself. It is compiled as a documentation test and is not
executed by tests.

```rust,no_run
use std::{path::Path, sync::Arc};
use serde_json::json;
use tachikoma_core::{
    agent::{
        AgentConfig, AgentError, AgentOutcome, ConfigurableAgent,
        OutputSchemaSource, PromptSource,
        service::{OutputValidatorRegistry, PromptRegistry},
    },
    llm::LLMChat,
};

async fn explain_loop<L: LLMChat>(
    client: Arc<L>,
    resources: &Path,
) -> Result<AgentOutcome, AgentError> {
    let prompts = PromptRegistry::from_directory(
        resources.join("templates"),
        PromptSource::Reference("configurable_agent_prefix".into()),
    )?;
    let validators = OutputValidatorRegistry::from_directory(
        resources.join("output_schemas"),
    )?;
    let config = AgentConfig::new(
        PromptSource::Reference("default_agent".into()),
        OutputSchemaSource::Inline(json!({
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
            "additionalProperties": false
        })),
    );
    let mut agent = ConfigurableAgent::new(
        config, client, &prompts, &validators, vec![],
    )?;

    let _first = agent.invoke("Explain an agent loop briefly.").await?;
    let follow_up = agent.invoke("Make that explanation shorter.").await?;
    let _history = agent.history(); // Explicit access to sensitive conversation data.
    agent.clear_history();         // Next invocation starts a new conversation.
    Ok(follow_up)
}
```

Use `PromptSource::Inline(String)` for an inline prompt, or
`OutputSchemaSource::Reference("answer".into())` for the bundled Answer schema.
The prefix source is also configurable. Initialization combines the prefix,
configured final-tool names and agent prompt once. `AgentConfig::new` defaults to
`terminal_tools = ["answer"]` and `max_turns = 10`; set those fields before
constructing the agent to change them. A zero turn limit or empty final-tool list
is rejected.

Registries load immutable snapshots at startup. File changes do not alter an
existing registry or agent. Referenced prompts and compiled validators are shared;
inline schemas are compiled when resolved. There is no registry file access in
the loop and no remote registry implementation yet.

Schemas use JSON Schema Draft 2020-12 and must declare `"type": "object"` at their
root. References can point within the same schema document; external retrieval is
disabled. `format` is an annotation, not a validation check. The configured output
schema is the `answer` tool's input/output contract. Other tools have their own
input schemas; another final tool returns its own output without applying the
Answer schema.

## Loop behavior

```text
invoke(input) --> append user message
                         |
                         v
                    call the LLM <----------------------------+
                         |                                    |
                  append assistant                            |
                         |                                    |
                   tool calls?                                |
                   /         \                                |
                 no           yes                             |
                 |             |                              |
         Unsatisfactory   final tool + any other call?         |
                            /                \                 |
                          yes                 no               |
                           |                   |               |
                 reject every call     validate each call     |
                 execute nothing       run valid calls        |
                           |           concurrently           |
                           +---------+---------+               |
                                     |                         |
                           append matching results            |
                                     |                         |
                          successful sole final tool?         |
                              /             \                  |
                            yes              no -- next turn --+
                             |
                         Completed

No completion after max_turns --> TurnLimitExceeded
Provider failure             --> AgentError::Provider
```

- A final tool must be the only call in that response. A mixed batch, two
  different final tools, or repeated calls to one final tool rejects the whole
  batch before execution. Every call receives a matching error for correction.
- Ordinary valid calls run concurrently. An unknown tool or invalid arguments
  receives an error while valid siblings can still run. Results are recorded in
  the model's call order, even when tools finish in a different order.
- Malformed JSON, schema failures and tool execution errors are returned to the
  model. A failed final-tool call does not finish the invocation. Tools are not
  automatically retried; the model can request a corrected call on another turn.
- Text without a tool call ends the invocation as `Unsatisfactory`, including
  an empty completed reply. The agent does not invent an `answer` call.
- The limit counts model turns, including turns spent correcting errors. The
  last allowed turn's tool batch is processed; a valid final call on that turn
  can still succeed. HTTP retry attempts are governed separately below.

History is retained after normal completion, an unsatisfactory reply, a provider
error or reaching the turn limit. Before awaiting tools, the loop installs a
result slot for every call. If the invocation future is dropped, completed results
remain and unresolved slots say the execution was interrupted with an unknown
outcome. The next invocation receives this history. Cancellation cannot promise
that an external side effect was undone. Tool implementations must support
concurrent calls and must not detach work from their returned futures.

## Provider clients and the web interface

All three existing connection modes support structured agent calls:

| Connection | Public constructor |
| --- | --- |
| OpenAI API key | `OpenAIChat::with_api_key(OpenAIConfig, RetryConfig)` |
| OpenAI ChatGPT subscription | `OpenAIChat::with_subscription(OpenAIConfig, RetryConfig).await` |
| Anthropic API key | `AnthropicChat::new(AnthropicConfig, RetryConfig)` |

Clients are in `tachikoma_core::clients::llm::{openai_client, anthropic_client}`;
configuration types are exported from the crate root. The subscription constructor
restores saved authorization from the OS credential store. Use the setup page for
initial sign-in. It does not fall back to API billing.

`LLMChat::complete` takes a system prompt, caller-owned history and tool definitions.
Adapters retain original assistant content for replay: OpenAI response items,
including encrypted reasoning, or Anthropic content blocks, including thinking
signatures. History belongs to its selected provider and is not converted when
switching providers.

The existing `LLMChat::invoke(&self, prompt: &str)` remains a text-only convenience
method. The web form uses it: each request sends one user prompt with no history or
tools. OpenAI's stream is collected internally; both providers return a completed
`String`. Interrupted, refused, truncated and empty text-only responses return
errors. Initial HTTP 429, 503 and 529 rejections can be retried with bounded
exponential backoff and jitter. Transport failures and interrupted, refused or
truncated responses are not retried. Calls never switch billing methods.

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
| `TACHIKOMA_ANTHROPIC_MAX_TOKENS` | Maximum generated tokens per Anthropic model call | `1024` |
| `ANTHROPIC_WORKSPACE_ID` | Workspace for an Anthropic multi-workspace key | Unset |
| `TACHIKOMA_LLM_MAX_RETRIES` | Retries after the first HTTP attempt; `0` disables retries | `2` |
| `TACHIKOMA_LLM_RETRY_INITIAL_DELAY_MS` | Initial backoff before jitter, in milliseconds | `500` |
| `TACHIKOMA_LLM_RETRY_MAX_DELAY_MS` | Backoff cap including jitter, in milliseconds | `5000` |
| `TACHIKOMA_LLM_TIMEOUT_MS` | Overall HTTP deadline across attempts, waits, and response reading, in milliseconds | `120000` |

Missing credentials allow setup to load. Invocation requires the selected
provider's model and credentials. Nonempty malformed configuration is rejected
without printing secret values. API-key presence does not select API billing.

The server's retry settings are shared by both providers and loaded once at startup.
With the defaults there are at most three HTTP attempts per model call. Backoff doubles on each
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

- `main.rs`: thin executable that calls the library's `serve()` function.
- `lib.rs`: public library modules, configuration exports and server startup.
- `agent/models.rs`: configuration, prompt/schema sources, outcomes and errors.
- `agent/configurable_agent.rs`: initialization, tool registration and owned history.
- `agent/service/agent_loop.rs`: model/tool rounds, batch rules and completion.
- `agent/service/{prompt_registry,output_validator_registry}.rs`: prompt snapshots and compiled schema lookup.
- `agent/tool/`: the `Tool` interface and built-in `AnswerTool`.
- `resources/`: runtime prompt templates and output schemas loaded explicitly.
- `common/config.rs`: immutable provider configuration and redacted API keys.
- `llm/mod.rs` and `llm/models.rs`: `LLMChat`, conversation/tool models, safe errors, bounded HTTP reads and client policy.
- `llm/retry.rs`: BackON retry policy, `Retry-After` parsing and shared HTTP deadline.
- `clients/llm/openai_client.rs`: explicit OpenAI authentication choice and Responses SSE parsing.
- `clients/llm/anthropic_client.rs`: Anthropic message/tool encoding and response parsing.
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
`store: false`; prompts and agent history are still sent to the chosen provider and
subject to its data policies. Agent history is held in memory, with no disk
persistence, automatic size limit or compression. Clear history explicitly when
needed; provider context-limit errors are returned to the caller.
`history()` explicitly exposes sensitive
content to its owner; conversation/configuration `Debug` output is redacted.
`clear_history()` drops the agent's history but does not undo tool effects. The
stateless web form retains no server-side conversation. The browser may save the
nonsecret connection preference.

## Verification

```sh
cargo test -p tachikoma-core
cargo clippy -p tachikoma-core --all-targets -- -D warnings
cargo fmt --all -- --check
```

Tests use scripted LLMs, local HTTP servers, fake API keys, test JWTs, and in-memory
credential storage. They cover tool correction, final-tool exclusivity, turn
limits, concurrent completion order, interruption and retained history, provider
replay, resource loading and schema validation. Documentation examples are compiled
without execution. Tests do not call provider services or use your OS-stored
credentials.

Wire contracts: [OpenAI subscription sign-in](https://developers.openai.com/siwc/token-sharing-open-source/sign-in),
[OpenAI subscription inference](https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference),
[Anthropic authentication](https://platform.claude.com/docs/en/manage-claude/authentication).

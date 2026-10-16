use std::{
    collections::VecDeque,
    future::{Future, poll_fn},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::Poll,
    time::Duration,
};

use serde_json::{Value, json};
use tachikoma_core::{
    agent::{
        AgentConfig, AgentError, AgentOutcome, ConfigurableAgent, OutputSchemaSource, PromptSource,
        service::{OutputValidatorRegistry, PromptRegistry},
        tool::{Tool, ToolFuture},
    },
    llm::{
        AssistantReplay, LLMChat, LLMError, LLMMessage, LLMRequest, LLMResponse, ToolArguments,
        ToolCall, ToolDefinition, ToolError, ToolResult,
    },
};
use tokio::sync::{Barrier, Notify};

struct CapturedRequest {
    prompt: String,
    history: Vec<LLMMessage>,
    tools: Vec<ToolDefinition>,
}

struct ScriptedModel {
    replies: Mutex<VecDeque<Result<LLMResponse, LLMError>>>,
    requests: Mutex<Vec<CapturedRequest>>,
}

impl ScriptedModel {
    fn new(replies: impl IntoIterator<Item = Result<LLMResponse, LLMError>>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        })
    }
}

impl LLMChat for ScriptedModel {
    async fn complete(&self, request: LLMRequest<'_>) -> Result<LLMResponse, LLMError> {
        self.requests.lock().unwrap().push(CapturedRequest {
            prompt: request.system_prompt.to_owned(),
            history: request.messages.to_vec(),
            tools: request.tools.to_vec(),
        });
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected extra model turn")
    }
}

struct TestTool {
    definition: ToolDefinition,
    handler: Box<dyn Fn(Value) -> ToolFuture<'static> + Send + Sync>,
}

impl Tool for TestTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    fn execute(&self, arguments: Value) -> ToolFuture<'_> {
        (self.handler)(arguments)
    }
}

fn tool<F, Fut>(name: &str, handler: F) -> Arc<dyn Tool>
where
    F: Fn(Value) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Value, ToolError>> + Send + 'static,
{
    Arc::new(TestTool {
        definition: ToolDefinition {
            name: name.to_owned(),
            description: format!("Test {name}"),
            input_schema: json!({"type": "object"}),
        },
        handler: Box::new(move |arguments| Box::pin(handler(arguments))),
    })
}

fn schema() -> Value {
    json!({"type": "object", "properties": {"text": {"type": "string"}},
        "required": ["text"], "additionalProperties": false})
}

fn config() -> AgentConfig {
    AgentConfig::new(
        PromptSource::Inline("Help with the requested task.".into()),
        OutputSchemaSource::Inline(schema()),
    )
}

fn registries() -> (PromptRegistry, OutputValidatorRegistry) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources");
    (
        PromptRegistry::from_directory(
            root.join("templates"),
            PromptSource::Reference("configurable_agent_prefix".into()),
        )
        .unwrap(),
        OutputValidatorRegistry::from_directory(root.join("output_schemas")).unwrap(),
    )
}

fn agent(
    model: Arc<ScriptedModel>,
    config: AgentConfig,
    tools: Vec<Arc<dyn Tool>>,
) -> ConfigurableAgent<ScriptedModel> {
    let (prompts, validators) = registries();
    ConfigurableAgent::new(config, model, &prompts, &validators, tools).unwrap()
}

fn call(id: &str, name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: ToolArguments::Json(arguments),
    }
}

fn response(calls: Vec<ToolCall>) -> LLMResponse {
    let replay = calls
        .iter()
        .map(|call| {
            json!({
                "type": "function_call", "call_id": call.id, "name": call.name,
                "arguments": match &call.arguments {
                    ToolArguments::Json(value) => value.to_string(),
                    ToolArguments::InvalidJson => "{malformed".to_owned(),
                },
            })
        })
        .collect();
    LLMResponse {
        text: None,
        tool_calls: calls,
        replay: AssistantReplay::OpenAI(replay),
    }
}

fn answer(id: &str, text: &str) -> Result<LLMResponse, LLMError> {
    Ok(response(vec![call(id, "answer", json!({"text": text}))]))
}

fn results(message: &LLMMessage) -> &[ToolResult] {
    match message {
        LLMMessage::ToolResults(results) => results,
        _ => panic!("expected tool results"),
    }
}

#[tokio::test]
async fn bundled_profile_resolves_real_resources_and_completes_through_answer() {
    let config = AgentConfig::from_profile_file(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources/agents/default.json"),
    )
    .unwrap();
    let model = ScriptedModel::new([answer("done", "4")]);
    let mut agent = agent(model.clone(), config, vec![]);
    assert_eq!(
        agent.invoke("What is 2 + 2?").await.unwrap(),
        AgentOutcome::Completed {
            tool_name: "answer".into(),
            output: json!({"text": "4"}),
        }
    );
    let requests = model.requests.lock().unwrap();
    let bundled_schema: Value =
        serde_json::from_str(include_str!("../resources/output_schemas/answer.json")).unwrap();
    assert_eq!(requests[0].tools[0].input_schema, bundled_schema);
    assert!(
        requests[0]
            .prompt
            .contains(include_str!("../resources/templates/default_agent.md").trim())
    );
}

#[tokio::test]
async fn profile_answer_schema_validates_calls_without_registering_declared_tools() {
    let custom_schema = json!({"type": "object", "properties": {"count": {"type": "integer"}},
        "required": ["count"], "additionalProperties": false});
    let options = json!({"future": {"enabled": true, "values": [1, "two"]}});
    let profile = json!({
        "system_prompt": {"inline": "Count accurately."},
        "tools": {"answer": {"output_schema": {"inline": custom_schema}}, "future_tool": options},
        "terminal_tools": ["answer"], "max_turns": 2
    });
    let path =
        std::env::temp_dir().join(format!("tachikoma-profile-{}.json", uuid::Uuid::new_v4()));
    std::fs::write(&path, profile.to_string()).unwrap();
    let loaded = AgentConfig::from_profile_file(&path);
    std::fs::remove_file(path).unwrap();
    let config = loaded.unwrap();
    assert_eq!(config.tools["future_tool"], *options.as_object().unwrap());
    let model = ScriptedModel::new([
        answer("invalid", "4"),
        Ok(response(vec![call("valid", "answer", json!({"count": 4}))])),
    ]);
    let mut agent = agent(model.clone(), config, vec![]);
    assert_eq!(
        agent.invoke("Count to four").await.unwrap(),
        AgentOutcome::Completed {
            tool_name: "answer".into(),
            output: json!({"count": 4}),
        }
    );
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].tools.len(), 1);
    assert_eq!(requests[0].tools[0].name, "answer");
    assert_eq!(requests[0].tools[0].input_schema, custom_schema);
    assert_eq!(
        results(requests[1].history.last().unwrap())[0].outcome,
        Err(ToolError::InvalidArguments)
    );
}

#[tokio::test]
async fn answer_completes_immediately_and_history_survives_until_explicitly_cleared() {
    let model = ScriptedModel::new([
        answer("one", "First"),
        answer("two", "Second"),
        answer("three", "Fresh"),
    ]);
    let mut agent = agent(model.clone(), config(), vec![]);
    assert_eq!(
        agent.invoke("First request").await.unwrap(),
        AgentOutcome::Completed {
            tool_name: "answer".into(),
            output: json!({"text": "First"}),
        }
    );
    let first_history = agent.history().to_vec();
    assert_eq!(first_history.len(), 3);
    assert_eq!(results(&first_history[2])[0].call_id, "one");
    agent.invoke("Second request").await.unwrap();
    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            &requests[1].history[..first_history.len()],
            first_history.as_slice()
        );
        assert_eq!(
            requests[1].history.last(),
            Some(&LLMMessage::User("Second request".into()))
        );
        assert_eq!(requests[0].prompt, requests[1].prompt);
        assert_eq!(requests[0].tools, requests[1].tools);
        let answer = requests[0]
            .tools
            .iter()
            .find(|tool| tool.name == "answer")
            .unwrap();
        assert_eq!(answer.input_schema, schema());
        assert!(requests[0].prompt.contains("Help with the requested task."));
        assert!(requests[0].prompt.contains("answer"));
    }
    agent.clear_history();
    assert!(agent.history().is_empty());
    agent.invoke("Fresh request").await.unwrap();
    let requests = model.requests.lock().unwrap();
    assert_eq!(
        requests[2].history,
        vec![LLMMessage::User("Fresh request".into())]
    );
}

#[tokio::test]
async fn agents_share_a_model_without_sharing_conversation_or_configuration() {
    let model = ScriptedModel::new([answer("a", "A"), answer("b", "B")]);
    let mut a_config = config();
    a_config.system_prompt = PromptSource::Inline("Agent A instructions".into());
    let mut b_config = config();
    b_config.system_prompt = PromptSource::Inline("Agent B instructions".into());
    let mut a = agent(model.clone(), a_config, vec![]);
    let mut b = agent(model.clone(), b_config, vec![]);
    a.invoke("Private A input").await.unwrap();
    b.invoke("Private B input").await.unwrap();
    let requests = model.requests.lock().unwrap();
    assert_eq!(
        requests[0].history,
        vec![LLMMessage::User("Private A input".into())]
    );
    assert_eq!(
        requests[1].history,
        vec![LLMMessage::User("Private B input".into())]
    );
    assert!(requests[0].prompt.contains("Agent A instructions"));
    assert!(!requests[0].prompt.contains("Agent B instructions"));
    assert!(requests[1].prompt.contains("Agent B instructions"));
    assert!(!requests[1].prompt.contains("Agent A instructions"));
    assert_ne!(a.history(), b.history());
}

#[tokio::test]
async fn no_tool_call_is_unsatisfactory_even_when_text_is_present() {
    for text in [Some("An unstructured reply".to_owned()), None] {
        let model = ScriptedModel::new([Ok(LLMResponse {
            text: text.clone(),
            tool_calls: vec![],
            replay: AssistantReplay::OpenAI(vec![]),
        })]);
        let mut agent = agent(model.clone(), config(), vec![]);
        assert_eq!(
            agent.invoke("Please answer").await.unwrap(),
            AgentOutcome::Unsatisfactory { text }
        );
        assert_eq!(model.requests.lock().unwrap().len(), 1);
        assert_eq!(agent.history().len(), 2);
    }
}

#[tokio::test]
async fn bad_calls_and_execution_errors_are_returned_to_the_model_for_correction() {
    let executions = Arc::new(AtomicUsize::new(0));
    let count = executions.clone();
    let work = tool("work", move |_| {
        let count = count.clone();
        async move {
            count.fetch_add(1, Ordering::SeqCst);
            Err(ToolError::ExecutionFailed)
        }
    });
    let model = ScriptedModel::new([
        Ok(response(vec![
            call("unknown", "missing", json!({})),
            ToolCall {
                id: "malformed".into(),
                name: "work".into(),
                arguments: ToolArguments::InvalidJson,
            },
            call("wrong-shape", "work", json!([1])),
            call("execution-error", "work", json!({})),
        ])),
        Ok(response(vec![call(
            "invalid-answer",
            "answer",
            json!({"text": 7}),
        )])),
        answer("corrected", "Now valid"),
    ]);
    let mut agent = agent(model.clone(), config(), vec![work]);
    assert!(matches!(
        agent.invoke("Do the work").await,
        Ok(AgentOutcome::Completed { .. })
    ));
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    let requests = model.requests.lock().unwrap();
    let first = results(requests[1].history.last().unwrap());
    assert_eq!(
        first
            .iter()
            .map(|result| (result.call_id.as_str(), result.outcome.clone()))
            .collect::<Vec<_>>(),
        vec![
            ("unknown", Err(ToolError::UnknownTool)),
            ("malformed", Err(ToolError::InvalidArguments)),
            ("wrong-shape", Err(ToolError::InvalidArguments)),
            ("execution-error", Err(ToolError::ExecutionFailed)),
        ]
    );
    let second = results(requests[2].history.last().unwrap());
    assert_eq!(second[0].call_id, "invalid-answer");
    assert_eq!(second[0].outcome, Err(ToolError::InvalidArguments));
}

#[tokio::test]
async fn turn_limit_counts_model_rounds_and_finishes_the_last_tool_batch() {
    for configured_limit in [None, Some(2)] {
        let limit = configured_limit.unwrap_or(10);
        let model = ScriptedModel::new((0..limit).map(|index| {
            Ok(response(vec![call(
                &format!("call-{index}"),
                "work",
                json!({}),
            )]))
        }));
        let executions = Arc::new(AtomicUsize::new(0));
        let count = executions.clone();
        let work = tool("work", move |_| {
            let count = count.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(json!({"done": true}))
            }
        });
        let mut config = config();
        if let Some(limit) = configured_limit {
            config.max_turns = limit;
        }
        let mut agent = agent(model.clone(), config, vec![work]);
        assert_eq!(
            agent.invoke("Keep working").await,
            Err(AgentError::TurnLimitExceeded)
        );
        assert_eq!(model.requests.lock().unwrap().len(), limit);
        assert_eq!(executions.load(Ordering::SeqCst), limit);
        assert!(results(agent.history().last().unwrap())[0].outcome.is_ok());
    }
}

#[tokio::test]
async fn every_terminal_conflict_rejects_the_whole_batch_before_execution() {
    let cases = [
        vec![
            call("a", "answer", json!({"text": "premature"})),
            call("b", "work", json!({})),
        ],
        vec![
            call("a", "answer", json!({"text": "premature"})),
            call("b", "finish", json!({})),
        ],
        vec![
            call("a", "answer", json!({"text": "first"})),
            call("b", "answer", json!({"text": "second"})),
        ],
    ];
    for calls in cases {
        let executions = Arc::new(AtomicUsize::new(0));
        let make_tool = |name| {
            let count = executions.clone();
            tool(name, move |_| {
                let count = count.clone();
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok(json!({}))
                }
            })
        };
        let model = ScriptedModel::new([Ok(response(calls)), answer("corrected", "Final")]);
        let mut config = config();
        config.terminal_tools.push("finish".into());
        let mut agent = agent(
            model.clone(),
            config,
            vec![make_tool("work"), make_tool("finish")],
        );
        agent.invoke("Complete the task").await.unwrap();
        assert_eq!(executions.load(Ordering::SeqCst), 0);
        let requests = model.requests.lock().unwrap();
        let rejected = results(requests[1].history.last().unwrap());
        assert_eq!(
            rejected
                .iter()
                .map(|result| result.call_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert!(
            rejected
                .iter()
                .all(|result| result.outcome == Err(ToolError::RejectedBatch))
        );
    }
}

#[tokio::test]
async fn another_final_tool_returns_its_own_output_without_the_answer_schema() {
    let model = ScriptedModel::new([Ok(response(vec![call("finished", "finish", json!({}))]))]);
    let mut config = config();
    config.terminal_tools = vec!["finish".into()];
    let mut agent = agent(
        model,
        config,
        vec![tool("finish", |_| async { Ok(json!(["arbitrary", 42])) })],
    );
    assert_eq!(
        agent.invoke("Finish").await.unwrap(),
        AgentOutcome::Completed {
            tool_name: "finish".into(),
            output: json!(["arbitrary", 42]),
        }
    );
}

#[tokio::test]
async fn ordinary_calls_overlap_and_results_keep_emission_order_despite_reverse_completion() {
    let barrier = Arc::new(Barrier::new(3));
    let slow_gate = Arc::new(Notify::new());
    let fast_gate = Arc::new(Notify::new());
    let fast_done = Arc::new(Notify::new());
    let completion_order = Arc::new(Mutex::new(Vec::new()));
    let make_tool = |name: &'static str, gate: Arc<Notify>, done: Option<Arc<Notify>>| {
        let barrier = barrier.clone();
        let completed = completion_order.clone();
        tool(name, move |_| {
            let (barrier, gate, done, completed) = (
                barrier.clone(),
                gate.clone(),
                done.clone(),
                completed.clone(),
            );
            async move {
                barrier.wait().await;
                gate.notified().await;
                completed.lock().unwrap().push(name);
                if let Some(done) = done {
                    done.notify_one();
                }
                Ok(json!({"tool": name}))
            }
        })
    };
    let tools = vec![
        make_tool("slow", slow_gate.clone(), None),
        make_tool("fast", fast_gate.clone(), Some(fast_done.clone())),
    ];
    let model = ScriptedModel::new([
        Ok(response(vec![
            call("slow-id", "slow", json!({})),
            call("fast-id", "fast", json!({})),
            call("invalid-id", "unknown", json!({})),
        ])),
        answer("done", "Complete"),
    ]);
    let mut agent = agent(model.clone(), config(), tools);
    let drive = async {
        barrier.wait().await;
        fast_gate.notify_one();
        fast_done.notified().await;
        slow_gate.notify_one();
    };
    let (outcome, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(agent.invoke("Run both"), drive)
    })
    .await
    .expect("tools must start concurrently");
    outcome.unwrap();
    assert_eq!(*completion_order.lock().unwrap(), vec!["fast", "slow"]);
    let requests = model.requests.lock().unwrap();
    let batch = results(requests[1].history.last().unwrap());
    assert_eq!(
        batch
            .iter()
            .map(|result| result.call_id.as_str())
            .collect::<Vec<_>>(),
        vec!["slow-id", "fast-id", "invalid-id"]
    );
    assert_eq!(batch[0].outcome, Ok(json!({"tool": "slow"})));
    assert_eq!(batch[1].outcome, Ok(json!({"tool": "fast"})));
    assert_eq!(batch[2].outcome, Err(ToolError::UnknownTool));
}

struct DropCounter(Arc<AtomicUsize>);
impl Drop for DropCounter {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn cancellation_preserves_known_results_marks_unknown_ones_and_drops_in_flight_work() {
    let started = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicUsize::new(0));
    let finished = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(Notify::new());
    let pending = {
        let (started, dropped, finished, gate) = (
            started.clone(),
            dropped.clone(),
            finished.clone(),
            gate.clone(),
        );
        tool("pending", move |_| {
            let (started, dropped, finished, gate) = (
                started.clone(),
                dropped.clone(),
                finished.clone(),
                gate.clone(),
            );
            async move {
                let _on_drop = DropCounter(dropped);
                started.fetch_add(1, Ordering::SeqCst);
                gate.notified().await;
                finished.fetch_add(1, Ordering::SeqCst);
                Ok(json!({"finished": true}))
            }
        })
    };
    let model = ScriptedModel::new([
        Ok(response(vec![
            call("known", "ready", json!({})),
            call("unknown", "pending", json!({})),
        ])),
        answer("resumed", "Recovered"),
    ]);
    let mut agent = agent(
        model.clone(),
        config(),
        vec![
            tool("ready", |_| async { Ok(json!({"done": true})) }),
            pending,
        ],
    );
    let mut invocation = Box::pin(agent.invoke("Start work"));
    poll_fn(|context| {
        assert!(invocation.as_mut().poll(context).is_pending());
        Poll::Ready(())
    })
    .await;
    drop(invocation);
    assert_eq!(started.load(Ordering::SeqCst), 1);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    let interrupted_history = agent.history().to_vec();
    let batch = results(interrupted_history.last().unwrap());
    assert_eq!(batch[0].call_id, "known");
    assert_eq!(batch[0].outcome, Ok(json!({"done": true})));
    assert_eq!(batch[1].call_id, "unknown");
    assert_eq!(batch[1].outcome, Err(ToolError::Interrupted));
    gate.notify_one();
    tokio::task::yield_now().await;
    assert_eq!(finished.load(Ordering::SeqCst), 0);
    agent.invoke("Continue carefully").await.unwrap();
    let requests = model.requests.lock().unwrap();
    assert_eq!(
        &requests[1].history[..interrupted_history.len()],
        interrupted_history.as_slice()
    );
    assert_eq!(
        requests[1].history.last(),
        Some(&LLMMessage::User("Continue carefully".into()))
    );
}

#[tokio::test]
async fn provider_failure_retains_completed_work_for_the_next_invocation() {
    let executions = Arc::new(AtomicUsize::new(0));
    let count = executions.clone();
    let model = ScriptedModel::new([
        Ok(response(vec![call("effect", "work", json!({}))])),
        Err(LLMError::Transport),
        answer("resumed", "Done"),
    ]);
    let mut agent = agent(
        model.clone(),
        config(),
        vec![tool("work", move |_| {
            let count = count.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(json!({"written": true}))
            }
        })],
    );
    assert_eq!(
        agent.invoke("Do work").await,
        Err(AgentError::Provider(LLMError::Transport))
    );
    let retained = agent.history().to_vec();
    assert_eq!(
        results(retained.last().unwrap())[0].outcome,
        Ok(json!({"written": true}))
    );
    agent.invoke("Continue").await.unwrap();
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    let requests = model.requests.lock().unwrap();
    assert_eq!(&requests[2].history[..retained.len()], retained.as_slice());
}

#[tokio::test]
async fn invalid_configuration_and_blank_input_fail_before_model_calls() {
    let model = ScriptedModel::new([]);
    let (prompts, validators) = registries();
    let mut zero_turns = config();
    zero_turns.max_turns = 0;
    let mut no_finals = config();
    no_finals.terminal_tools.clear();
    let mut unknown_final = config();
    unknown_final.terminal_tools = vec!["missing".into()];
    for config in [zero_turns, no_finals, unknown_final] {
        assert!(matches!(
            ConfigurableAgent::new(config, model.clone(), &prompts, &validators, vec![]),
            Err(AgentError::Configuration(_))
        ));
    }
    for tools in [
        vec![tool("answer", |_| async { Ok(json!({})) })],
        vec![
            tool("duplicate", |_| async { Ok(json!({})) }),
            tool("duplicate", |_| async { Ok(json!({})) }),
        ],
        vec![tool("invalid tool name", |_| async { Ok(json!({})) })],
    ] {
        assert!(matches!(
            ConfigurableAgent::new(config(), model.clone(), &prompts, &validators, tools),
            Err(AgentError::Configuration(_))
        ));
    }
    let mut agent = agent(model.clone(), config(), vec![]);
    assert_eq!(agent.invoke(" \n ").await, Err(AgentError::InvalidInput));
    assert!(agent.history().is_empty());
    assert!(model.requests.lock().unwrap().is_empty());
}

#[test]
fn debug_output_does_not_reveal_conversation_or_tool_payloads() {
    let secret = "private-user-data-123";
    let config = AgentConfig::new(
        PromptSource::Inline(secret.into()),
        OutputSchemaSource::Inline(json!({"type": "object", "description": secret})),
    );
    let response = response(vec![call(
        "private-call",
        "answer",
        json!({"text": secret}),
    )]);
    let outcome = AgentOutcome::Completed {
        tool_name: "answer".into(),
        output: json!({"text": secret}),
    };
    let message = LLMMessage::User(secret.into());
    for debug in [
        format!("{config:?}"),
        format!("{response:?}"),
        format!("{outcome:?}"),
        format!("{message:?}"),
        format!("{:?}", response.tool_calls[0].arguments),
    ] {
        assert!(!debug.contains(secret));
    }
}

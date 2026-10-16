use std::{
    collections::VecDeque,
    io::{self, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use serde_json::{Value, json};
use tachikoma_core::{
    agent::{
        AgentConfig, AgentOutcome, ConfigurableAgent, OutputSchemaSource, PromptSource,
        service::{OutputValidatorRegistry, PromptRegistry},
        tool::{Tool, ToolFuture},
    },
    llm::{
        AssistantReplay, LLMChat, LLMError, LLMMessage, LLMRequest, LLMResponse, ToolArguments,
        ToolCall, ToolDefinition, ToolError,
    },
};
use tracing::instrument::WithSubscriber;
use tracing_subscriber::fmt::format::FmtSpan;

const INPUT: &str = "PRIVATE_INPUT_SENTINEL";
const FOLLOW_UP: &str = "PRIVATE_FOLLOW_UP_SENTINEL";
const PROMPT: &str = "PRIVATE_SYSTEM_PROMPT_SENTINEL";
const PREFIX: &str = "PRIVATE_PREFIX_SENTINEL";
const ARGUMENT: &str = "PRIVATE_TOOL_ARGUMENT_SENTINEL";
const TOOL_OUTPUT: &str = "PRIVATE_TOOL_OUTPUT_SENTINEL";
const ANSWER: &str = "PRIVATE_FINAL_ANSWER_SENTINEL";
const REPLAY: &str = "PRIVATE_REASONING_REPLAY_SENTINEL";
const ASSISTANT_TEXT: &str = "PRIVATE_ASSISTANT_TEXT_SENTINEL";
const UNKNOWN_TOOL: &str = "PRIVATE_UNKNOWN_TOOL_NAME_SENTINEL";
const SCHEMA: &str = "PRIVATE_SCHEMA_DESCRIPTION_SENTINEL";
const DESCRIPTION: &str = "PRIVATE_TOOL_DESCRIPTION_SENTINEL";

#[derive(Clone, Default)]
struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

impl Write for CapturedLogs {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct ScriptedModel {
    responses: Mutex<VecDeque<LLMResponse>>,
    histories: Mutex<Vec<Vec<LLMMessage>>>,
}

impl LLMChat for ScriptedModel {
    async fn complete(&self, request: LLMRequest<'_>) -> Result<LLMResponse, LLMError> {
        assert!(request.system_prompt.contains(PROMPT));
        assert!(request.system_prompt.contains(PREFIX));
        self.histories
            .lock()
            .unwrap()
            .push(request.messages.to_vec());
        tokio::task::yield_now().await;
        Ok(self.responses.lock().unwrap().pop_front().unwrap())
    }
}

struct LookupTool(ToolDefinition);

impl Tool for LookupTool {
    fn definition(&self) -> &ToolDefinition {
        &self.0
    }

    fn execute(&self, arguments: Value) -> ToolFuture<'_> {
        Box::pin(async move {
            assert_eq!(arguments, json!({"query": ARGUMENT}));
            tokio::task::yield_now().await;
            Ok(json!({"found": TOOL_OUTPUT}))
        })
    }
}

fn response(id: &str, name: &str, arguments: Value) -> LLMResponse {
    LLMResponse {
        text: Some(ASSISTANT_TEXT.into()),
        replay: AssistantReplay::OpenAI(vec![
            json!({"type": "reasoning", "encrypted_content": REPLAY}),
            json!({"type": "function_call", "call_id": id, "name": name,
                "arguments": arguments.to_string()}),
        ]),
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: ToolArguments::Json(arguments),
        }],
    }
}

async fn exercise_agent_with_logs(level: tracing::Level) -> String {
    let capture = CapturedLogs::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(level)
        .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
        .with_writer(move || writer.clone())
        .finish();

    let resources = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources");
    let prompts = PromptRegistry::from_directory(
        resources.join("templates"),
        PromptSource::Inline(PREFIX.into()),
    )
    .unwrap();
    let validators =
        OutputValidatorRegistry::from_directory(resources.join("output_schemas")).unwrap();
    let model = Arc::new(ScriptedModel {
        responses: Mutex::new(VecDeque::from([
            response("PRIVATE_CALL_ID_1", "lookup", json!({"query": ARGUMENT})),
            response("PRIVATE_CALL_ID_2", "answer", json!({"answer": ANSWER})),
            response(
                "PRIVATE_CALL_ID_3",
                UNKNOWN_TOOL,
                json!({"query": ARGUMENT}),
            ),
            response("PRIVATE_CALL_ID_4", "answer", json!({"answer": ANSWER})),
        ])),
        histories: Mutex::new(Vec::new()),
    });
    let config = AgentConfig::new(
        PromptSource::Inline(PROMPT.into()),
        OutputSchemaSource::Inline(json!({
            "type": "object", "description": SCHEMA,
            "properties": {"answer": {"type": "string"}}, "required": ["answer"]
        })),
    );
    let lookup: Arc<dyn Tool> = Arc::new(LookupTool(ToolDefinition {
        name: "lookup".into(),
        description: DESCRIPTION.into(),
        input_schema: json!({"type": "object"}),
    }));
    let mut agent =
        ConfigurableAgent::new(config, model.clone(), &prompts, &validators, vec![lookup]).unwrap();

    async {
        for input in [INPUT, FOLLOW_UP] {
            assert_eq!(
                agent.invoke(input).await.unwrap(),
                AgentOutcome::Completed {
                    tool_name: "answer".into(),
                    output: json!({"answer": ANSWER}),
                }
            );
        }
    }
    .with_subscriber(subscriber)
    .await;

    let histories = model.histories.lock().unwrap();
    assert_eq!(histories.len(), 4);
    assert!(model.responses.lock().unwrap().is_empty());
    assert!(histories[2].contains(&LLMMessage::User(INPUT.into())));
    assert!(matches!(
        histories[1].last(),
        Some(LLMMessage::ToolResults(results))
            if results[0].outcome == Ok(json!({"found": TOOL_OUTPUT}))
    ));
    assert!(matches!(
        histories[3].last(),
        Some(LLMMessage::ToolResults(results))
            if results[0].outcome == Err(ToolError::UnknownTool)
    ));

    String::from_utf8(capture.0.lock().unwrap().clone()).unwrap()
}

#[tokio::test]
async fn info_logs_remain_useful_without_disclosing_conversation_or_tool_data() {
    let logs = exercise_agent_with_logs(tracing::Level::INFO).await;
    for signal in [
        "agent_invocation",
        "Agent invocation started",
        "Requesting model response",
        "Model response received",
        "Executing tool",
        "Tool execution succeeded",
        "Agent invocation completed",
    ] {
        assert!(logs.contains(signal), "Missing lifecycle signal: {signal}");
    }
    for private in [
        INPUT,
        FOLLOW_UP,
        PROMPT,
        PREFIX,
        ARGUMENT,
        TOOL_OUTPUT,
        ANSWER,
        REPLAY,
        ASSISTANT_TEXT,
        UNKNOWN_TOOL,
        SCHEMA,
        DESCRIPTION,
        "PRIVATE_CALL_ID",
    ] {
        assert!(
            !logs.contains(private),
            "A private fixture value appeared in logs"
        );
    }
}

#[tokio::test]
async fn debug_logs_show_tool_inputs_and_outputs_without_internal_provider_data() {
    let logs = exercise_agent_with_logs(tracing::Level::DEBUG).await;
    assert!(logs.contains("tachikoma_core::agent::content"));
    assert_eq!(logs.matches("Agent prompt").count(), 2);
    assert_eq!(logs.matches("Model answer").count(), 4);
    assert_eq!(logs.matches("Model tool call").count(), 4);
    assert_eq!(logs.matches("Available tool").count(), 4);
    // The unknown call is rejected, so only three calls actually execute.
    assert_eq!(logs.matches("Tool input").count(), 3);
    assert_eq!(logs.matches("Tool output").count(), 3);
    for line in logs
        .lines()
        .filter(|line| line.contains("Tool input") || line.contains("Tool output"))
    {
        for field in ["invocation_id=", "turn=", "call_index=", "tool="] {
            assert!(line.contains(field), "Missing tool correlation: {field}");
        }
    }

    for content in [
        INPUT,
        FOLLOW_UP,
        PROMPT,
        PREFIX,
        ASSISTANT_TEXT,
        ARGUMENT,
        TOOL_OUTPUT,
        ANSWER,
        UNKNOWN_TOOL,
        SCHEMA,
        DESCRIPTION,
    ] {
        assert!(
            logs.contains(content),
            "Explicit DEBUG logging omitted expected conversation content"
        );
    }
    for field in [
        "system_prompt=",
        "user_input=",
        "text=",
        "tool=",
        "arguments=",
        "input=",
        "output=",
        "description=",
        "input_schema=",
        "final_tool=",
    ] {
        assert!(logs.contains(field), "Missing DEBUG content field: {field}");
    }
    for private in [REPLAY, "PRIVATE_CALL_ID"] {
        assert!(
            !logs.contains(private),
            "DEBUG logs exposed internal data outside the requested content"
        );
    }
}

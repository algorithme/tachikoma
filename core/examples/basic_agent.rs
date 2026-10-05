use std::{error::Error, io, path::Path, process::ExitCode, sync::Arc};

use tracing_subscriber::prelude::*;

use tachikoma_core::{
    OpenAIAuthMode, OpenAIConfig, RetryConfig,
    agent::{
        AgentConfig, AgentOutcome, ConfigurableAgent, OutputSchemaSource, PromptSource,
        service::{OutputValidatorRegistry, PromptRegistry},
    },
    clients::llm::openai_client::OpenAIChat,
    llm::LLMMessage,
};

const USAGE: &str =
    "Usage: cargo run -p tachikoma-core --example basic_agent -- [--debug] MODEL [\"TASK\"]";

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1).peekable();
    let debug = args.next_if(|argument| argument == "--debug").is_some();
    let model = args
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, USAGE))?;
    if model == "--help" || model == "-h" {
        println!(
            "{USAGE}\n\n--debug  Print prompts, tool definitions, model text and tool-call arguments to stderr."
        );
        return Ok(());
    }
    let task = args.next().unwrap_or_else(|| "What is 2 + 2?".into());
    if model.trim().is_empty() || task.trim().is_empty() || args.next().is_some() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, USAGE).into());
    }

    let level = if debug {
        tracing::Level::DEBUG
    } else {
        tracing::Level::INFO
    };
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::filter::Targets::new()
                .with_target("tachikoma_core", level)
                .with_target("basic_agent", level),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_target(false)
                .with_ansi(false),
        )
        .init();

    tracing::info!("Loading agent prompt and output schema");
    let resources = Path::new(env!("CARGO_MANIFEST_DIR")).join("resources");
    let prompts = PromptRegistry::from_directory(
        resources.join("templates"),
        PromptSource::Reference("configurable_agent_prefix".into()),
    )?;
    let validators = OutputValidatorRegistry::from_directory(resources.join("output_schemas"))?;
    tracing::debug!(model = ?model, connection = "chatgpt_subscription", "Example connection");
    tracing::info!("Restoring saved ChatGPT authorization from the OS credential store");
    let client = OpenAIChat::with_subscription(
        OpenAIConfig {
            model: Some(model),
            preferred_auth: Some(OpenAIAuthMode::Subscription),
            api_key: None,
        },
        RetryConfig::default(),
    )
    .await?;
    let config = AgentConfig::new(
        PromptSource::Reference("default_agent".into()),
        OutputSchemaSource::Reference("answer".into()),
    );
    let mut agent =
        ConfigurableAgent::new(config, Arc::new(client), &prompts, &validators, vec![])?;

    let outcome = agent.invoke(&task).await?;
    let turns = agent
        .history()
        .iter()
        .filter(|message| matches!(message, LLMMessage::Assistant(_)))
        .count();
    eprintln!("Model turns: {turns}");
    match outcome {
        AgentOutcome::Completed { tool_name, output } => {
            println!(
                "Completed via {tool_name}\n{}",
                serde_json::to_string_pretty(&output)?
            );
            Ok(())
        }
        AgentOutcome::Unsatisfactory { text } => {
            println!(
                "Model returned without a tool call:\n{}",
                text.as_deref().unwrap_or("<no text>")
            );
            Err(io::Error::other("Unsatisfactory: the model did not call a final tool.").into())
        }
    }
}

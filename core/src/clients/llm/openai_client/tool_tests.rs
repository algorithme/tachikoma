use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{Json, Router, http::HeaderMap, routing::post};

use super::*;
use crate::{
    agent::{
        service::OutputValidator,
        tool::{AnswerTool, Tool},
    },
    llm::{ToolDefinition, ToolResult},
};

fn definitions() -> Vec<ToolDefinition> {
    let schema = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/resources/output_schemas/answer.json"
    )))
    .unwrap();
    let answer = AnswerTool::new(Arc::new(OutputValidator::new(schema).unwrap()));
    vec![
        answer.definition().clone(),
        ToolDefinition {
            name: "lookup".into(),
            description: "Look up the requested value before answering.".into(),
            // Open-ended objects remain flexible instead of silently closing the schema.
            input_schema: json!({
                "type": "object", "properties": {"query": {"type": "string"}},
                "required": ["query"]
            }),
        },
    ]
}

fn call_output(name: &str, call_id: &str, arguments: Value) -> Value {
    json!({
        "type": "function_call", "status": "completed",
        "namespace": TOOL_NAMESPACE, "name": name,
        "call_id": call_id, "arguments": arguments.to_string()
    })
}

fn sse_completed(output: Value) -> String {
    let mut events = String::new();
    for (output_index, item) in output.as_array().unwrap().iter().enumerate() {
        events.push_str(&format!(
            "data: {}\n\n",
            json!({
                "type": "response.output_item.done", "output_index": output_index,
                "item": item
            })
        ));
    }
    events.push_str(&format!(
        "data: {}\n\n",
        json!({"type": "response.completed", "response": {
            "status": "completed", "output": []
        }})
    ));
    events
}

#[tokio::test]
async fn subscription_tools_round_trip_with_namespaces_and_original_definitions() {
    let tools = definitions();
    let first_output = json!([
        {"id": "reasoning-1", "type": "reasoning", "summary": [],
         "encrypted_content": "opaque-reasoning-for-replay"},
        call_output("lookup", "lookup-1", json!({"query": "meaning"}))
    ]);
    let expected_tools = tools.clone();
    let expected_output = first_output.clone();
    let attempts = Arc::new(AtomicUsize::new(0));
    let requests = attempts.clone();
    let app = Router::new().route(
        "/responses",
        post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let attempt = requests.fetch_add(1, Ordering::SeqCst);
            let tools = expected_tools.clone();
            let output = expected_output.clone();
            async move {
                assert_eq!(headers["authorization"], "Bearer fake-subscription-token");
                assert_eq!(headers["accept"], "text/event-stream");
                assert_eq!(body["model"], "gpt-6-luna");
                assert_eq!(body["instructions"], "Use lookup, then answer.");
                assert_eq!(body["store"], false);
                assert_eq!(body["stream"], true);
                assert_eq!(body["tool_choice"], "auto");
                assert_eq!(body["parallel_tool_calls"], true);
                assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
                let namespaces = body["tools"].as_array().unwrap();
                assert_eq!(namespaces.len(), 1);
                assert_eq!(namespaces[0]["type"], "namespace");
                assert_eq!(namespaces[0]["name"], TOOL_NAMESPACE);
                assert!(!namespaces[0]["description"].as_str().unwrap().is_empty());
                let functions = namespaces[0]["tools"].as_array().unwrap();
                assert_eq!(functions.len(), tools.len());
                for ((function, definition), strict) in
                    functions.iter().zip(&tools).zip([true, false])
                {
                    assert_eq!(
                        function,
                        &json!({
                            "type": "function", "name": definition.name,
                            "description": definition.description,
                            "parameters": definition.input_schema, "strict": strict
                        })
                    );
                }
                if attempt == 0 {
                    assert_eq!(
                        body["input"],
                        json!([{"role": "user", "content": "Find the meaning."}])
                    );
                    sse_completed(output)
                } else {
                    assert_eq!(attempt, 1);
                    assert_eq!(
                        body["input"],
                        json!([
                            {"role": "user", "content": "Find the meaning."},
                            output[0], output[1],
                            {"type": "function_call_output", "call_id": "lookup-1",
                             "output": "{\"meaning\":42}"}
                        ])
                    );
                    sse_completed(json!([call_output(
                        "answer",
                        "answer-1",
                        json!({"text": "The meaning is 42."})
                    )]))
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/responses", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = http_client(Duration::from_secs(5)).unwrap();
    let mut history = vec![LLMMessage::User("Find the meaning.".into())];
    let mut responses = Vec::new();
    for turn in 0..2 {
        let body = request_body(
            "gpt-6-luna",
            LLMRequest {
                system_prompt: "Use lookup, then answer.",
                messages: &history,
                tools: &tools,
            },
            ToolSchemaMode::Auto,
            Some(TOOL_NAMESPACE),
        )
        .unwrap();
        let http_response = client
            .post(&url)
            .bearer_auth("fake-subscription-token")
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&body)
            .send()
            .await
            .unwrap();
        let response = read_response(http_response, Some(TOOL_NAMESPACE))
            .await
            .unwrap();
        if turn == 0 {
            history.push(LLMMessage::Assistant(response.replay.clone()));
            history.push(LLMMessage::ToolResults(vec![ToolResult {
                call_id: response.tool_calls[0].id.clone(),
                outcome: Ok(json!({"meaning": 42})),
            }]));
        }
        responses.push(response);
    }
    server.abort();
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(
        responses[0].replay,
        AssistantReplay::OpenAI(first_output.as_array().unwrap().clone())
    );
    for (response, name, arguments) in [
        (&responses[0], "lookup", json!({"query": "meaning"})),
        (
            &responses[1],
            "answer",
            json!({"text": "The meaning is 42."}),
        ),
    ] {
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, name);
        assert_eq!(
            response.tool_calls[0].arguments,
            ToolArguments::Json(arguments)
        );
    }
}

#[test]
fn namespaces_must_match_the_tool_surface_used_for_the_request() {
    for (namespace, subscription_valid, flat_valid) in [
        (Some(json!(TOOL_NAMESPACE)), true, false),
        (None, false, true),
        (Some(Value::Null), false, true),
        (Some(json!("unexpected")), false, false),
        (Some(json!("")), false, false),
        (Some(json!(42)), false, false),
    ] {
        let mut call = call_output("answer", "answer-1", json!({"text": "Done"}));
        match namespace {
            Some(value) => call["namespace"] = value,
            None => {
                call.as_object_mut().unwrap().remove("namespace");
            }
        }
        let response =
            completed_response(&json!({"status": "completed", "output": [call]})).unwrap();
        for (expected, valid) in [
            (Some(TOOL_NAMESPACE), subscription_valid),
            (None, flat_valid),
        ] {
            assert_eq!(
                validate_namespace(&response, expected),
                if valid {
                    Ok(())
                } else {
                    Err(LLMError::InvalidResponse)
                }
            );
        }
    }
}

#[test]
fn auto_schema_flags_preserve_bundled_answer_and_flexible_tools_on_the_wire() {
    let tools = definitions();
    for (model, answer_strict) in [("gpt-6-luna", true), ("unrecognized-model", false)] {
        let body = request_body(
            model,
            LLMRequest {
                system_prompt: "Answer the user.",
                messages: &[LLMMessage::User("Hello".into())],
                tools: &tools,
            },
            ToolSchemaMode::Auto,
            Some(TOOL_NAMESPACE),
        )
        .unwrap();
        let functions = body["tools"][0]["tools"].as_array().unwrap();
        assert_eq!(functions[0]["strict"], answer_strict);
        assert_eq!(functions[1]["strict"], false);
        for (function, definition) in functions.iter().zip(&tools) {
            assert_eq!(function["parameters"], definition.input_schema);
            assert_eq!(function["description"], definition.description);
        }
    }
}

use serde_json::{Value, json};

use super::{AssistantReplay, LLMError, LLMResponse, ResponseStream, ToolArguments};

fn call(id: &str, arguments: &str) -> Value {
    json!({
        "type": "function_call", "id": format!("item-{id}"), "status": "completed",
        "call_id": id, "name": "answer", "namespace": "agent", "arguments": arguments
    })
}

fn done(index: u64, item: Value) -> Value {
    json!({"type": "response.output_item.done", "output_index": index, "item": item})
}

fn added(index: u64, item: Value) -> Value {
    json!({"type": "response.output_item.added", "output_index": index, "item": item})
}

fn completed(output: Value) -> Value {
    json!({"type": "response.completed", "response": {"status": "completed", "output": output}})
}

fn stream(events: &[Value]) -> String {
    events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}

fn feed(
    parser: &mut ResponseStream,
    stream: &str,
    chunk_size: usize,
) -> Result<Option<LLMResponse>, LLMError> {
    let mut response = None;
    for chunk in stream.as_bytes().chunks(chunk_size) {
        if let Some(completed) = parser.push(chunk)? {
            assert!(
                response.is_none(),
                "one stream must produce only one response"
            );
            response = Some(completed);
        }
    }
    Ok(response)
}

#[test]
fn empty_completion_restores_opaque_items_in_output_order_across_chunk_boundaries() {
    let items = vec![
        json!({
            "type": "reasoning", "id": "reasoning-one", "summary": [],
            "encrypted_content": "opaque-test-content", "opaque_future_field": {"preserved": true}
        }),
        call("one", r#"{"text":"Grüezi"}"#),
        call("two", r#"{"text":"Second"}"#),
        json!({
            "type": "message", "id": "message-one", "role": "assistant", "status": "completed",
            "content": [{"type": "output_text", "text": "Grüezi", "annotations": []}]
        }),
    ];
    let prefix = stream(&[
        done(3, items[3].clone()),
        done(1, items[1].clone()),
        done(0, items[0].clone()),
        done(2, items[2].clone()),
    ]);
    let completion = stream(&[completed(json!([]))]);

    for chunk_size in [1, 7, 64, usize::MAX] {
        let mut parser = ResponseStream::default();
        assert!(feed(&mut parser, &prefix, chunk_size).unwrap().is_none());
        let response = feed(&mut parser, &completion, chunk_size)
            .unwrap()
            .expect("successful completion must expose the finished items");
        assert_eq!(response.text.as_deref(), Some("Grüezi"));
        assert_eq!(response.tool_calls.len(), 2);
        assert_eq!(response.tool_calls[0].id, "one");
        assert_eq!(response.tool_calls[1].id, "two");
        assert_eq!(
            response.tool_calls[0].arguments,
            ToolArguments::Json(json!({"text": "Grüezi"}))
        );
        assert_eq!(response.replay, AssistantReplay::OpenAI(items.clone()));
    }
}

#[test]
fn finished_tool_items_stay_unavailable_without_successful_overall_completion() {
    let prefix = stream(&[done(0, call("one", "{}"))]);
    for (ending, expected) in [
        (
            stream(&[json!({"type": "response.incomplete"})]),
            LLMError::IncompleteResponse,
        ),
        (
            stream(&[json!({"type": "response.failed"})]),
            LLMError::ProviderFailure,
        ),
        ("data: [DONE]\n\n".to_owned(), LLMError::IncompleteResponse),
    ] {
        let mut parser = ResponseStream::default();
        assert!(feed(&mut parser, &prefix, 3).unwrap().is_none());
        assert_eq!(feed(&mut parser, &ending, 3), Err(expected));
    }

    // EOF or a truncated completion leaves no response for the transport to return.
    let mut parser = ResponseStream::default();
    assert!(feed(&mut parser, &prefix, 1).unwrap().is_none());
    assert!(
        feed(&mut parser, "data: {\"type\":\"response.completed\"", 1)
            .unwrap()
            .is_none()
    );

    let partial = json!({"type": "function_call", "status": "in_progress"});
    for mut events in [
        vec![
            added(0, partial.clone()),
            done(0, call("one", "{}")),
            added(1, partial.clone()),
        ],
        vec![added(0, partial)],
    ] {
        events.push(completed(json!([])));
        assert_eq!(
            feed(&mut ResponseStream::default(), &stream(&events), 3),
            Err(LLMError::IncompleteResponse)
        );
    }
}

#[test]
fn finished_payload_replaces_the_announced_partial_payload() {
    let partial = json!({
        "type": "function_call", "id": "item-one", "status": "in_progress",
        "call_id": "one", "name": "answer", "namespace": "agent",
        "arguments": "{\"text\":\"Partial"
    });
    let final_item = call("one", r#"{"text":"Final"}"#);
    let events = stream(&[
        added(0, partial),
        done(0, final_item.clone()),
        completed(json!([])),
    ]);
    let response = feed(&mut ResponseStream::default(), &events, 7)
        .unwrap()
        .unwrap();
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(
        response.tool_calls[0].arguments,
        ToolArguments::Json(json!({"text": "Final"}))
    );
    assert_eq!(response.replay, AssistantReplay::OpenAI(vec![final_item]));
}

#[test]
fn malformed_or_ambiguous_finished_items_are_rejected() {
    let item = call("one", "{}");
    let mut invalid = vec![
        vec![done(0, item.clone()), done(0, item.clone())],
        vec![done(1, item.clone())],
        vec![done(0, item.clone()), done(2, call("two", "{}"))],
        vec![json!({"type": "response.output_item.done", "item": item})],
        vec![json!({"type": "response.output_item.done", "output_index": 0})],
        vec![done(0, Value::Null)],
        vec![done(0, json!([]))],
    ];
    for index in [json!(-1), json!(0.5), json!("0"), Value::Null] {
        invalid.push(vec![json!({
            "type": "response.output_item.done", "output_index": index, "item": item
        })]);
    }
    for mut events in invalid {
        events.push(completed(json!([])));
        assert_eq!(
            feed(&mut ResponseStream::default(), &stream(&events), 5),
            Err(LLMError::InvalidResponse)
        );
    }
}

#[test]
fn nonempty_final_output_is_authoritative_and_never_duplicates_done_items() {
    let reasoning = json!({"type": "reasoning", "id": "reasoning-one", "summary": []});
    let final_items = vec![reasoning.clone(), call("one", r#"{"text":"Final"}"#)];
    let events = stream(&[
        done(0, reasoning),
        done(1, call("one", r#"{"text":"Earlier"}"#)),
        completed(json!(final_items)),
    ]);
    let response = feed(&mut ResponseStream::default(), &events, 11)
        .unwrap()
        .unwrap();
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(
        response.tool_calls[0].arguments,
        ToolArguments::Json(json!({"text": "Final"}))
    );
    assert_eq!(response.replay, AssistantReplay::OpenAI(final_items));
}

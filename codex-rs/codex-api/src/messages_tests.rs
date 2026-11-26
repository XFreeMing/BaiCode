use super::*;

#[test]
fn malformed_function_arguments_reach_the_tool_runner() {
    let block = json!({
        "type": "tool_use",
        "id": "toolu_patch",
        "name": "apply_patch",
        "input": {},
        "partial_json": "{\"input\":",
    });

    let item = response_item("message_0", &block, &BTreeMap::new())
        .expect("a malformed argument must not terminate the stream");

    assert!(matches!(
        item,
        Some(ResponseItem::FunctionCall {
            call_id,
            arguments,
            ..
        }) if call_id == "toolu_patch" && arguments == "{\"input\":"
    ));
}

#[test]
fn reports_gateway_errors_without_an_event_type() {
    let mut stream = MessagesStream::default();
    let event = json!({"error": {"type": "overloaded_error", "message": "Gateway busy"}});

    let result = stream.handle(event, &BTreeMap::new());

    assert!(matches!(result, Err(ApiError::ServerOverloaded { .. })));
}

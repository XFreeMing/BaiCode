//! Translate Messages requests and events at the HTTP boundary.

use crate::common::ResponseEvent;
use crate::common::ResponseStream;
use crate::common::ResponsesApiRequest;
use crate::error::ApiError;
use crate::telemetry::SseTelemetry;
use codex_client::StreamResponse;
use codex_protocol::error::ResponseIncompleteError;
use codex_protocol::models::ReasoningItemContent;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio::time::timeout;

const SIGNATURE_PREFIX: &str = "messages-signature:";

pub(crate) struct MessagesRequest {
    pub(crate) body: Value,
    tools: BTreeMap<String, ToolName>,
}

struct ToolName {
    name: String,
    namespace: Option<String>,
    custom: bool,
}

#[derive(Serialize)]
struct Message {
    role: &'static str,
    content: Vec<Value>,
}

impl MessagesRequest {
    pub(crate) fn from_responses(request: ResponsesApiRequest) -> Result<Self, ApiError> {
        let mut system = Vec::new();
        let mut messages = Vec::new();
        for item in request.input {
            match item {
                ResponseItem::Reasoning {
                    content: Some(content),
                    encrypted_content,
                    ..
                } => {
                    let thinking: String = content
                        .into_iter()
                        .map(|part| match part {
                            ReasoningItemContent::ReasoningText { text }
                            | ReasoningItemContent::Text { text } => text,
                        })
                        .collect();
                    let signature = encrypted_content
                        .as_deref()
                        .and_then(|value| value.strip_prefix(SIGNATURE_PREFIX))
                        .unwrap_or_default();
                    if !thinking.is_empty() {
                        push_content(
                            &mut messages,
                            "assistant",
                            json!({
                                "type": "thinking", "thinking": thinking, "signature": signature,
                            }),
                        );
                    }
                }
                item => append_item(&mut system, &mut messages, &item)?,
            }
        }
        if let Some(format) = request.text.and_then(|text| text.format) {
            system.push(format!(
                "Return JSON that conforms to this schema: {}",
                format.schema
            ));
        }
        // Messages requires all results before any text in a tool-result message.
        for message in messages.iter_mut().filter(|message| message.role == "user") {
            message
                .content
                .sort_by_key(|block| block["type"] != "tool_result");
        }

        let mut tools = BTreeMap::new();
        let mut tool_specs = Vec::new();
        if let Some(specs) = request.tools {
            let specs: Vec<Value> = serde_json::from_str(specs.as_raw_value().get())
                .map_err(|_| invalid_request("invalid tool definitions"))?;
            for spec in specs {
                if spec["type"] == "namespace" {
                    if let Some(children) = spec["tools"].as_array() {
                        for child in children {
                            add_tool(&mut tools, &mut tool_specs, spec["name"].as_str(), child)?;
                        }
                    }
                } else {
                    add_tool(&mut tools, &mut tool_specs, None, &spec)?;
                }
            }
        }

        let effort = request.reasoning.and_then(|reasoning| reasoning.effort);
        let thinking = request.tool_choice != "required"
            && effort
                .as_ref()
                .is_some_and(|effort| effort.as_str() != "none");
        let mut body = json!({
            "model": request.model,
            "max_tokens": 16384,
            "messages": messages,
            "stream": true,
            "thinking": if thinking {
                json!({"type": "enabled", "budget_tokens": 1024})
            } else {
                json!({"type": "disabled"})
            },
        });
        if !system.is_empty() {
            body["system"] = json!(system.join("\n\n"));
        }
        if !tool_specs.is_empty() {
            let tool_choice = match request.tool_choice.as_str() {
                "required" => "any",
                "none" => "none",
                _ => "auto",
            };
            body["tools"] = json!(tool_specs);
            body["tool_choice"] = json!({"type": tool_choice});
        }
        if let Some(effort) = effort.filter(|_| thinking) {
            let effort = match effort.as_str() {
                "minimal" | "low" => "low",
                "max" => "max",
                _ => "high",
            };
            body["output_config"] = json!({"effort": effort});
        }
        Ok(Self { body, tools })
    }

    pub(crate) fn stream(
        self,
        response: StreamResponse,
        idle_timeout: Duration,
        telemetry: Option<Arc<dyn SseTelemetry>>,
    ) -> ResponseStream {
        let upstream_request_id = response
            .headers
            .get("x-request-id")
            .or_else(|| response.headers.get("request-id"))
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let (tx, rx_event) = mpsc::channel(1600);
        tokio::spawn(async move {
            let mut stream = response.bytes.eventsource();
            let mut state = MessagesStream::default();
            loop {
                let start = Instant::now();
                let next = tokio::select! {
                    biased;
                    _ = tx.closed() => return,
                    next = timeout(idle_timeout, stream.next()) => next,
                };
                if let Some(telemetry) = &telemetry {
                    telemetry.on_sse_poll(&next, start.elapsed());
                }
                let events = match next {
                    Ok(Some(Ok(event))) => serde_json::from_str(&event.data)
                        .map_err(|_| stream_error("could not decode an SSE event"))
                        .and_then(|event| state.handle(event, &self.tools)),
                    Ok(Some(Err(eventsource_stream::EventStreamError::Transport(error)))) => {
                        Err(ApiError::Transport(error))
                    }
                    Ok(Some(Err(_))) => Err(stream_error("could not read an SSE event")),
                    Ok(None) => Err(stream_error("stream closed before message_stop")),
                    Err(_) => Err(stream_error("idle timeout waiting for SSE")),
                };
                match events {
                    Ok(events) => {
                        for event in events {
                            let done = matches!(event, ResponseEvent::Completed { .. });
                            if tx.send(Ok(event)).await.is_err() || done {
                                return;
                            }
                        }
                    }
                    Err(error) => {
                        let _ = tx.send(Err(error)).await;
                        return;
                    }
                }
            }
        });
        ResponseStream {
            routing_affinity: None,
            rx_event,
            upstream_request_id,
            interrupt: None,
        }
    }
}

fn push_content(messages: &mut Vec<Message>, role: &'static str, block: Value) {
    if let Some(message) = messages.last_mut()
        && message.role == role
    {
        message.content.push(block);
    } else {
        messages.push(Message {
            role,
            content: vec![block],
        });
    }
}

fn append_item(
    system: &mut Vec<String>,
    messages: &mut Vec<Message>,
    item: &ResponseItem,
) -> Result<(), ApiError> {
    let value = serde_json::to_value(item).map_err(|_| invalid_request("invalid history item"))?;
    match item {
        ResponseItem::Message { role, .. } => {
            for part in value["content"].as_array().into_iter().flatten() {
                if matches!(role.as_str(), "system" | "developer") {
                    if let Some(text) = part["text"].as_str().filter(|text| !text.is_empty()) {
                        system.push(text.to_owned());
                        continue;
                    }
                }
                if let Some(content) = message_content(part)? {
                    let role = if role == "assistant" {
                        "assistant"
                    } else {
                        "user"
                    };
                    push_content(messages, role, content);
                }
            }
        }
        ResponseItem::FunctionCall {
            name,
            namespace,
            arguments,
            call_id,
            ..
        } => {
            let arguments: Value = serde_json::from_str(arguments).unwrap_or_else(|_| json!({}));
            push_content(
                messages,
                "assistant",
                json!({
                    "type": "tool_use", "id": call_id,
                    "name": wire_tool_name(namespace.as_deref(), name), "input": arguments,
                }),
            );
        }
        ResponseItem::CustomToolCall {
            name,
            namespace,
            input,
            call_id,
            ..
        } => {
            push_content(
                messages,
                "assistant",
                json!({
                    "type": "tool_use", "id": call_id,
                    "name": wire_tool_name(namespace.as_deref(), name), "input": {"input": input},
                }),
            );
        }
        ResponseItem::FunctionCallOutput {
            call_id: Some(call_id),
            ..
        }
        | ResponseItem::CustomToolCallOutput { call_id, .. } => {
            let content = if let Some(parts) = value["output"].as_array() {
                let parts = parts
                    .iter()
                    .map(message_content)
                    .collect::<Result<Vec<_>, _>>()?;
                json!(parts.into_iter().flatten().collect::<Vec<_>>())
            } else {
                value["output"].clone()
            };
            push_content(
                messages,
                "user",
                json!({
                    "type": "tool_result", "tool_use_id": call_id, "content": content,
                }),
            );
        }
        _ => {}
    }
    Ok(())
}

fn message_content(part: &Value) -> Result<Option<Value>, ApiError> {
    match part["type"].as_str() {
        Some("input_text" | "output_text") => Ok(part["text"]
            .as_str()
            .filter(|text| !text.is_empty())
            .map(|text| json!({"type": "text", "text": text}))),
        Some("input_image") => {
            let source = if let Some(file_id) = part["file_id"].as_str() {
                json!({"type": "file", "file_id": file_id})
            } else if let Some(url) = part["image_url"].as_str() {
                if let Some(data) = url.strip_prefix("data:") {
                    let (media_type, data) = data.split_once(";base64,").ok_or_else(|| {
                        invalid_request("inline images require a base64 data URL")
                    })?;
                    json!({"type": "base64", "media_type": media_type, "data": data})
                } else {
                    json!({"type": "url", "url": url})
                }
            } else {
                return Err(invalid_request("image content has no URL or file ID"));
            };
            Ok(Some(json!({"type": "image", "source": source})))
        }
        Some("input_audio") => Err(invalid_request("audio input is unavailable for Messages")),
        _ => Ok(None),
    }
}

fn wire_tool_name(namespace: Option<&str>, name: &str) -> String {
    match namespace {
        Some(namespace) => format!("{namespace}__{name}"),
        None => name.to_owned(),
    }
}

fn add_tool(
    tools: &mut BTreeMap<String, ToolName>,
    specs: &mut Vec<Value>,
    namespace: Option<&str>,
    spec: &Value,
) -> Result<(), ApiError> {
    let custom = match spec["type"].as_str() {
        Some("function") => false,
        Some("custom") => true,
        _ => return Ok(()),
    };
    let name = spec["name"]
        .as_str()
        .ok_or_else(|| invalid_request("tool has no name"))?;
    let wire_name = wire_tool_name(namespace, name);
    let input_schema = if custom {
        json!({
            "type": "object",
            "properties": {"input": {"type": "string", "description": "Tool input"}},
            "required": ["input"],
            "additionalProperties": false,
        })
    } else {
        spec["parameters"].clone()
    };
    let mut description = spec["description"].as_str().unwrap_or_default().to_owned();
    if let Some(grammar) = spec["format"]["definition"].as_str() {
        description.push_str("\n\nInput grammar:\n");
        description.push_str(grammar);
    }
    if tools
        .insert(
            wire_name.clone(),
            ToolName {
                name: name.to_owned(),
                namespace: namespace.map(str::to_owned),
                custom,
            },
        )
        .is_some()
    {
        return Err(invalid_request(
            "tool names collide after adding namespaces",
        ));
    }
    specs
        .push(json!({"name": wire_name, "description": description, "input_schema": input_schema}));
    Ok(())
}

#[derive(Default)]
struct MessagesStream {
    response_id: Option<String>,
    stop_reason: Option<String>,
    usage: Option<TokenUsage>,
    blocks: BTreeMap<u64, Value>,
}

impl MessagesStream {
    fn handle(
        &mut self,
        event: Value,
        tools: &BTreeMap<String, ToolName>,
    ) -> Result<Vec<ResponseEvent>, ApiError> {
        let mut events = Vec::new();
        let event_type = event["type"]
            .as_str()
            .or_else(|| event["error"].is_object().then_some("error"));
        match event_type {
            Some("message_start") => {
                let message = &event["message"];
                self.response_id = message["id"].as_str().map(str::to_owned);
                self.update_usage(&message["usage"]);
                events.push(ResponseEvent::Created {
                    response_id: self.response_id.clone(),
                });
            }
            Some("content_block_start" | "content_block_delta" | "content_block_stop") => {
                let index = event["index"]
                    .as_u64()
                    .ok_or_else(|| stream_error("missing block index"))?;
                let id = format!(
                    "{}_{}",
                    self.response_id.as_deref().unwrap_or("message"),
                    index
                );
                match event_type {
                    Some("content_block_start") => {
                        let block = event["content_block"].clone();
                        if let Some(item) = response_item(&id, &block, tools)? {
                            events.push(ResponseEvent::OutputItemAdded(item));
                        }
                        self.blocks.insert(index, block);
                    }
                    Some("content_block_delta") => {
                        let block = self
                            .blocks
                            .get_mut(&index)
                            .ok_or_else(|| stream_error("delta for an unknown block"))?;
                        if let Some(delta) = apply_delta(block, &event["delta"]) {
                            events.push(delta);
                        }
                    }
                    _ => {
                        let block = self
                            .blocks
                            .remove(&index)
                            .ok_or_else(|| stream_error("end of an unknown block"))?;
                        if let Some(item) = response_item(&id, &block, tools)? {
                            events.push(ResponseEvent::OutputItemDone(item));
                        }
                    }
                }
            }
            Some("message_delta") => {
                self.stop_reason = event["delta"]["stop_reason"].as_str().map(str::to_owned);
                self.update_usage(&event["usage"]);
            }
            Some("message_stop") => {
                let response_id = self
                    .response_id
                    .take()
                    .ok_or_else(|| stream_error("message_stop without message_start"))?;
                let reason = self.stop_reason.as_deref();
                if reason == Some("refusal") {
                    return Err(ApiError::ContentFilter);
                }
                if matches!(reason, Some("max_tokens" | "model_context_window_exceeded")) {
                    return Err(ApiError::ResponseIncomplete(Box::new(
                        ResponseIncompleteError {
                            reason: reason.unwrap_or_default().to_owned(),
                            response_id: Some(response_id),
                            partial_output: Vec::new(),
                            token_usage: self.usage.take(),
                        },
                    )));
                }
                events.push(ResponseEvent::Completed {
                    response_id,
                    token_usage: self.usage.take(),
                    usage_metadata: None,
                    reasoning_removals: None,
                    end_turn: reason.map(|reason| reason != "tool_use"),
                });
            }
            Some("error") => {
                let error = &event["error"];
                let message = error["message"].as_str().unwrap_or("gateway error");
                return Err(match error["type"].as_str() {
                    Some("invalid_request_error") => invalid_request(message),
                    Some("overloaded_error") => ApiError::ServerOverloaded { retry_after: None },
                    _ => stream_error(message),
                });
            }
            _ => {}
        }
        Ok(events)
    }

    fn update_usage(&mut self, value: &Value) {
        if !value.is_object() {
            return;
        }
        let usage = self.usage.get_or_insert_with(TokenUsage::default);
        if let Some(input) = value["input_tokens"].as_i64() {
            usage.cached_input_tokens = value["cache_read_input_tokens"].as_i64().unwrap_or(0);
            usage.cache_write_input_tokens =
                value["cache_creation_input_tokens"].as_i64().unwrap_or(0);
            usage.input_tokens = input
                .saturating_add(usage.cached_input_tokens)
                .saturating_add(usage.cache_write_input_tokens);
        }
        if let Some(output) = value["output_tokens"].as_i64() {
            usage.output_tokens = output;
        }
        usage.total_tokens = usage.input_tokens.saturating_add(usage.output_tokens);
    }
}

fn apply_delta(block: &mut Value, delta: &Value) -> Option<ResponseEvent> {
    let field = match delta["type"].as_str()? {
        "text_delta" => "text",
        "thinking_delta" => "thinking",
        "signature_delta" => "signature",
        "input_json_delta" => "partial_json",
        _ => return None,
    };
    let fragment = delta[field].as_str()?;
    if let Some(Value::String(text)) = block.get_mut(field) {
        text.push_str(fragment);
    } else {
        block[field] = json!(fragment);
    }
    match field {
        "text" => Some(ResponseEvent::OutputTextDelta(fragment.to_owned())),
        "thinking" => Some(ResponseEvent::ReasoningContentDelta {
            delta: fragment.to_owned(),
            content_index: 0,
        }),
        _ => None,
    }
}

fn response_item(
    id: &str,
    block: &Value,
    tools: &BTreeMap<String, ToolName>,
) -> Result<Option<ResponseItem>, ApiError> {
    let item = match block["type"].as_str() {
        Some("text") => json!({
            "type": "message", "id": id, "role": "assistant",
            "content": [{"type": "output_text", "text": block["text"]}],
        }),
        Some("thinking") => json!({
            "type": "reasoning", "id": id, "summary": [],
            "content": [{"type": "reasoning_text", "text": block["thinking"]}],
            "encrypted_content": block["signature"].as_str().filter(|s| !s.is_empty())
                .map(|signature| format!("{SIGNATURE_PREFIX}{signature}")),
        }),
        Some("tool_use") => {
            let wire_name = block["name"]
                .as_str()
                .ok_or_else(|| stream_error("tool has no name"))?;
            let partial_json = block["partial_json"]
                .as_str()
                .filter(|part| !part.is_empty());
            let (name, namespace, custom) = tools
                .get(wire_name)
                .map_or((wire_name, None, false), |tool| {
                    (tool.name.as_str(), tool.namespace.as_deref(), tool.custom)
                });
            if custom {
                let input: Value = if let Some(partial) = partial_json {
                    serde_json::from_str(partial)
                        .map_err(|_| stream_error("invalid custom tool input JSON"))?
                } else {
                    block["input"].clone()
                };
                json!({"type": "custom_tool_call", "id": id, "call_id": block["id"],
                    "name": name, "namespace": namespace, "input": input["input"].as_str().unwrap_or_default()})
            } else {
                // The tool runner reports malformed arguments to the model so it can retry.
                let arguments =
                    partial_json.map_or_else(|| block["input"].to_string(), str::to_owned);
                json!({"type": "function_call", "id": id, "call_id": block["id"],
                    "name": name, "namespace": namespace, "arguments": arguments})
            }
        }
        _ => return Ok(None),
    };
    serde_json::from_value(item)
        .map(Some)
        .map_err(|_| stream_error("invalid content block"))
}

fn invalid_request(message: &str) -> ApiError {
    ApiError::InvalidRequest {
        message: format!("Messages: {message}"),
    }
}

fn stream_error(message: &str) -> ApiError {
    ApiError::Stream(format!("Messages: {message}"))
}

#[cfg(test)]
#[path = "messages_tests.rs"]
mod tests;

use crate::agent::api_types::*;
use serde_json::{json, Value};

/// Which LLM provider's wire format to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Anthropic,
    OpenAi,
}

impl Provider {
    /// Return the URL path for this provider's chat/messages endpoint.
    pub fn url_path(&self) -> &'static str {
        match self {
            Provider::Anthropic => "/v1/messages",
            Provider::OpenAi => "/v1/chat/completions",
        }
    }

    /// Return the required HTTP headers.
    ///
    /// When `use_bearer` is true for the Anthropic provider, sends
    /// `Authorization: Bearer` instead of `x-api-key`. This matches Claude
    /// Code's ANTHROPIC_AUTH_TOKEN behavior for proxies/gateways.
    pub fn auth_headers(&self, api_key: &str, use_bearer: bool) -> Vec<(&'static str, String)> {
        match self {
            Provider::Anthropic if use_bearer => vec![
                ("authorization", format!("Bearer {api_key}")),
                ("anthropic-version", "2023-06-01".to_string()),
                ("content-type", "application/json".to_string()),
            ],
            Provider::Anthropic => vec![
                ("x-api-key", api_key.to_string()),
                ("anthropic-version", "2023-06-01".to_string()),
                ("content-type", "application/json".to_string()),
            ],
            Provider::OpenAi => vec![
                ("authorization", format!("Bearer {api_key}")),
                ("content-type", "application/json".to_string()),
            ],
        }
    }

    /// Serialize a `MessagesRequest` into the provider-specific JSON body.
    pub fn serialize_request(&self, req: &MessagesRequest) -> Value {
        match self {
            Provider::Anthropic => serialize_anthropic_request(req),
            Provider::OpenAi => serialize_openai_request(req),
        }
    }

    /// Deserialize a successful response body into our canonical `MessagesResponse`.
    pub fn deserialize_response(&self, body: &str) -> Result<MessagesResponse, ProviderError> {
        match self {
            Provider::Anthropic => deserialize_anthropic_response(body),
            Provider::OpenAi => deserialize_openai_response(body),
        }
    }

}

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("deserialization failed: {0}")]
    DeserializationFailed(String),
    #[error("content filtered: {message}")]
    ContentFiltered { message: String },
}

// ---------------------------------------------------------------------------
// Anthropic serialization
// ---------------------------------------------------------------------------

fn serialize_anthropic_request(req: &MessagesRequest) -> Value {
    let mut body = json!({
        "model": req.model,
        "max_tokens": req.max_tokens,
        "messages": serialize_anthropic_messages(&req.messages),
    });

    if let Some(system) = &req.system {
        body["system"] = json!(system);
    }

    if let Some(tools) = &req.tools {
        body["tools"] = json!(tools);
    }

    body
}

fn serialize_anthropic_messages(messages: &[Message]) -> Value {
    let msgs: Vec<Value> = messages
        .iter()
        .map(|m| {
            json!({
                "role": m.role,
                "content": serialize_anthropic_content(&m.content),
            })
        })
        .collect();
    json!(msgs)
}

fn serialize_anthropic_content(blocks: &[ContentBlock]) -> Value {
    let items: Vec<Value> = blocks
        .iter()
        .map(|b| match b {
            ContentBlock::Text { text } => json!({"type": "text", "text": text}),
            ContentBlock::ToolUse { id, name, input } => {
                json!({"type": "tool_use", "id": id, "name": name, "input": input})
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                let mut v = json!({
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": content,
                });
                if let Some(err) = is_error {
                    v["is_error"] = json!(err);
                }
                v
            }
            ContentBlock::Unknown => json!({"type": "unknown"}),
        })
        .collect();
    json!(items)
}

// ---------------------------------------------------------------------------
// Anthropic deserialization
// ---------------------------------------------------------------------------

fn deserialize_anthropic_response(body: &str) -> Result<MessagesResponse, ProviderError> {
    serde_json::from_str::<MessagesResponse>(body)
        .map_err(|e| ProviderError::DeserializationFailed(format!("Anthropic JSON: {e}")))
}

// ---------------------------------------------------------------------------
// OpenAI serialization
// ---------------------------------------------------------------------------

fn serialize_openai_request(req: &MessagesRequest) -> Value {
    let mut oai_messages: Vec<Value> = Vec::new();

    // System prompt becomes the first message with role "system"
    if let Some(system) = &req.system {
        oai_messages.push(json!({"role": "system", "content": system}));
    }

    for msg in &req.messages {
        let converted = convert_message_to_openai(msg);
        for m in converted {
            oai_messages.push(m);
        }
    }

    let mut body = json!({
        "model": req.model,
        "max_tokens": req.max_tokens,
        "messages": oai_messages,
    });

    if let Some(tools) = &req.tools {
        let oai_tools: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    }
                })
            })
            .collect();
        body["tools"] = json!(oai_tools);
    }

    body
}

fn convert_message_to_openai(msg: &Message) -> Vec<Value> {
    // For "user" role with tool results, each tool result becomes a separate
    // "tool" role message. Regular content stays as "user".
    // For "assistant" role, we collect ALL text and tool_calls and produce a
    // SINGLE message with both `content` and `tool_calls` fields.
    let mut result = Vec::new();
    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();

    for block in &msg.content {
        match block {
            ContentBlock::Text { text } => {
                text_parts.push(text.clone());
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error: _,
            } => {
                // Flush any accumulated text first
                if !text_parts.is_empty() {
                    result.push(json!({
                        "role": msg.role,
                        "content": text_parts.join("\n"),
                    }));
                    text_parts.clear();
                }
                result.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_use_id,
                    "content": content,
                }));
            }
            ContentBlock::ToolUse { id, name, input } => {
                // Accumulate tool calls to emit as a single assistant message.
                tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": input.to_string(),
                    }
                }));
            }
            ContentBlock::Unknown => {}
        }
    }

    // If we have tool_calls, produce a single assistant message with both
    // content (text) and tool_calls combined.
    if !tool_calls.is_empty() {
        let content_val = if text_parts.is_empty() {
            Value::Null
        } else {
            Value::String(text_parts.join("\n"))
        };
        result.push(json!({
            "role": "assistant",
            "content": content_val,
            "tool_calls": tool_calls,
        }));
        text_parts.clear();
    }

    // Flush remaining text (for non-tool messages)
    if !text_parts.is_empty() {
        let content = text_parts.join("\n");
        // For simple single-text user messages, content is a plain string
        result.push(json!({
            "role": msg.role,
            "content": content,
        }));
    }

    // If nothing was produced (empty content), still produce a message
    if result.is_empty() {
        result.push(json!({
            "role": msg.role,
            "content": "",
        }));
    }

    result
}

// ---------------------------------------------------------------------------
// OpenAI deserialization
// ---------------------------------------------------------------------------

fn deserialize_openai_response(body: &str) -> Result<MessagesResponse, ProviderError> {
    let v: Value = serde_json::from_str(body)
        .map_err(|e| ProviderError::DeserializationFailed(format!("OpenAI JSON: {e}")))?;

    let choices = v["choices"]
        .as_array()
        .ok_or_else(|| ProviderError::DeserializationFailed("missing choices array".into()))?;

    if choices.is_empty() {
        return Err(ProviderError::DeserializationFailed(
            "empty choices array".into(),
        ));
    }

    let first = &choices[0];
    let message = &first["message"];
    let finish_reason = first["finish_reason"]
        .as_str()
        .unwrap_or("stop")
        .to_string();

    // Map OpenAI finish_reason to Anthropic stop_reason
    let stop_reason = map_openai_finish_reason(&finish_reason)?;

    // Build content blocks
    let mut content_blocks: Vec<ContentBlock> = Vec::new();

    // Text content
    if let Some(text) = message["content"].as_str() {
        if !text.is_empty() {
            content_blocks.push(ContentBlock::Text {
                text: text.to_string(),
            });
        }
    }

    // Tool calls
    if let Some(tool_calls) = message["tool_calls"].as_array() {
        for tc in tool_calls {
            let id = tc["id"].as_str().unwrap_or("").to_string();
            let name = tc["function"]["name"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let arguments_str = tc["function"]["arguments"]
                .as_str()
                .unwrap_or("{}");
            let input: Value = serde_json::from_str(arguments_str).unwrap_or(json!({}));
            content_blocks.push(ContentBlock::ToolUse { id, name, input });
        }
    }

    // If we got no content and no tool calls, but content was null (common with
    // tool_calls responses), that's fine -- content_blocks may be empty or have
    // only tool uses.

    // Usage
    let usage = if let Some(usage_obj) = v.get("usage") {
        Usage {
            input_tokens: usage_obj["prompt_tokens"].as_u64().unwrap_or(0),
            output_tokens: usage_obj["completion_tokens"].as_u64().unwrap_or(0),
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        }
    } else {
        Usage {
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        }
    };

    let id = v["id"].as_str().unwrap_or("").to_string();

    Ok(MessagesResponse {
        id,
        content: content_blocks,
        stop_reason,
        usage,
    })
}

fn map_openai_finish_reason(reason: &str) -> Result<String, ProviderError> {
    match reason {
        "stop" => Ok("end_turn".to_string()),
        "length" => Ok("max_tokens".to_string()),
        "tool_calls" => Ok("tool_use".to_string()),
        "content_filter" => Err(ProviderError::ContentFiltered {
            message: "response filtered by content filter".to_string(),
        }),
        other => Ok(other.to_string()),
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn simple_request(system: Option<&str>) -> MessagesRequest {
        MessagesRequest {
            model: "claude-sonnet-4-20250514".to_string(),
            max_tokens: 4096,
            system: system.map(|s| s.to_string()),
            messages: vec![Message {
                role: "user".to_string(),
                content: vec![ContentBlock::Text {
                    text: "Hello".to_string(),
                }],
            }],
            tools: None,
            temperature: None,
            stop_sequences: None,
            tool_choice: None,
        }
    }

    fn request_with_tools() -> MessagesRequest {
        MessagesRequest {
            model: "gpt-4".to_string(),
            max_tokens: 1024,
            system: Some("You are helpful.".to_string()),
            messages: vec![Message {
                role: "user".to_string(),
                content: vec![ContentBlock::Text {
                    text: "Run ls".to_string(),
                }],
            }],
            tools: Some(vec![ToolDefinition {
                name: "bash".to_string(),
                description: "Run a bash command".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "command": {"type": "string"}
                    },
                    "required": ["command"]
                }),
            }]),
            temperature: None,
            stop_sequences: None,
            tool_choice: None,
        }
    }

    // -----------------------------------------------------------------------
    // Request serialization tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_anthropic_request_with_system_prompt() {
        let req = simple_request(Some("Be helpful."));
        let body = Provider::Anthropic.serialize_request(&req);

        assert_eq!(body["system"], "Be helpful.");
        assert_eq!(body["model"], "claude-sonnet-4-20250514");
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"][0]["type"], "text");
        assert_eq!(body["messages"][0]["content"][0]["text"], "Hello");
    }

    #[test]
    fn test_anthropic_request_without_system_prompt() {
        let req = simple_request(None);
        let body = Provider::Anthropic.serialize_request(&req);

        assert!(body.get("system").is_none());
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_anthropic_request_with_tools() {
        let req = request_with_tools();
        let body = Provider::Anthropic.serialize_request(&req);

        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "bash");
        assert_eq!(tools[0]["description"], "Run a bash command");
        assert!(tools[0]["input_schema"]["properties"]["command"].is_object());
    }

    #[test]
    fn test_anthropic_request_with_tool_choice() {
        // Anthropic tool definitions go at top level, not wrapped
        let req = request_with_tools();
        let body = Provider::Anthropic.serialize_request(&req);

        // Tools are direct objects with name/description/input_schema
        let tool = &body["tools"][0];
        assert!(tool.get("name").is_some());
        assert!(tool.get("input_schema").is_some());
        // NOT wrapped in {"type": "function", "function": {...}}
        assert!(tool.get("type").is_none() || tool["type"] != "function");
    }

    #[test]
    fn test_openai_request_system_as_first_message() {
        let req = simple_request(Some("Be concise."));
        let body = Provider::OpenAi.serialize_request(&req);

        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "Be concise.");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "Hello");
    }

    #[test]
    fn test_openai_request_no_system() {
        let req = simple_request(None);
        let body = Provider::OpenAi.serialize_request(&req);

        let messages = body["messages"].as_array().unwrap();
        // No system message, first message should be user
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn test_openai_request_tools_wrapped_in_function() {
        let req = request_with_tools();
        let body = Provider::OpenAi.serialize_request(&req);

        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "bash");
        assert_eq!(tools[0]["function"]["description"], "Run a bash command");
        assert!(tools[0]["function"]["parameters"]["properties"]["command"].is_object());
    }

    #[test]
    fn test_openai_request_content_as_string_for_simple_text() {
        let req = simple_request(Some("sys"));
        let body = Provider::OpenAi.serialize_request(&req);

        let messages = body["messages"].as_array().unwrap();
        // User content should be a plain string, not an array
        let user_msg = &messages[1];
        assert!(user_msg["content"].is_string());
        assert_eq!(user_msg["content"], "Hello");
    }

    #[test]
    fn test_openai_tool_result_as_role_tool_message() {
        let req = MessagesRequest {
            model: "gpt-4".to_string(),
            max_tokens: 1024,
            system: None,
            messages: vec![
                Message {
                    role: "user".to_string(),
                    content: vec![ContentBlock::Text {
                        text: "Run it".to_string(),
                    }],
                },
                Message {
                    role: "assistant".to_string(),
                    content: vec![ContentBlock::ToolUse {
                        id: "call_1".to_string(),
                        name: "bash".to_string(),
                        input: json!({"command": "ls"}),
                    }],
                },
                Message {
                    role: "user".to_string(),
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "call_1".to_string(),
                        content: "file.txt".to_string(),
                        is_error: None,
                    }],
                },
            ],
            tools: None,
            temperature: None,
            stop_sequences: None,
            tool_choice: None,
        };

        let body = Provider::OpenAi.serialize_request(&req);
        let messages = body["messages"].as_array().unwrap();

        // Find the tool role message
        let tool_msg = messages
            .iter()
            .find(|m| m["role"] == "tool")
            .expect("should have a role:tool message");
        assert_eq!(tool_msg["tool_call_id"], "call_1");
        assert_eq!(tool_msg["content"], "file.txt");
    }

    #[test]
    fn test_anthropic_tool_result_as_content_block() {
        let req = MessagesRequest {
            model: "claude-sonnet-4-20250514".to_string(),
            max_tokens: 4096,
            system: None,
            messages: vec![Message {
                role: "user".to_string(),
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "tu_1".to_string(),
                    content: "output here".to_string(),
                    is_error: Some(false),
                }],
            }],
            tools: None,
            temperature: None,
            stop_sequences: None,
            tool_choice: None,
        };

        let body = Provider::Anthropic.serialize_request(&req);
        let content = &body["messages"][0]["content"][0];
        assert_eq!(content["type"], "tool_result");
        assert_eq!(content["tool_use_id"], "tu_1");
        assert_eq!(content["content"], "output here");
        assert_eq!(content["is_error"], false);
    }

    // -----------------------------------------------------------------------
    // Response deserialization tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_anthropic_response_text() {
        let body = json!({
            "id": "msg_abc",
            "content": [{"type": "text", "text": "Hello!"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        })
        .to_string();

        let resp = Provider::Anthropic.deserialize_response(&body).unwrap();
        assert_eq!(resp.id, "msg_abc");
        assert_eq!(resp.stop_reason, "end_turn");
        assert_eq!(resp.content.len(), 1);
        match &resp.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "Hello!"),
            other => panic!("expected Text, got {:?}", other),
        }
        assert_eq!(resp.usage.input_tokens, 10);
        assert_eq!(resp.usage.output_tokens, 5);
    }

    #[test]
    fn test_anthropic_response_tool_use() {
        let body = json!({
            "id": "msg_tu",
            "content": [{
                "type": "tool_use",
                "id": "tu_001",
                "name": "bash",
                "input": {"command": "pwd"}
            }],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 50, "output_tokens": 20}
        })
        .to_string();

        let resp = Provider::Anthropic.deserialize_response(&body).unwrap();
        assert_eq!(resp.stop_reason, "tool_use");
        match &resp.content[0] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "tu_001");
                assert_eq!(name, "bash");
                assert_eq!(input["command"], "pwd");
            }
            other => panic!("expected ToolUse, got {:?}", other),
        }
    }

    #[test]
    fn test_anthropic_response_mixed_text_and_tool_use() {
        let body = json!({
            "id": "msg_mix",
            "content": [
                {"type": "text", "text": "Let me check."},
                {"type": "tool_use", "id": "tu_002", "name": "bash", "input": {"command": "ls"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 100, "output_tokens": 40}
        })
        .to_string();

        let resp = Provider::Anthropic.deserialize_response(&body).unwrap();
        assert_eq!(resp.content.len(), 2);
        assert!(matches!(&resp.content[0], ContentBlock::Text { .. }));
        assert!(matches!(&resp.content[1], ContentBlock::ToolUse { .. }));
    }

    #[test]
    fn test_openai_response_text() {
        let body = json!({
            "id": "chatcmpl-abc",
            "choices": [{
                "message": {"role": "assistant", "content": "Hi there!"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 15, "completion_tokens": 8}
        })
        .to_string();

        let resp = Provider::OpenAi.deserialize_response(&body).unwrap();
        assert_eq!(resp.id, "chatcmpl-abc");
        assert_eq!(resp.stop_reason, "end_turn");
        assert_eq!(resp.content.len(), 1);
        match &resp.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "Hi there!"),
            other => panic!("expected Text, got {:?}", other),
        }
    }

    #[test]
    fn test_openai_response_with_tool_calls() {
        let body = json!({
            "id": "chatcmpl-tc",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_123",
                        "type": "function",
                        "function": {
                            "name": "bash",
                            "arguments": "{\"command\":\"ls\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 20, "completion_tokens": 10}
        })
        .to_string();

        let resp = Provider::OpenAi.deserialize_response(&body).unwrap();
        assert_eq!(resp.stop_reason, "tool_use");
        assert_eq!(resp.content.len(), 1);
        match &resp.content[0] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "call_123");
                assert_eq!(name, "bash");
                assert_eq!(input["command"], "ls");
            }
            other => panic!("expected ToolUse, got {:?}", other),
        }
    }

    #[test]
    fn test_openai_response_null_content_with_tool_calls() {
        let body = json!({
            "id": "chatcmpl-null",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_456",
                        "type": "function",
                        "function": {
                            "name": "read_file",
                            "arguments": "{\"path\":\"/tmp/x\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 3}
        })
        .to_string();

        let resp = Provider::OpenAi.deserialize_response(&body).unwrap();
        // No text block should be present (content was null)
        assert_eq!(resp.content.len(), 1);
        assert!(matches!(&resp.content[0], ContentBlock::ToolUse { .. }));
    }

    #[test]
    fn test_openai_finish_reason_mapping() {
        // stop -> end_turn
        let body_stop = json!({
            "id": "x", "choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        }).to_string();
        let resp = Provider::OpenAi.deserialize_response(&body_stop).unwrap();
        assert_eq!(resp.stop_reason, "end_turn");

        // length -> max_tokens
        let body_length = json!({
            "id": "x", "choices": [{"message": {"content": "ok"}, "finish_reason": "length"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        }).to_string();
        let resp = Provider::OpenAi.deserialize_response(&body_length).unwrap();
        assert_eq!(resp.stop_reason, "max_tokens");

        // tool_calls -> tool_use
        let body_tc = json!({
            "id": "x",
            "choices": [{"message": {"content": null, "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "f", "arguments": "{}"}}
            ]}, "finish_reason": "tool_calls"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        }).to_string();
        let resp = Provider::OpenAi.deserialize_response(&body_tc).unwrap();
        assert_eq!(resp.stop_reason, "tool_use");
    }

    #[test]
    fn test_openai_usage_field_mapping() {
        let body = json!({
            "id": "x",
            "choices": [{"message": {"content": "hi"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 42, "completion_tokens": 17}
        })
        .to_string();

        let resp = Provider::OpenAi.deserialize_response(&body).unwrap();
        assert_eq!(resp.usage.input_tokens, 42);
        assert_eq!(resp.usage.output_tokens, 17);
        // Cache fields default to 0 for OpenAI
        assert_eq!(resp.usage.cache_creation_input_tokens, 0);
        assert_eq!(resp.usage.cache_read_input_tokens, 0);
    }

    #[test]
    fn test_openai_missing_usage() {
        // Some proxies omit the usage field entirely
        let body = json!({
            "id": "x",
            "choices": [{"message": {"content": "hi"}, "finish_reason": "stop"}]
        })
        .to_string();

        let resp = Provider::OpenAi.deserialize_response(&body).unwrap();
        assert_eq!(resp.usage.input_tokens, 0);
        assert_eq!(resp.usage.output_tokens, 0);
    }

    #[test]
    fn test_openai_multiple_choices_uses_first() {
        let body = json!({
            "id": "x",
            "choices": [
                {"message": {"content": "first"}, "finish_reason": "stop"},
                {"message": {"content": "second"}, "finish_reason": "stop"}
            ],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        })
        .to_string();

        let resp = Provider::OpenAi.deserialize_response(&body).unwrap();
        match &resp.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "first"),
            other => panic!("expected Text, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Auth header tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_anthropic_headers_api_key() {
        let headers = Provider::Anthropic.auth_headers("sk-ant-key", false);

        let map: std::collections::HashMap<&str, &str> = headers
            .iter()
            .map(|(k, v)| (*k, v.as_str()))
            .collect();

        assert_eq!(map["x-api-key"], "sk-ant-key");
        assert_eq!(map["anthropic-version"], "2023-06-01");
        assert_eq!(map["content-type"], "application/json");
        assert!(!map.contains_key("authorization"));
    }

    #[test]
    fn test_anthropic_headers_bearer_token() {
        let headers = Provider::Anthropic.auth_headers("my-auth-token", true);

        let map: std::collections::HashMap<&str, &str> = headers
            .iter()
            .map(|(k, v)| (*k, v.as_str()))
            .collect();

        assert_eq!(map["authorization"], "Bearer my-auth-token");
        assert_eq!(map["anthropic-version"], "2023-06-01");
        assert_eq!(map["content-type"], "application/json");
        // Should NOT have x-api-key when using bearer
        assert!(!map.contains_key("x-api-key"));
    }

    #[test]
    fn test_openai_headers() {
        let headers = Provider::OpenAi.auth_headers("sk-openai-key", true);

        let map: std::collections::HashMap<&str, &str> = headers
            .iter()
            .map(|(k, v)| (*k, v.as_str()))
            .collect();

        assert_eq!(map["authorization"], "Bearer sk-openai-key");
        assert_eq!(map["content-type"], "application/json");
        assert!(!map.contains_key("x-api-key"));
    }

    #[test]
    fn test_openai_headers_use_bearer_ignored() {
        // OpenAI always uses bearer regardless of the flag
        let headers = Provider::OpenAi.auth_headers("sk-key", false);

        let map: std::collections::HashMap<&str, &str> = headers
            .iter()
            .map(|(k, v)| (*k, v.as_str()))
            .collect();

        assert_eq!(map["authorization"], "Bearer sk-key");
    }

    // -----------------------------------------------------------------------
    // URL path tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_anthropic_url_path() {
        assert_eq!(Provider::Anthropic.url_path(), "/v1/messages");
    }

    #[test]
    fn test_openai_url_path() {
        assert_eq!(Provider::OpenAi.url_path(), "/v1/chat/completions");
    }

    // -----------------------------------------------------------------------
    // Error format tests
    // -----------------------------------------------------------------------

}

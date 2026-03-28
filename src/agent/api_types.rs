use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize)]
pub struct MessagesRequest {
    pub model: String,
    pub max_tokens: u32,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDefinition>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct MessagesResponse {
    pub id: String,
    pub content: Vec<ContentBlock>,
    pub stop_reason: String,
    pub usage: Usage,
    // Allow unknown fields for forward compatibility — serde ignores them by default
    // when there is no #[serde(deny_unknown_fields)].
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_serialize_messages_request() {
        let req = MessagesRequest {
            model: "claude-sonnet-4-20250514".to_string(),
            max_tokens: 4096,
            system: Some("You are a helpful assistant.".to_string()),
            messages: vec![Message {
                role: "user".to_string(),
                content: vec![ContentBlock::Text {
                    text: "Hello".to_string(),
                }],
            }],
            tools: None,
        };

        let serialized = serde_json::to_value(&req).unwrap();
        assert_eq!(serialized["model"], "claude-sonnet-4-20250514");
        assert_eq!(serialized["max_tokens"], 4096);
        assert_eq!(serialized["system"], "You are a helpful assistant.");
        assert_eq!(serialized["messages"][0]["role"], "user");
        assert_eq!(serialized["messages"][0]["content"][0]["type"], "text");
        assert_eq!(serialized["messages"][0]["content"][0]["text"], "Hello");
        // tools should be absent when None (skip_serializing_if)
        assert!(serialized.get("tools").is_none());
    }

    #[test]
    fn test_serialize_messages_request_with_tools() {
        let req = MessagesRequest {
            model: "claude-sonnet-4-20250514".to_string(),
            max_tokens: 1024,
            system: None,
            messages: vec![],
            tools: Some(vec![ToolDefinition {
                name: "bash".to_string(),
                description: "Run a bash command".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string" }
                    },
                    "required": ["command"]
                }),
            }]),
        };

        let serialized = serde_json::to_value(&req).unwrap();
        assert_eq!(serialized["tools"][0]["name"], "bash");
        assert!(serialized["tools"][0]["input_schema"]["properties"]["command"].is_object());
    }

    #[test]
    fn test_deserialize_response_text_content() {
        let raw = json!({
            "id": "msg_abc123",
            "content": [
                { "type": "text", "text": "Hello, world!" }
            ],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5
            }
        });

        let resp: MessagesResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.id, "msg_abc123");
        assert_eq!(resp.stop_reason, "end_turn");
        assert_eq!(resp.content.len(), 1);
        match &resp.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "Hello, world!"),
            other => panic!("expected Text, got {:?}", other),
        }
        assert_eq!(resp.usage.input_tokens, 10);
        assert_eq!(resp.usage.output_tokens, 5);
    }

    #[test]
    fn test_deserialize_response_tool_use_content() {
        let raw = json!({
            "id": "msg_tool456",
            "content": [
                {
                    "type": "tool_use",
                    "id": "tu_001",
                    "name": "bash",
                    "input": { "command": "ls -la" }
                }
            ],
            "stop_reason": "tool_use",
            "usage": {
                "input_tokens": 50,
                "output_tokens": 20
            }
        });

        let resp: MessagesResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.stop_reason, "tool_use");
        match &resp.content[0] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "tu_001");
                assert_eq!(name, "bash");
                assert_eq!(input["command"], "ls -la");
            }
            other => panic!("expected ToolUse, got {:?}", other),
        }
    }

    #[test]
    fn test_deserialize_response_mixed_content() {
        let raw = json!({
            "id": "msg_mixed789",
            "content": [
                { "type": "text", "text": "Let me run that command." },
                {
                    "type": "tool_use",
                    "id": "tu_002",
                    "name": "bash",
                    "input": { "command": "echo hello" }
                }
            ],
            "stop_reason": "tool_use",
            "usage": {
                "input_tokens": 100,
                "output_tokens": 40
            }
        });

        let resp: MessagesResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.content.len(), 2);
        assert!(matches!(&resp.content[0], ContentBlock::Text { .. }));
        assert!(matches!(&resp.content[1], ContentBlock::ToolUse { .. }));
    }

    #[test]
    fn test_deserialize_response_with_unknown_fields() {
        // Forward compatibility: unknown fields should be silently ignored
        let raw = json!({
            "id": "msg_compat",
            "content": [
                { "type": "text", "text": "hi" }
            ],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 1,
                "output_tokens": 1
            },
            "model": "claude-sonnet-4-20250514",
            "type": "message",
            "some_future_field": [1, 2, 3]
        });

        let resp: Result<MessagesResponse, _> = serde_json::from_value(raw);
        assert!(resp.is_ok(), "should ignore unknown fields");
        assert_eq!(resp.unwrap().id, "msg_compat");
    }

    #[test]
    fn test_usage_missing_optional_fields_default_to_zero() {
        let raw = json!({
            "input_tokens": 42,
            "output_tokens": 7
        });

        let usage: Usage = serde_json::from_value(raw).unwrap();
        assert_eq!(usage.input_tokens, 42);
        assert_eq!(usage.output_tokens, 7);
        assert_eq!(usage.cache_creation_input_tokens, 0);
        assert_eq!(usage.cache_read_input_tokens, 0);
    }

    #[test]
    fn test_usage_with_all_fields() {
        let raw = json!({
            "input_tokens": 100,
            "output_tokens": 50,
            "cache_creation_input_tokens": 200,
            "cache_read_input_tokens": 80
        });

        let usage: Usage = serde_json::from_value(raw).unwrap();
        assert_eq!(
            usage,
            Usage {
                input_tokens: 100,
                output_tokens: 50,
                cache_creation_input_tokens: 200,
                cache_read_input_tokens: 80,
            }
        );
    }

    #[test]
    fn test_content_block_text_roundtrip() {
        let block = ContentBlock::Text {
            text: "round trip".to_string(),
        };
        let serialized = serde_json::to_value(&block).unwrap();
        assert_eq!(serialized["type"], "text");
        assert_eq!(serialized["text"], "round trip");

        let deserialized: ContentBlock = serde_json::from_value(serialized).unwrap();
        assert_eq!(deserialized, block);
    }

    #[test]
    fn test_content_block_tool_use_roundtrip() {
        let block = ContentBlock::ToolUse {
            id: "tu_rt".to_string(),
            name: "read_file".to_string(),
            input: json!({"path": "/tmp/test.txt"}),
        };
        let serialized = serde_json::to_value(&block).unwrap();
        assert_eq!(serialized["type"], "tool_use");
        assert_eq!(serialized["id"], "tu_rt");
        assert_eq!(serialized["name"], "read_file");
        assert_eq!(serialized["input"]["path"], "/tmp/test.txt");

        let deserialized: ContentBlock = serde_json::from_value(serialized).unwrap();
        assert_eq!(deserialized, block);
    }

    #[test]
    fn test_content_block_tool_result_roundtrip() {
        let block = ContentBlock::ToolResult {
            tool_use_id: "tu_rt".to_string(),
            content: "file contents here".to_string(),
            is_error: Some(false),
        };
        let serialized = serde_json::to_value(&block).unwrap();
        assert_eq!(serialized["type"], "tool_result");
        assert_eq!(serialized["tool_use_id"], "tu_rt");
        assert_eq!(serialized["content"], "file contents here");
        assert_eq!(serialized["is_error"], false);

        let deserialized: ContentBlock = serde_json::from_value(serialized).unwrap();
        assert_eq!(deserialized, block);
    }

    #[test]
    fn test_content_block_tool_result_no_error_field() {
        let block = ContentBlock::ToolResult {
            tool_use_id: "tu_x".to_string(),
            content: "ok".to_string(),
            is_error: None,
        };
        let serialized = serde_json::to_value(&block).unwrap();
        // is_error should be absent when None
        assert!(serialized.get("is_error").is_none());

        let deserialized: ContentBlock = serde_json::from_value(serialized).unwrap();
        assert_eq!(deserialized, block);
    }
}

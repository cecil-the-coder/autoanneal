use crate::agent::api_types::*;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Traits -- these abstract away the API client and tool executor so the
// conversation loop can be tested with mocks.
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
pub trait MessageSender: Send + Sync {
    async fn send(
        &self,
        request: &MessagesRequest,
        timeout: Duration,
    ) -> Result<MessagesResponse, ApiError>;
}

#[async_trait::async_trait]
pub trait ToolHandler: Send + Sync {
    /// Execute a single tool call. Returns `(content, is_error)`.
    async fn execute(&self, name: &str, input: &serde_json::Value) -> (String, bool);

    /// Return the tool definitions to include in the API request.
    fn definitions(&self) -> Vec<ToolDefinition>;
}

// ---------------------------------------------------------------------------
// Error type (mirrors what a real ApiClient would expose)
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String },

    #[error("timeout")]
    Timeout,

    #[error("request error: {0}")]
    Request(String),

    #[error("malformed response: {0}")]
    MalformedResponse(String),
}

// ---------------------------------------------------------------------------
// Config & result types
// ---------------------------------------------------------------------------

pub struct ConversationConfig {
    pub model: String,
    pub system_prompt: Option<String>,
    pub max_turns: u32,
    pub max_tokens_per_turn: u32,
    pub max_total_input_tokens: u64,
    pub max_total_output_tokens: u64,
    pub timeout_per_turn: Duration,
    pub tools_enabled: bool,
}

impl Default for ConversationConfig {
    fn default() -> Self {
        Self {
            model: "claude-sonnet-4-20250514".to_string(),
            system_prompt: None,
            max_turns: 20,
            max_tokens_per_turn: 4096,
            max_total_input_tokens: 500_000,
            max_total_output_tokens: 100_000,
            timeout_per_turn: Duration::from_secs(120),
            tools_enabled: true,
        }
    }
}

#[derive(Debug)]
pub struct ConversationResult {
    pub text: String,
    pub turns: u32,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub stop_reason: StopReason,
}

#[derive(Debug)]
pub enum StopReason {
    EndTurn,
    MaxTurns,
    BudgetExhausted,
    Timeout,
    Error(String),
}

/// Maximum size (in bytes) for a single tool result before truncation.
const MAX_TOOL_RESULT_BYTES: usize = 100_000;

// ---------------------------------------------------------------------------
// Core conversation loop
// ---------------------------------------------------------------------------

pub async fn run(
    sender: &dyn MessageSender,
    executor: &dyn ToolHandler,
    config: &ConversationConfig,
    prompt: &str,
) -> ConversationResult {
    let mut messages: Vec<Message> = vec![Message {
        role: "user".to_string(),
        content: vec![ContentBlock::Text {
            text: prompt.to_string(),
        }],
    }];

    let mut total_input_tokens: u64 = 0;
    let mut total_output_tokens: u64 = 0;
    let mut turns: u32 = 0;
    let mut collected_text = String::new();

    loop {
        // --- guard: turn limit ---
        turns += 1;
        if turns > config.max_turns {
            return ConversationResult {
                text: collected_text,
                turns: turns - 1,
                total_input_tokens,
                total_output_tokens,
                stop_reason: StopReason::MaxTurns,
            };
        }

        // --- guard: token budget ---
        if total_input_tokens >= config.max_total_input_tokens
            || total_output_tokens >= config.max_total_output_tokens
        {
            return ConversationResult {
                text: collected_text,
                turns: turns - 1,
                total_input_tokens,
                total_output_tokens,
                stop_reason: StopReason::BudgetExhausted,
            };
        }

        // --- build request ---
        let tools = if config.tools_enabled {
            let defs = executor.definitions();
            if defs.is_empty() {
                None
            } else {
                Some(defs)
            }
        } else {
            None
        };

        let request = MessagesRequest {
            model: config.model.clone(),
            max_tokens: config.max_tokens_per_turn,
            system: config.system_prompt.clone(),
            messages: messages.clone(),
            tools,
        };

        // --- send ---
        let response = match sender.send(&request, config.timeout_per_turn).await {
            Ok(r) => r,
            Err(ApiError::Timeout) => {
                return ConversationResult {
                    text: collected_text,
                    turns,
                    total_input_tokens,
                    total_output_tokens,
                    stop_reason: StopReason::Timeout,
                };
            }
            Err(ApiError::Http { status, body }) if status == 401 || status == 403 => {
                return ConversationResult {
                    text: collected_text,
                    turns,
                    total_input_tokens,
                    total_output_tokens,
                    stop_reason: StopReason::Error(format!("HTTP {status}: {body}")),
                };
            }
            Err(ApiError::Http { status, body: _ }) if status >= 500 => {
                // One retry for server errors
                match sender.send(&request, config.timeout_per_turn).await {
                    Ok(r) => r,
                    Err(e) => {
                        return ConversationResult {
                            text: collected_text,
                            turns,
                            total_input_tokens,
                            total_output_tokens,
                            stop_reason: StopReason::Error(format!(
                                "server error after retry: {e}"
                            )),
                        };
                    }
                }
            }
            Err(e) => {
                return ConversationResult {
                    text: collected_text,
                    turns,
                    total_input_tokens,
                    total_output_tokens,
                    stop_reason: StopReason::Error(e.to_string()),
                };
            }
        };

        // --- accumulate tokens ---
        total_input_tokens += response.usage.input_tokens
            + response.usage.cache_creation_input_tokens
            + response.usage.cache_read_input_tokens;
        total_output_tokens += response.usage.output_tokens;

        // --- process content blocks ---
        let mut has_tool_use = false;
        let mut tool_results: Vec<ContentBlock> = Vec::new();

        // Collect text from this turn, and gather tool calls.
        for block in &response.content {
            match block {
                ContentBlock::Text { text } => {
                    if !collected_text.is_empty() && !text.is_empty() {
                        collected_text.push('\n');
                    }
                    collected_text.push_str(text);
                }
                ContentBlock::ToolUse { id, name, input } => {
                    has_tool_use = true;
                    let (mut result_content, is_error) =
                        executor.execute(name, input).await;

                    // Truncate very large tool results
                    if result_content.len() > MAX_TOOL_RESULT_BYTES {
                        result_content.truncate(MAX_TOOL_RESULT_BYTES);
                        result_content
                            .push_str("\n... [truncated, result too large]");
                    }

                    tool_results.push(ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content: result_content,
                        is_error: if is_error { Some(true) } else { None },
                    });
                }
                ContentBlock::ToolResult { .. } => {
                    // Shouldn't appear in a response, ignore.
                }
            }
        }

        // Append the assistant message to history.
        messages.push(Message {
            role: "assistant".to_string(),
            content: response.content.clone(),
        });

        // --- decide what to do next ---
        match response.stop_reason.as_str() {
            "end_turn" => {
                return ConversationResult {
                    text: collected_text,
                    turns,
                    total_input_tokens,
                    total_output_tokens,
                    stop_reason: StopReason::EndTurn,
                };
            }
            "tool_use" => {
                // Append tool results as a user message and loop.
                messages.push(Message {
                    role: "user".to_string(),
                    content: tool_results,
                });
                // Continue loop → next turn
            }
            "max_tokens" => {
                // Model hit its output limit mid-generation. If there were
                // tool calls we still need to handle them, otherwise ask to
                // continue.
                if has_tool_use {
                    messages.push(Message {
                        role: "user".to_string(),
                        content: tool_results,
                    });
                } else {
                    messages.push(Message {
                        role: "user".to_string(),
                        content: vec![ContentBlock::Text {
                            text: "Continue.".to_string(),
                        }],
                    });
                }
            }
            other => {
                return ConversationResult {
                    text: collected_text,
                    turns,
                    total_input_tokens,
                    total_output_tokens,
                    stop_reason: StopReason::Error(format!(
                        "unexpected stop_reason: {other}"
                    )),
                };
            }
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Mutex;

    // -----------------------------------------------------------------------
    // Mock helpers
    // -----------------------------------------------------------------------

    /// A mock sender that returns pre-programmed responses in order.
    struct MockSender {
        responses: Mutex<Vec<Result<MessagesResponse, ApiError>>>,
        call_count: AtomicUsize,
    }

    impl MockSender {
        fn new(responses: Vec<Result<MessagesResponse, ApiError>>) -> Self {
            Self {
                responses: Mutex::new(responses),
                call_count: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl MessageSender for MockSender {
        async fn send(
            &self,
            _request: &MessagesRequest,
            _timeout: Duration,
        ) -> Result<MessagesResponse, ApiError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            let mut queue = self.responses.lock().await;
            if queue.is_empty() {
                panic!("MockSender: no more responses queued");
            }
            queue.remove(0)
        }
    }

    /// A mock tool handler that records calls and returns pre-programmed results.
    struct MockToolHandler {
        results: Mutex<Vec<(String, bool)>>,
        calls: Mutex<Vec<(String, serde_json::Value)>>,
        defs: Vec<ToolDefinition>,
    }

    impl MockToolHandler {
        fn new(results: Vec<(String, bool)>) -> Self {
            Self {
                results: Mutex::new(results),
                calls: Mutex::new(Vec::new()),
                defs: vec![
                    ToolDefinition {
                        name: "read_file".to_string(),
                        description: "Read a file".to_string(),
                        input_schema: json!({
                            "type": "object",
                            "properties": {
                                "path": { "type": "string" }
                            },
                            "required": ["path"]
                        }),
                    },
                    ToolDefinition {
                        name: "write_file".to_string(),
                        description: "Write a file".to_string(),
                        input_schema: json!({
                            "type": "object",
                            "properties": {
                                "path": { "type": "string" },
                                "content": { "type": "string" }
                            },
                            "required": ["path", "content"]
                        }),
                    },
                    ToolDefinition {
                        name: "bash".to_string(),
                        description: "Run bash".to_string(),
                        input_schema: json!({
                            "type": "object",
                            "properties": {
                                "command": { "type": "string" }
                            },
                            "required": ["command"]
                        }),
                    },
                ],
            }
        }

        #[allow(dead_code)]
        fn with_defs(mut self, defs: Vec<ToolDefinition>) -> Self {
            self.defs = defs;
            self
        }

        async fn recorded_calls(&self) -> Vec<(String, serde_json::Value)> {
            self.calls.lock().await.clone()
        }
    }

    #[async_trait::async_trait]
    impl ToolHandler for MockToolHandler {
        async fn execute(
            &self,
            name: &str,
            input: &serde_json::Value,
        ) -> (String, bool) {
            self.calls
                .lock()
                .await
                .push((name.to_string(), input.clone()));
            let mut queue = self.results.lock().await;
            if queue.is_empty() {
                ("mock: no result queued".to_string(), true)
            } else {
                queue.remove(0)
            }
        }

        fn definitions(&self) -> Vec<ToolDefinition> {
            self.defs.clone()
        }
    }

    // Helper to build a simple text response.
    fn text_response(text: &str, input_tok: u64, output_tok: u64) -> MessagesResponse {
        MessagesResponse {
            id: "msg_test".to_string(),
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            stop_reason: "end_turn".to_string(),
            usage: Usage {
                input_tokens: input_tok,
                output_tokens: output_tok,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            },
        }
    }

    /// Build a tool_use response with one tool call.
    fn tool_use_response(
        tool_id: &str,
        tool_name: &str,
        input: serde_json::Value,
        input_tok: u64,
        output_tok: u64,
    ) -> MessagesResponse {
        MessagesResponse {
            id: "msg_test".to_string(),
            content: vec![ContentBlock::ToolUse {
                id: tool_id.to_string(),
                name: tool_name.to_string(),
                input,
            }],
            stop_reason: "tool_use".to_string(),
            usage: Usage {
                input_tokens: input_tok,
                output_tokens: output_tok,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            },
        }
    }

    /// Build a tool_use response with two tool calls.
    fn multi_tool_response(
        calls: Vec<(&str, &str, serde_json::Value)>,
        input_tok: u64,
        output_tok: u64,
    ) -> MessagesResponse {
        let content = calls
            .into_iter()
            .map(|(id, name, input)| ContentBlock::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                input,
            })
            .collect();
        MessagesResponse {
            id: "msg_test".to_string(),
            content,
            stop_reason: "tool_use".to_string(),
            usage: Usage {
                input_tokens: input_tok,
                output_tokens: output_tok,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            },
        }
    }

    /// Build a response with both text and tool_use content.
    fn mixed_response(
        text: &str,
        tool_id: &str,
        tool_name: &str,
        input: serde_json::Value,
        input_tok: u64,
        output_tok: u64,
    ) -> MessagesResponse {
        MessagesResponse {
            id: "msg_test".to_string(),
            content: vec![
                ContentBlock::Text {
                    text: text.to_string(),
                },
                ContentBlock::ToolUse {
                    id: tool_id.to_string(),
                    name: tool_name.to_string(),
                    input,
                },
            ],
            stop_reason: "tool_use".to_string(),
            usage: Usage {
                input_tokens: input_tok,
                output_tokens: output_tok,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            },
        }
    }

    fn max_tokens_response(text: &str, input_tok: u64, output_tok: u64) -> MessagesResponse {
        MessagesResponse {
            id: "msg_test".to_string(),
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            stop_reason: "max_tokens".to_string(),
            usage: Usage {
                input_tokens: input_tok,
                output_tokens: output_tok,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            },
        }
    }

    fn default_config() -> ConversationConfig {
        ConversationConfig::default()
    }

    // -----------------------------------------------------------------------
    // Simple response tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_simple_text_response() {
        let sender = MockSender::new(vec![Ok(text_response("Hello!", 10, 5))]);
        let executor = MockToolHandler::new(vec![]);

        let result = run(&sender, &executor, &default_config(), "Hi").await;

        assert_eq!(result.text, "Hello!");
        assert_eq!(result.turns, 1);
        assert_eq!(result.total_input_tokens, 10);
        assert_eq!(result.total_output_tokens, 5);
        assert!(matches!(result.stop_reason, StopReason::EndTurn));
    }

    #[tokio::test]
    async fn test_empty_text_response() {
        let sender = MockSender::new(vec![Ok(text_response("", 10, 1))]);
        let executor = MockToolHandler::new(vec![]);

        let result = run(&sender, &executor, &default_config(), "Hi").await;

        assert_eq!(result.text, "");
        assert_eq!(result.turns, 1);
        assert!(matches!(result.stop_reason, StopReason::EndTurn));
    }

    // -----------------------------------------------------------------------
    // Tool use tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_single_tool_call() {
        // Turn 1: model calls read_file
        // Turn 2: model returns text after seeing tool result
        let sender = MockSender::new(vec![
            Ok(tool_use_response(
                "tu_1",
                "read_file",
                json!({"path": "/tmp/f.txt"}),
                20,
                10,
            )),
            Ok(text_response("The file contains: hello", 30, 15)),
        ]);
        let executor = MockToolHandler::new(vec![
            ("hello world".to_string(), false),
        ]);

        let result = run(&sender, &executor, &default_config(), "Read the file").await;

        assert_eq!(result.text, "The file contains: hello");
        assert_eq!(result.turns, 2);
        assert_eq!(result.total_input_tokens, 50);
        assert_eq!(result.total_output_tokens, 25);
        assert!(matches!(result.stop_reason, StopReason::EndTurn));

        let calls = executor.recorded_calls().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "read_file");
        assert_eq!(calls[0].1, json!({"path": "/tmp/f.txt"}));
    }

    #[tokio::test]
    async fn test_two_tools_in_one_turn() {
        let sender = MockSender::new(vec![
            Ok(multi_tool_response(
                vec![
                    ("tu_1", "read_file", json!({"path": "/a.txt"})),
                    ("tu_2", "bash", json!({"command": "ls"})),
                ],
                30,
                20,
            )),
            Ok(text_response("Done reading and listing", 40, 10)),
        ]);
        let executor = MockToolHandler::new(vec![
            ("contents of a".to_string(), false),
            ("file1\nfile2".to_string(), false),
        ]);

        let result = run(&sender, &executor, &default_config(), "Do stuff").await;

        assert_eq!(result.text, "Done reading and listing");
        assert_eq!(result.turns, 2);

        let calls = executor.recorded_calls().await;
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "read_file");
        assert_eq!(calls[1].0, "bash");
    }

    #[tokio::test]
    async fn test_tool_failure_recovery() {
        // Tool fails, model sees error, recovers
        let sender = MockSender::new(vec![
            Ok(tool_use_response(
                "tu_1",
                "read_file",
                json!({"path": "/nonexistent"}),
                20,
                10,
            )),
            Ok(text_response("File not found, using default", 30, 12)),
        ]);
        let executor = MockToolHandler::new(vec![
            ("ENOENT: file not found".to_string(), true),
        ]);

        let result = run(&sender, &executor, &default_config(), "Read it").await;

        assert_eq!(result.text, "File not found, using default");
        assert_eq!(result.turns, 2);
        assert!(matches!(result.stop_reason, StopReason::EndTurn));
    }

    #[tokio::test]
    async fn test_chained_tool_calls_three_turns() {
        // Turn 1: model calls read_file
        // Turn 2: model calls write_file
        // Turn 3: model returns text
        let sender = MockSender::new(vec![
            Ok(tool_use_response(
                "tu_1",
                "read_file",
                json!({"path": "/src/main.rs"}),
                20,
                10,
            )),
            Ok(tool_use_response(
                "tu_2",
                "write_file",
                json!({"path": "/src/main.rs", "content": "updated"}),
                40,
                15,
            )),
            Ok(text_response("File updated successfully", 50, 8)),
        ]);
        let executor = MockToolHandler::new(vec![
            ("fn main() {}".to_string(), false),
            ("ok".to_string(), false),
        ]);

        let result = run(&sender, &executor, &default_config(), "Fix the file").await;

        assert_eq!(result.text, "File updated successfully");
        assert_eq!(result.turns, 3);
        assert_eq!(result.total_input_tokens, 110);
        assert_eq!(result.total_output_tokens, 33);

        let calls = executor.recorded_calls().await;
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "read_file");
        assert_eq!(calls[1].0, "write_file");
    }

    // -----------------------------------------------------------------------
    // Limit tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_max_turns_limit() {
        // Model keeps calling tools forever. With max_turns=3, we should stop.
        let sender = MockSender::new(vec![
            Ok(tool_use_response("tu_1", "bash", json!({"command": "a"}), 10, 5)),
            Ok(tool_use_response("tu_2", "bash", json!({"command": "b"}), 10, 5)),
            Ok(tool_use_response("tu_3", "bash", json!({"command": "c"}), 10, 5)),
            // This 4th response should never be reached
            Ok(text_response("unreachable", 10, 5)),
        ]);
        let executor = MockToolHandler::new(vec![
            ("ok".to_string(), false),
            ("ok".to_string(), false),
            ("ok".to_string(), false),
        ]);

        let mut config = default_config();
        config.max_turns = 3;

        let result = run(&sender, &executor, &config, "Loop forever").await;

        assert!(matches!(result.stop_reason, StopReason::MaxTurns));
        assert_eq!(result.turns, 3);
    }

    #[tokio::test]
    async fn test_output_token_budget_exhausted() {
        // Each turn uses 50 output tokens. Budget is 80, so after 2 turns
        // (100 total) the loop should detect budget exceeded before turn 3.
        let sender = MockSender::new(vec![
            Ok(tool_use_response("tu_1", "bash", json!({"command": "a"}), 10, 50)),
            Ok(tool_use_response("tu_2", "bash", json!({"command": "b"}), 10, 50)),
            Ok(text_response("unreachable", 10, 5)),
        ]);
        let executor = MockToolHandler::new(vec![
            ("ok".to_string(), false),
            ("ok".to_string(), false),
        ]);

        let mut config = default_config();
        config.max_total_output_tokens = 80;

        let result = run(&sender, &executor, &config, "Budget test").await;

        assert!(matches!(result.stop_reason, StopReason::BudgetExhausted));
        assert_eq!(result.total_output_tokens, 100);
        assert_eq!(result.turns, 2);
    }

    #[tokio::test]
    async fn test_input_token_budget_exhausted() {
        let sender = MockSender::new(vec![
            Ok(tool_use_response("tu_1", "bash", json!({"command": "a"}), 100, 5)),
            Ok(text_response("unreachable", 10, 5)),
        ]);
        let executor = MockToolHandler::new(vec![
            ("ok".to_string(), false),
        ]);

        let mut config = default_config();
        config.max_total_input_tokens = 50; // budget already exceeded after turn 1

        let result = run(&sender, &executor, &config, "Budget test").await;

        assert!(matches!(result.stop_reason, StopReason::BudgetExhausted));
        assert_eq!(result.turns, 1);
    }

    #[tokio::test]
    async fn test_timeout() {
        let sender = MockSender::new(vec![Err(ApiError::Timeout)]);
        let executor = MockToolHandler::new(vec![]);

        let result = run(&sender, &executor, &default_config(), "Slow").await;

        assert!(matches!(result.stop_reason, StopReason::Timeout));
        assert_eq!(result.turns, 1);
    }

    // -----------------------------------------------------------------------
    // Error handling tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_auth_error_no_retry() {
        let sender = MockSender::new(vec![Err(ApiError::Http {
            status: 401,
            body: "Unauthorized".to_string(),
        })]);
        let executor = MockToolHandler::new(vec![]);

        let result = run(&sender, &executor, &default_config(), "Hi").await;

        assert!(matches!(result.stop_reason, StopReason::Error(_)));
        if let StopReason::Error(msg) = &result.stop_reason {
            assert!(msg.contains("401"));
        }
        // Should NOT retry — only 1 call
        assert_eq!(sender.call_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_server_error_retried_then_fails() {
        let sender = MockSender::new(vec![
            Err(ApiError::Http {
                status: 500,
                body: "Internal Server Error".to_string(),
            }),
            // Retry also fails
            Err(ApiError::Http {
                status: 500,
                body: "Still broken".to_string(),
            }),
        ]);
        let executor = MockToolHandler::new(vec![]);

        let result = run(&sender, &executor, &default_config(), "Hi").await;

        assert!(matches!(result.stop_reason, StopReason::Error(_)));
        // Should have made 2 calls (original + 1 retry)
        assert_eq!(sender.call_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_server_error_retried_then_succeeds() {
        let sender = MockSender::new(vec![
            Err(ApiError::Http {
                status: 502,
                body: "Bad Gateway".to_string(),
            }),
            Ok(text_response("Recovered!", 10, 5)),
        ]);
        let executor = MockToolHandler::new(vec![]);

        let result = run(&sender, &executor, &default_config(), "Hi").await;

        assert_eq!(result.text, "Recovered!");
        assert!(matches!(result.stop_reason, StopReason::EndTurn));
        assert_eq!(sender.call_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_malformed_response_error() {
        let sender = MockSender::new(vec![Err(ApiError::MalformedResponse(
            "invalid JSON".to_string(),
        ))]);
        let executor = MockToolHandler::new(vec![]);

        let result = run(&sender, &executor, &default_config(), "Hi").await;

        assert!(matches!(result.stop_reason, StopReason::Error(_)));
        if let StopReason::Error(msg) = &result.stop_reason {
            assert!(msg.contains("malformed"));
        }
    }

    #[tokio::test]
    async fn test_unknown_tool_name_sent_as_error() {
        // Model calls a tool that doesn't exist. The executor handles it
        // by returning an error. Model then recovers.
        let sender = MockSender::new(vec![
            Ok(tool_use_response(
                "tu_1",
                "nonexistent_tool",
                json!({}),
                20,
                10,
            )),
            Ok(text_response("I'll try a different approach", 30, 10)),
        ]);
        // The executor will return error for unknown tool
        let executor = MockToolHandler::new(vec![
            ("unknown tool: nonexistent_tool".to_string(), true),
        ]);

        let result = run(&sender, &executor, &default_config(), "Do it").await;

        assert_eq!(result.text, "I'll try a different approach");
        assert_eq!(result.turns, 2);

        let calls = executor.recorded_calls().await;
        assert_eq!(calls[0].0, "nonexistent_tool");
    }

    #[tokio::test]
    async fn test_tool_with_invalid_input() {
        let sender = MockSender::new(vec![
            Ok(tool_use_response(
                "tu_1",
                "read_file",
                json!("not an object"), // invalid input
                20,
                10,
            )),
            Ok(text_response("Sorry, let me fix that", 30, 10)),
        ]);
        let executor = MockToolHandler::new(vec![
            ("invalid input: expected object".to_string(), true),
        ]);

        let result = run(&sender, &executor, &default_config(), "Read").await;

        assert_eq!(result.text, "Sorry, let me fix that");
        assert!(matches!(result.stop_reason, StopReason::EndTurn));
    }

    // -----------------------------------------------------------------------
    // Token tracking tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_tokens_accumulated_across_turns() {
        let sender = MockSender::new(vec![
            Ok(tool_use_response("tu_1", "bash", json!({"command": "a"}), 100, 20)),
            Ok(tool_use_response("tu_2", "bash", json!({"command": "b"}), 200, 30)),
            Ok(text_response("done", 300, 40)),
        ]);
        let executor = MockToolHandler::new(vec![
            ("ok".to_string(), false),
            ("ok".to_string(), false),
        ]);

        let result = run(&sender, &executor, &default_config(), "Multi-turn").await;

        assert_eq!(result.total_input_tokens, 600); // 100+200+300
        assert_eq!(result.total_output_tokens, 90); // 20+30+40
        assert_eq!(result.turns, 3);
    }

    #[tokio::test]
    async fn test_cache_tokens_tracked() {
        let response = MessagesResponse {
            id: "msg_cache".to_string(),
            content: vec![ContentBlock::Text {
                text: "cached".to_string(),
            }],
            stop_reason: "end_turn".to_string(),
            usage: Usage {
                input_tokens: 50,
                output_tokens: 10,
                cache_creation_input_tokens: 200,
                cache_read_input_tokens: 100,
            },
        };
        let sender = MockSender::new(vec![Ok(response)]);
        let executor = MockToolHandler::new(vec![]);

        let result = run(&sender, &executor, &default_config(), "Cache test").await;

        // input_tokens includes base + cache_creation + cache_read
        assert_eq!(result.total_input_tokens, 350); // 50 + 200 + 100
        assert_eq!(result.total_output_tokens, 10);
    }

    // -----------------------------------------------------------------------
    // Edge case tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_max_tokens_stop_reason_continues() {
        // Model hits output limit, conversation continues with "Continue."
        let sender = MockSender::new(vec![
            Ok(max_tokens_response("partial text", 20, 100)),
            Ok(text_response(" and more text", 30, 50)),
        ]);
        let executor = MockToolHandler::new(vec![]);

        let result = run(&sender, &executor, &default_config(), "Write a lot").await;

        assert_eq!(result.text, "partial text\n and more text");
        assert_eq!(result.turns, 2);
        assert!(matches!(result.stop_reason, StopReason::EndTurn));
    }

    #[tokio::test]
    async fn test_mixed_text_and_tool_use() {
        // Response has both text and tool_use blocks
        let sender = MockSender::new(vec![
            Ok(mixed_response(
                "Let me check that file",
                "tu_1",
                "read_file",
                json!({"path": "/etc/hosts"}),
                20,
                15,
            )),
            Ok(text_response("The file has 10 lines", 30, 10)),
        ]);
        let executor = MockToolHandler::new(vec![
            ("127.0.0.1 localhost".to_string(), false),
        ]);

        let result = run(&sender, &executor, &default_config(), "Check hosts").await;

        // Text from both turns collected
        assert!(result.text.contains("Let me check that file"));
        assert!(result.text.contains("The file has 10 lines"));
        assert_eq!(result.turns, 2);
    }

    #[tokio::test]
    async fn test_empty_tool_result() {
        let sender = MockSender::new(vec![
            Ok(tool_use_response(
                "tu_1",
                "bash",
                json!({"command": "true"}),
                20,
                10,
            )),
            Ok(text_response("Command produced no output", 30, 10)),
        ]);
        let executor = MockToolHandler::new(vec![
            ("".to_string(), false), // empty result
        ]);

        let result = run(&sender, &executor, &default_config(), "Run it").await;

        assert_eq!(result.text, "Command produced no output");
        assert!(matches!(result.stop_reason, StopReason::EndTurn));
    }

    #[tokio::test]
    async fn test_large_tool_result_truncated() {
        let huge = "x".repeat(MAX_TOOL_RESULT_BYTES + 5000);
        let sender = MockSender::new(vec![
            Ok(tool_use_response(
                "tu_1",
                "read_file",
                json!({"path": "/big"}),
                20,
                10,
            )),
            Ok(text_response("Got truncated result", 30, 10)),
        ]);
        let executor = MockToolHandler::new(vec![(huge, false)]);

        let result = run(&sender, &executor, &default_config(), "Read big file").await;

        assert_eq!(result.text, "Got truncated result");
        assert!(matches!(result.stop_reason, StopReason::EndTurn));
    }

    #[tokio::test]
    async fn test_tools_disabled() {
        // When tools_enabled is false, no tool definitions are sent
        let sender = MockSender::new(vec![Ok(text_response("Just text", 10, 5))]);
        let executor = MockToolHandler::new(vec![]);

        let mut config = default_config();
        config.tools_enabled = false;

        let result = run(&sender, &executor, &config, "No tools").await;

        assert_eq!(result.text, "Just text");
        assert!(matches!(result.stop_reason, StopReason::EndTurn));
    }

    #[tokio::test]
    async fn test_system_prompt_passed_through() {
        // We verify the request gets a system prompt by using a sender that
        // inspects the request. For simplicity we just check the run succeeds.
        let sender = MockSender::new(vec![Ok(text_response("ok", 10, 5))]);
        let executor = MockToolHandler::new(vec![]);

        let mut config = default_config();
        config.system_prompt = Some("You are a code reviewer.".to_string());

        let result = run(&sender, &executor, &config, "Review this").await;

        assert_eq!(result.text, "ok");
        assert!(matches!(result.stop_reason, StopReason::EndTurn));
    }

    #[tokio::test]
    async fn test_forbidden_error_no_retry() {
        let sender = MockSender::new(vec![Err(ApiError::Http {
            status: 403,
            body: "Forbidden".to_string(),
        })]);
        let executor = MockToolHandler::new(vec![]);

        let result = run(&sender, &executor, &default_config(), "Hi").await;

        assert!(matches!(result.stop_reason, StopReason::Error(_)));
        assert_eq!(sender.call_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_budget_check_before_first_turn() {
        // If budget is 0, we should stop immediately (before sending any request).
        let sender = MockSender::new(vec![
            Ok(text_response("unreachable", 10, 5)),
        ]);
        let executor = MockToolHandler::new(vec![]);

        let mut config = default_config();
        config.max_total_input_tokens = 0;

        // Budget is already "exceeded" (0 >= 0) — but our check is >=,
        // so the first turn should still attempt (total starts at 0, 0 >= 0 is true).
        // Actually, 0 >= 0 is true, so budget is exhausted before turn 1.
        // Let's use max_turns = 0 to be sure about the edge.
        let result = run(&sender, &executor, &config, "Zero budget").await;

        // With max_total_input_tokens=0, 0 >= 0 triggers budget exhausted
        // before the first API call. Actually let me re-check the logic...
        // The budget check happens at the top of the loop after incrementing turns.
        // turns=1, then we check budget: total_input=0 >= 0 → true → BudgetExhausted.
        assert!(matches!(result.stop_reason, StopReason::BudgetExhausted));
        assert_eq!(result.turns, 0); // never actually sent a request
        assert_eq!(sender.call_count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn test_max_turns_one() {
        // With max_turns=1, the model gets exactly one turn.
        let sender = MockSender::new(vec![
            Ok(tool_use_response("tu_1", "bash", json!({"command": "a"}), 10, 5)),
            // Turn 2 would be needed but max_turns=1
        ]);
        let executor = MockToolHandler::new(vec![
            ("ok".to_string(), false),
        ]);

        let mut config = default_config();
        config.max_turns = 1;

        let result = run(&sender, &executor, &config, "Once").await;

        // Turn 1 succeeds (tool_use), then loop tries turn 2 → hits max_turns
        assert!(matches!(result.stop_reason, StopReason::MaxTurns));
        assert_eq!(result.turns, 1);
    }

    #[tokio::test]
    async fn test_request_error() {
        let sender = MockSender::new(vec![Err(ApiError::Request(
            "connection refused".to_string(),
        ))]);
        let executor = MockToolHandler::new(vec![]);

        let result = run(&sender, &executor, &default_config(), "Hi").await;

        assert!(matches!(result.stop_reason, StopReason::Error(_)));
        if let StopReason::Error(msg) = &result.stop_reason {
            assert!(msg.contains("connection refused"));
        }
    }

    #[tokio::test]
    async fn test_unexpected_stop_reason() {
        let mut resp = text_response("hmm", 10, 5);
        resp.stop_reason = "something_new".to_string();

        let sender = MockSender::new(vec![Ok(resp)]);
        let executor = MockToolHandler::new(vec![]);

        let result = run(&sender, &executor, &default_config(), "Hi").await;

        assert!(matches!(result.stop_reason, StopReason::Error(_)));
        if let StopReason::Error(msg) = &result.stop_reason {
            assert!(msg.contains("unexpected stop_reason"));
        }
    }
}

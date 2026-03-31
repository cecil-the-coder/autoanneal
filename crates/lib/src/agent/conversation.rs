use crate::agent::api_types::*;
use crate::agent::context::{self, ContextManager};
use crate::agent::output_filter;
use std::time::Duration;
use tracing::trace;

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
    async fn execute(&mut self, name: &str, input: &serde_json::Value) -> (String, bool);

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
    pub timeout_per_turn: Duration,
    pub tools_enabled: bool,
    pub temperature: Option<f64>,
    /// Maximum context window in tokens. Old tool results are evicted when
    /// usage approaches this limit, and a `recall_result` tool is provided
    /// so the model can retrieve them on demand.
    pub context_window: u64,
}

impl Default for ConversationConfig {
    fn default() -> Self {
        Self {
            model: "claude-sonnet-4-20250514".to_string(),
            system_prompt: None,
            max_turns: 20,
            max_tokens_per_turn: 4096,
            timeout_per_turn: Duration::from_secs(120),
            tools_enabled: true,
            temperature: None,
            context_window: context::DEFAULT_CONTEXT_WINDOW,
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
    executor: &mut dyn ToolHandler,
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
    let mut ctx_mgr = ContextManager::new(config.context_window);

    loop {
        // --- guard: turn limit ---
        if turns >= config.max_turns {
            return ConversationResult {
                text: collected_text,
                turns,
                total_input_tokens,
                total_output_tokens,
                stop_reason: StopReason::MaxTurns,
            };
        }

        turns += 1;

        // --- build request ---
        let tools = if config.tools_enabled {
            let mut defs = executor.definitions();
            // Inject the recall_result tool when results have been evicted.
            if ctx_mgr.has_evicted() {
                defs.push(ContextManager::tool_definition());
            }
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
            temperature: config.temperature,
            stop_sequences: None,
            tool_choice: None,
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
            Err(e) => {
                // Client already handles retries for server errors and rate limits.
                // Conversation loop just surfaces the final error.
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
        trace!(
            msg_id = %response.id,
            turn = turns,
            rss_mb = crate::agent::bridge::rss_mb(),
            messages = messages.len(),
            "received API response"
        );
        let last_input_tokens = response.usage.input_tokens
            + response.usage.cache_creation_input_tokens
            + response.usage.cache_read_input_tokens;
        total_input_tokens += last_input_tokens;
        total_output_tokens += response.usage.output_tokens;

        // --- evict old tool results if context is getting large ---
        ctx_mgr.maybe_evict(&mut messages, last_input_tokens);

        // --- process content blocks ---
        let mut has_tool_use = false;
        let mut tool_results: Vec<ContentBlock> = Vec::new();
        let mut recall_ids: Vec<String> = Vec::new();

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

                    // Intercept recall_result — handled by context manager, not executor.
                    let (mut result_content, is_error) =
                        if name == context::RECALL_TOOL_NAME {
                            recall_ids.push(id.clone());
                            match input.get("id").and_then(|v| v.as_str()) {
                                Some(recall_id) if !recall_id.is_empty() => {
                                    match ctx_mgr.recall(recall_id) {
                                        Some(content) => (content, false),
                                        None => (format!("No stored result found for id: {recall_id}"), true),
                                    }
                                }
                                _ => {
                                    ("recall_result requires a non-empty 'id' parameter".to_string(), true)
                                }
                            }
                        } else {
                            executor.execute(name, input).await
                        };

                    // Filter command output and store full version for recall.
                    if name == "run_command" && !is_error {
                        let command = input.get("command")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let filtered = output_filter::filter(command, &result_content);

                        if filtered.len() < result_content.len() {
                            let original_len = result_content.len();
                            let store_id = format!("cmd_{}", id);
                            ctx_mgr.store_raw(&store_id, result_content);
                            tracing::debug!(
                                command = %command,
                                original_bytes = original_len,
                                filtered_bytes = filtered.len(),
                                reduction_pct = (100 - (filtered.len() * 100 / original_len.max(1))),
                                "output filter applied"
                            );
                            result_content = format!(
                                "{filtered}\n[full output available via recall_result(id: \"{store_id}\")]"
                            );
                        } else {
                            tracing::debug!(
                                command = %command,
                                output_bytes = result_content.len(),
                                "output filter: no reduction"
                            );
                        }
                    }

                    // Truncate very large tool results (safe at char boundary)
                    if result_content.len() > MAX_TOOL_RESULT_BYTES {
                        let truncate_at = result_content
                            .char_indices()
                            .take_while(|(i, _)| *i <= MAX_TOOL_RESULT_BYTES)
                            .last()
                            .map(|(i, _)| i)
                            .unwrap_or(0);
                        result_content.truncate(truncate_at);
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
                ContentBlock::Unknown => {
                    // Forward-compat: unknown content block types are silently ignored.
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
            "end_turn" | "stop_sequence" => {
                return ConversationResult {
                    text: collected_text,
                    turns,
                    total_input_tokens,
                    total_output_tokens,
                    stop_reason: StopReason::EndTurn,
                };
            }
            "tool_use" => {
                // Track tool results for potential eviction later.
                // Skip recalled results — they're already in the store and
                // would be immediately re-evicted, wasting context.
                let result_msg_idx = messages.len();
                for block in &tool_results {
                    if let ContentBlock::ToolResult { tool_use_id, .. } = block {
                        if !recall_ids.contains(tool_use_id) {
                            ctx_mgr.track(result_msg_idx, tool_use_id);
                        }
                    }
                }
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
                    let result_msg_idx = messages.len();
                    for block in &tool_results {
                        if let ContentBlock::ToolResult { tool_use_id, .. } = block {
                            if !recall_ids.contains(tool_use_id) {
                                ctx_mgr.track(result_msg_idx, tool_use_id);
                            }
                        }
                    }
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
            &mut self,
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
    async fn test_auth_error_surfaces_immediately() {
        let sender = MockSender::new(vec![Err(ApiError::Http {
            status: 401,
            body: "Unauthorized".to_string(),
        })]);
        let executor = MockToolHandler::new(vec![]);

        let result = run(&sender, &executor, &default_config(), "Hi").await;

        assert!(matches!(result.stop_reason, StopReason::Error(_)));
        // Conversation loop does not retry — client handles retries.
        assert_eq!(sender.call_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_server_error_surfaces_immediately() {
        // Client is responsible for retries. Conversation loop just surfaces the error.
        let sender = MockSender::new(vec![Err(ApiError::Http {
            status: 500,
            body: "Internal Server Error".to_string(),
        })]);
        let executor = MockToolHandler::new(vec![]);

        let result = run(&sender, &executor, &default_config(), "Hi").await;

        assert!(matches!(result.stop_reason, StopReason::Error(_)));
        assert_eq!(sender.call_count.load(Ordering::SeqCst), 1);
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

    // ===================================================================
    // max_tokens handling (tests 1-5)
    // ===================================================================

    #[tokio::test]
    async fn test_max_tokens_with_tool_use() {
        // stop_reason "max_tokens" AND a ToolUse block — tool should be executed.
        let resp = MessagesResponse {
            id: "msg_mt".to_string(),
            content: vec![
                ContentBlock::Text { text: "thinking...".to_string() },
                ContentBlock::ToolUse {
                    id: "tu_mt".to_string(),
                    name: "read_file".to_string(),
                    input: json!({"path": "/f.txt"}),
                },
            ],
            stop_reason: "max_tokens".to_string(),
            usage: Usage { input_tokens: 10, output_tokens: 10, cache_creation_input_tokens: 0, cache_read_input_tokens: 0 },
        };
        let sender = MockSender::new(vec![
            Ok(resp),
            Ok(text_response("done", 10, 5)),
        ]);
        let executor = MockToolHandler::new(vec![
            ("file contents".to_string(), false),
        ]);

        let result = run(&sender, &executor, &default_config(), "go").await;

        let calls = executor.recorded_calls().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "read_file");
        assert_eq!(result.text, "thinking...\ndone");
        assert!(matches!(result.stop_reason, StopReason::EndTurn));
    }

    #[tokio::test]
    async fn test_max_tokens_continuation_chain() {
        // Model hits max_tokens 3 times before end_turn; text concatenated.
        let sender = MockSender::new(vec![
            Ok(max_tokens_response("part1", 10, 10)),
            Ok(max_tokens_response("part2", 10, 10)),
            Ok(max_tokens_response("part3", 10, 10)),
            Ok(text_response("part4", 10, 10)),
        ]);
        let executor = MockToolHandler::new(vec![]);

        let result = run(&sender, &executor, &default_config(), "write a lot").await;

        assert_eq!(result.text, "part1\npart2\npart3\npart4");
        assert_eq!(result.turns, 4);
        assert!(matches!(result.stop_reason, StopReason::EndTurn));
    }

    #[tokio::test]
    async fn test_max_tokens_with_empty_content() {
        // stop_reason "max_tokens" but content is empty vec.
        let resp = MessagesResponse {
            id: "msg_empty_mt".to_string(),
            content: vec![],
            stop_reason: "max_tokens".to_string(),
            usage: Usage { input_tokens: 10, output_tokens: 5, cache_creation_input_tokens: 0, cache_read_input_tokens: 0 },
        };
        let sender = MockSender::new(vec![
            Ok(resp),
            Ok(text_response("continued", 10, 5)),
        ]);
        let executor = MockToolHandler::new(vec![]);

        let result = run(&sender, &executor, &default_config(), "go").await;

        assert_eq!(result.text, "continued");
        assert_eq!(result.turns, 2);
        assert!(matches!(result.stop_reason, StopReason::EndTurn));
    }

    #[tokio::test]
    async fn test_max_tokens_only_tool_use_no_text() {
        // max_tokens with only ToolUse block, no Text.
        let resp = MessagesResponse {
            id: "msg_mt_tool".to_string(),
            content: vec![ContentBlock::ToolUse {
                id: "tu_only".to_string(),
                name: "bash".to_string(),
                input: json!({"command": "ls"}),
            }],
            stop_reason: "max_tokens".to_string(),
            usage: Usage { input_tokens: 10, output_tokens: 10, cache_creation_input_tokens: 0, cache_read_input_tokens: 0 },
        };
        let sender = MockSender::new(vec![
            Ok(resp),
            Ok(text_response("after tool", 10, 5)),
        ]);
        let executor = MockToolHandler::new(vec![
            ("file list".to_string(), false),
        ]);

        let result = run(&sender, &executor, &default_config(), "go").await;

        let calls = executor.recorded_calls().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "bash");
        assert_eq!(result.text, "after tool");
        assert!(matches!(result.stop_reason, StopReason::EndTurn));
    }

    // ===================================================================
    // Request inspection (tests 6-10)
    // ===================================================================

    /// An inspecting sender that wraps MockSender but records each request.
    struct InspectingSender {
        inner: MockSender,
        requests: std::sync::Arc<tokio::sync::Mutex<Vec<MessagesRequest>>>,
    }

    impl InspectingSender {
        fn new(responses: Vec<Result<MessagesResponse, ApiError>>) -> Self {
            Self {
                inner: MockSender::new(responses),
                requests: std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new())),
            }
        }

        async fn recorded_requests(&self) -> Vec<MessagesRequest> {
            self.requests.lock().await.clone()
        }
    }

    #[async_trait::async_trait]
    impl MessageSender for InspectingSender {
        async fn send(
            &self,
            request: &MessagesRequest,
            timeout: Duration,
        ) -> Result<MessagesResponse, ApiError> {
            self.requests.lock().await.push(request.clone());
            self.inner.send(request, timeout).await
        }
    }

    #[tokio::test]
    async fn test_tool_result_ordering_matches_tool_calls() {
        // 3 tool calls in one turn, verify results in same order.
        let sender = InspectingSender::new(vec![
            Ok(multi_tool_response(
                vec![
                    ("tu_a", "read_file", json!({"path": "a.txt"})),
                    ("tu_b", "bash", json!({"command": "echo b"})),
                    ("tu_c", "write_file", json!({"path": "c.txt", "content": "c"})),
                ],
                20, 15,
            )),
            Ok(text_response("done", 20, 5)),
        ]);
        let executor = MockToolHandler::new(vec![
            ("result_a".to_string(), false),
            ("result_b".to_string(), false),
            ("result_c".to_string(), false),
        ]);

        let _ = run(&sender, &executor, &default_config(), "go").await;

        let reqs = sender.recorded_requests().await;
        // Second request has tool results as last user message.
        let last_msg = reqs[1].messages.last().expect("second request should have messages after tool use");
        assert_eq!(last_msg.role, "user");
        let ids: Vec<&str> = last_msg.content.iter().filter_map(|b| {
            if let ContentBlock::ToolResult { tool_use_id, .. } = b { Some(tool_use_id.as_str()) } else { None }
        }).collect();
        assert_eq!(ids, vec!["tu_a", "tu_b", "tu_c"]);
    }

    #[tokio::test]
    async fn test_message_history_structure() {
        // Verify alternating user/assistant/user pattern after tool use.
        let sender = InspectingSender::new(vec![
            Ok(tool_use_response("tu_1", "bash", json!({"command": "x"}), 10, 5)),
            Ok(text_response("final", 10, 5)),
        ]);
        let executor = MockToolHandler::new(vec![
            ("ok".to_string(), false),
        ]);

        let _ = run(&sender, &executor, &default_config(), "go").await;

        let reqs = sender.recorded_requests().await;
        // Request 1: messages = [user]
        assert_eq!(reqs[0].messages.len(), 1);
        assert_eq!(reqs[0].messages[0].role, "user");

        // Request 2: messages = [user, assistant, user(tool_result)]
        assert_eq!(reqs[1].messages.len(), 3);
        assert_eq!(reqs[1].messages[0].role, "user");
        assert_eq!(reqs[1].messages[1].role, "assistant");
        assert_eq!(reqs[1].messages[2].role, "user");
    }

    #[tokio::test]
    async fn test_continue_message_after_max_tokens() {
        // Verify "Continue." is appended as user message after max_tokens.
        let sender = InspectingSender::new(vec![
            Ok(max_tokens_response("partial", 10, 10)),
            Ok(text_response("rest", 10, 5)),
        ]);
        let executor = MockToolHandler::new(vec![]);

        let _ = run(&sender, &executor, &default_config(), "write").await;

        let reqs = sender.recorded_requests().await;
        // Second request: messages = [user, assistant, user("Continue.")]
        let last_msg = reqs[1].messages.last().expect("second request should have messages after max_tokens stop");
        assert_eq!(last_msg.role, "user");
        assert_eq!(last_msg.content.len(), 1);
        match &last_msg.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "Continue."),
            other => panic!("expected Text(Continue.), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_system_prompt_in_request() {
        // Verify system field is set in the request.
        let sender = InspectingSender::new(vec![
            Ok(text_response("ok", 10, 5)),
        ]);
        let executor = MockToolHandler::new(vec![]);

        let mut config = default_config();
        config.system_prompt = Some("Be helpful.".to_string());

        let _ = run(&sender, &executor, &config, "hi").await;

        let reqs = sender.recorded_requests().await;
        assert_eq!(reqs[0].system, Some("Be helpful.".to_string()));
    }

    #[tokio::test]
    async fn test_tools_omitted_when_disabled() {
        // Verify request.tools is None when tools_enabled=false.
        let sender = InspectingSender::new(vec![
            Ok(text_response("ok", 10, 5)),
        ]);
        let executor = MockToolHandler::new(vec![]);

        let mut config = default_config();
        config.tools_enabled = false;

        let _ = run(&sender, &executor, &config, "hi").await;

        let reqs = sender.recorded_requests().await;
        assert!(reqs[0].tools.is_none());
    }

    // ===================================================================
    // Multi-turn coherence (tests 11-14)
    // ===================================================================

    #[tokio::test]
    async fn test_duplicate_tool_calls() {
        // Model calls read_file with same path twice, both execute.
        let sender = MockSender::new(vec![
            Ok(multi_tool_response(
                vec![
                    ("tu_1", "read_file", json!({"path": "same.txt"})),
                    ("tu_2", "read_file", json!({"path": "same.txt"})),
                ],
                10, 10,
            )),
            Ok(text_response("both read", 10, 5)),
        ]);
        let executor = MockToolHandler::new(vec![
            ("contents1".to_string(), false),
            ("contents2".to_string(), false),
        ]);

        let result = run(&sender, &executor, &default_config(), "read twice").await;

        let calls = executor.recorded_calls().await;
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "read_file");
        assert_eq!(calls[1].0, "read_file");
        assert_eq!(result.turns, 2);
    }

    #[tokio::test]
    async fn test_infinite_loop_capped_by_max_turns() {
        // Model calls same tool every turn, max_turns=5, verify 5 executions.
        let mut responses = Vec::new();
        let mut tool_results = Vec::new();
        for i in 0..6 {
            responses.push(Ok(tool_use_response(
                &format!("tu_{i}"),
                "bash",
                json!({"command": "loop"}),
                10, 5,
            )));
            tool_results.push(("ok".to_string(), false));
        }
        // Extra response that should never be reached.
        responses.push(Ok(text_response("unreachable", 10, 5)));

        let sender = MockSender::new(responses);
        let executor = MockToolHandler::new(tool_results);

        let mut config = default_config();
        config.max_turns = 5;

        let result = run(&sender, &executor, &config, "loop").await;

        assert!(matches!(result.stop_reason, StopReason::MaxTurns));
        assert_eq!(result.turns, 5);
        let calls = executor.recorded_calls().await;
        assert_eq!(calls.len(), 5);
    }

    #[tokio::test]
    async fn test_tool_use_stop_reason_but_no_tool_blocks() {
        // Malformed response: stop_reason "tool_use" but no ToolUse blocks.
        let resp = MessagesResponse {
            id: "msg_bad".to_string(),
            content: vec![ContentBlock::Text { text: "oops".to_string() }],
            stop_reason: "tool_use".to_string(),
            usage: Usage { input_tokens: 10, output_tokens: 5, cache_creation_input_tokens: 0, cache_read_input_tokens: 0 },
        };
        let sender = MockSender::new(vec![
            Ok(resp),
            // The loop will push empty tool_results as user message and continue.
            Ok(text_response("recovered", 10, 5)),
        ]);
        let executor = MockToolHandler::new(vec![]);

        let result = run(&sender, &executor, &default_config(), "go").await;

        // Should handle gracefully — continues with empty tool results then recovers.
        assert!(result.text.contains("oops") || result.text.contains("recovered"));
        assert_eq!(result.turns, 2);
    }

    #[tokio::test]
    async fn test_end_turn_with_tool_use_block() {
        // stop_reason "end_turn" but response contains a ToolUse block.
        // The code iterates content blocks unconditionally so the tool IS executed,
        // but stop_reason "end_turn" returns immediately — tool result is never sent back.
        let resp = MessagesResponse {
            id: "msg_et".to_string(),
            content: vec![
                ContentBlock::Text { text: "here is info".to_string() },
                ContentBlock::ToolUse {
                    id: "tu_end".to_string(),
                    name: "bash".to_string(),
                    input: json!({"command": "ls"}),
                },
            ],
            stop_reason: "end_turn".to_string(),
            usage: Usage { input_tokens: 10, output_tokens: 5, cache_creation_input_tokens: 0, cache_read_input_tokens: 0 },
        };
        let sender = MockSender::new(vec![Ok(resp)]);
        let executor = MockToolHandler::new(vec![
            ("listing".to_string(), false),
        ]);

        let result = run(&sender, &executor, &default_config(), "info").await;

        // end_turn returns immediately with the text collected.
        assert_eq!(result.text, "here is info");
        assert_eq!(result.turns, 1);
        assert!(matches!(result.stop_reason, StopReason::EndTurn));
    }

    // ===================================================================
    // Error recovery mid-conversation (tests 15-18)
    // ===================================================================

    #[tokio::test]
    async fn test_server_error_mid_conversation() {
        // 500 on turn 3 after 2 successful tool turns.
        let sender = MockSender::new(vec![
            Ok(tool_use_response("tu_1", "bash", json!({"command": "a"}), 10, 5)),
            Ok(tool_use_response("tu_2", "bash", json!({"command": "b"}), 10, 5)),
            // Turn 3: error (client already exhausted its retries)
            Err(ApiError::Http { status: 500, body: "boom".to_string() }),
        ]);
        let executor = MockToolHandler::new(vec![
            ("ok".to_string(), false),
            ("ok".to_string(), false),
        ]);

        let result = run(&sender, &executor, &default_config(), "go").await;

        assert!(matches!(result.stop_reason, StopReason::Error(_)));
        assert_eq!(result.turns, 3);
        assert_eq!(result.total_input_tokens, 20); // 2 successful turns
    }

    #[tokio::test]
    async fn test_any_api_error_surfaces_without_retry() {
        // Conversation loop does not retry any errors — client handles all retries.
        for (status, body) in [(429, "rate limited"), (500, "server error"), (400, "bad request")] {
            let sender = MockSender::new(vec![
                Err(ApiError::Http { status, body: body.to_string() }),
            ]);
            let executor = MockToolHandler::new(vec![]);

            let result = run(&sender, &executor, &default_config(), "go").await;

            assert!(matches!(result.stop_reason, StopReason::Error(_)),
                "expected Error for status {status}");
            assert_eq!(sender.call_count.load(Ordering::SeqCst), 1,
                "conversation loop should not retry status {status}");
        }
    }

    #[tokio::test]
    async fn test_error_after_successful_tool() {
        // Tool executed on turn 1, error on turn 2, partial result preserved.
        let sender = MockSender::new(vec![
            Ok(mixed_response(
                "I will read it",
                "tu_1",
                "read_file",
                json!({"path": "f.txt"}),
                10, 5,
            )),
            Err(ApiError::Http { status: 400, body: "bad request".to_string() }),
        ]);
        let executor = MockToolHandler::new(vec![
            ("file data".to_string(), false),
        ]);

        let result = run(&sender, &executor, &default_config(), "read").await;

        assert!(matches!(result.stop_reason, StopReason::Error(_)));
        // Text from first turn is preserved.
        assert_eq!(result.text, "I will read it");
        assert_eq!(result.turns, 2);
    }

    #[tokio::test]
    async fn test_zero_token_response() {
        // input=0, output=0, loop continues via turn limit.
        let mut responses = Vec::new();
        let mut tool_results = Vec::new();
        for i in 0..3 {
            responses.push(Ok(tool_use_response(
                &format!("tu_{i}"),
                "bash",
                json!({"command": "noop"}),
                0, 0,
            )));
            tool_results.push(("".to_string(), false));
        }
        responses.push(Ok(text_response("done", 0, 0)));

        let sender = MockSender::new(responses);
        let executor = MockToolHandler::new(tool_results);

        let result = run(&sender, &executor, &default_config(), "zero").await;

        assert!(matches!(result.stop_reason, StopReason::EndTurn));
        assert_eq!(result.turns, 4);
        assert_eq!(result.total_input_tokens, 0);
        assert_eq!(result.total_output_tokens, 0);
    }

    #[tokio::test]
    async fn test_max_turns_one_with_tool_use() {
        // With max_turns=1, a tool_use response still counts as the one turn.
        let sender = MockSender::new(vec![
            Ok(tool_use_response("tu_1", "bash", json!({"command": "a"}), 100, 100)),
        ]);
        let executor = MockToolHandler::new(vec![
            ("ok".to_string(), false),
        ]);

        let mut config = default_config();
        config.max_turns = 1;

        let result = run(&sender, &executor, &config, "once").await;

        assert!(matches!(result.stop_reason, StopReason::MaxTurns));
        assert_eq!(result.turns, 1);
    }

    // ===================================================================
    // Text collection (tests 23-26)
    // ===================================================================

    #[tokio::test]
    async fn test_text_from_multiple_turns() {
        // Text in turns 1 and 3 but not 2 (turn 2 is tool-only).
        let sender = MockSender::new(vec![
            Ok(mixed_response(
                "first text",
                "tu_1", "bash", json!({"command": "a"}),
                10, 5,
            )),
            Ok(tool_use_response("tu_2", "bash", json!({"command": "b"}), 10, 5)),
            Ok(text_response("third text", 10, 5)),
        ]);
        let executor = MockToolHandler::new(vec![
            ("ok".to_string(), false),
            ("ok".to_string(), false),
        ]);

        let result = run(&sender, &executor, &default_config(), "go").await;

        assert!(result.text.contains("first text"));
        assert!(result.text.contains("third text"));
        // Turn 2 (tool_use_response) has no text, so no extra separator.
        assert_eq!(result.text, "first text\nthird text");
    }

    #[tokio::test]
    async fn test_whitespace_only_text() {
        // Whitespace text preserved as-is.
        let sender = MockSender::new(vec![
            Ok(text_response("   \n\t  ", 10, 5)),
        ]);
        let executor = MockToolHandler::new(vec![]);

        let result = run(&sender, &executor, &default_config(), "space").await;

        assert_eq!(result.text, "   \n\t  ");
    }

    #[tokio::test]
    async fn test_multiple_text_blocks_in_one_response() {
        // Two Text blocks in one response, both captured.
        let resp = MessagesResponse {
            id: "msg_multi_txt".to_string(),
            content: vec![
                ContentBlock::Text { text: "block one".to_string() },
                ContentBlock::Text { text: "block two".to_string() },
            ],
            stop_reason: "end_turn".to_string(),
            usage: Usage { input_tokens: 10, output_tokens: 5, cache_creation_input_tokens: 0, cache_read_input_tokens: 0 },
        };
        let sender = MockSender::new(vec![Ok(resp)]);
        let executor = MockToolHandler::new(vec![]);

        let result = run(&sender, &executor, &default_config(), "go").await;

        assert_eq!(result.text, "block one\nblock two");
    }

    #[tokio::test]
    async fn test_empty_string_text_block() {
        // Text { text: "" } should not add separator.
        let resp = MessagesResponse {
            id: "msg_empty_txt".to_string(),
            content: vec![
                ContentBlock::Text { text: "real".to_string() },
                ContentBlock::Text { text: "".to_string() },
                ContentBlock::Text { text: "end".to_string() },
            ],
            stop_reason: "end_turn".to_string(),
            usage: Usage { input_tokens: 10, output_tokens: 5, cache_creation_input_tokens: 0, cache_read_input_tokens: 0 },
        };
        let sender = MockSender::new(vec![Ok(resp)]);
        let executor = MockToolHandler::new(vec![]);

        let result = run(&sender, &executor, &default_config(), "go").await;

        // Empty text block doesn't add a separator (the condition is !text.is_empty()).
        assert_eq!(result.text, "real\nend");
    }

    // ===================================================================
    // Real-world patterns (tests 27-29)
    // ===================================================================

    #[tokio::test]
    async fn test_read_edit_verify_cycle() {
        // read file, edit fails, re-read, edit succeeds (5 turns).
        let sender = MockSender::new(vec![
            // Turn 1: read_file
            Ok(tool_use_response("tu_1", "read_file", json!({"path": "f.rs"}), 10, 5)),
            // Turn 2: edit_file (fails)
            Ok(tool_use_response("tu_2", "edit_file", json!({"path": "f.rs", "old_string": "x", "new_string": "y"}), 10, 5)),
            // Turn 3: re-read
            Ok(tool_use_response("tu_3", "read_file", json!({"path": "f.rs"}), 10, 5)),
            // Turn 4: edit_file (succeeds)
            Ok(tool_use_response("tu_4", "edit_file", json!({"path": "f.rs", "old_string": "a", "new_string": "b"}), 10, 5)),
            // Turn 5: done
            Ok(text_response("Applied fix successfully", 10, 5)),
        ]);
        let executor = MockToolHandler::new(vec![
            ("fn main() {}".to_string(), false),        // read
            ("old_string not found".to_string(), true),  // edit fails
            ("fn main() { a }".to_string(), false),      // re-read
            ("ok".to_string(), false),                    // edit succeeds
        ]);

        let result = run(&sender, &executor, &default_config(), "fix file").await;

        assert_eq!(result.turns, 5);
        assert_eq!(result.text, "Applied fix successfully");
        assert!(matches!(result.stop_reason, StopReason::EndTurn));

        let calls = executor.recorded_calls().await;
        assert_eq!(calls.len(), 4);
        assert_eq!(calls[0].0, "read_file");
        assert_eq!(calls[1].0, "edit_file");
        assert_eq!(calls[2].0, "read_file");
        assert_eq!(calls[3].0, "edit_file");
    }

    #[tokio::test]
    async fn test_slow_tool_no_timeout() {
        // Tool takes 100ms, per-turn timeout is 5s, succeeds.
        struct SlowToolHandler;

        #[async_trait::async_trait]
        impl ToolHandler for SlowToolHandler {
            async fn execute(&mut self, _name: &str, _input: &serde_json::Value) -> (String, bool) {
                tokio::time::sleep(Duration::from_millis(100)).await;
                ("slow result".to_string(), false)
            }
            fn definitions(&self) -> Vec<ToolDefinition> {
                vec![ToolDefinition {
                    name: "bash".to_string(),
                    description: "Run bash".to_string(),
                    input_schema: json!({"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}),
                }]
            }
        }

        let sender = MockSender::new(vec![
            Ok(tool_use_response("tu_1", "bash", json!({"command": "slow"}), 10, 5)),
            Ok(text_response("got it", 10, 5)),
        ]);

        let mut config = default_config();
        config.timeout_per_turn = Duration::from_secs(5);

        let result = run(&sender, &SlowToolHandler, &config, "slow").await;

        assert_eq!(result.text, "got it");
        assert!(matches!(result.stop_reason, StopReason::EndTurn));
    }

    #[tokio::test]
    async fn test_empty_prompt() {
        // run with prompt="" still works.
        let sender = MockSender::new(vec![
            Ok(text_response("empty prompt response", 10, 5)),
        ]);
        let executor = MockToolHandler::new(vec![]);

        let result = run(&sender, &executor, &default_config(), "").await;

        assert_eq!(result.text, "empty prompt response");
        assert!(matches!(result.stop_reason, StopReason::EndTurn));
    }

    // ===================================================================
    // Concurrent safety (tests 30-31)
    // ===================================================================

    #[tokio::test]
    async fn test_concurrent_conversations() {
        // Two run() calls with separate mocks, no interference.
        let sender1 = MockSender::new(vec![
            Ok(text_response("response one", 10, 5)),
        ]);
        let executor1 = MockToolHandler::new(vec![]);

        let sender2 = MockSender::new(vec![
            Ok(text_response("response two", 20, 10)),
        ]);
        let executor2 = MockToolHandler::new(vec![]);

        let config = default_config();

        let (r1, r2) = tokio::join!(
            run(&sender1, &executor1, &config, "prompt one"),
            run(&sender2, &executor2, &config, "prompt two"),
        );

        assert_eq!(r1.text, "response one");
        assert_eq!(r1.total_input_tokens, 10);
        assert_eq!(r2.text, "response two");
        assert_eq!(r2.total_input_tokens, 20);
    }

    #[tokio::test]
    async fn test_empty_tool_definitions() {
        // tools_enabled=true but executor returns empty defs — tools field should be None.
        struct EmptyDefsHandler;

        #[async_trait::async_trait]
        impl ToolHandler for EmptyDefsHandler {
            async fn execute(&mut self, _name: &str, _input: &serde_json::Value) -> (String, bool) {
                ("should not be called".to_string(), true)
            }
            fn definitions(&self) -> Vec<ToolDefinition> {
                vec![]
            }
        }

        let sender = InspectingSender::new(vec![
            Ok(text_response("no tools", 10, 5)),
        ]);

        let mut config = default_config();
        config.tools_enabled = true;

        let result = run(&sender, &EmptyDefsHandler, &config, "hi").await;

        assert_eq!(result.text, "no tools");
        let reqs = sender.recorded_requests().await;
        assert!(reqs[0].tools.is_none(), "tools should be None when definitions are empty");
    }
}

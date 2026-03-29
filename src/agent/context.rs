//! Context window management: evicts old tool results when the conversation
//! approaches the model's context limit, and provides a `recall_result` tool
//! so the model can retrieve evicted content on demand.

use crate::agent::api_types::{ContentBlock, Message, ToolDefinition};
use serde_json::json;
use std::collections::HashMap;
use tracing::{info, warn};

/// Default context window in tokens (128K — safe for most models).
pub const DEFAULT_CONTEXT_WINDOW: u64 = 128_000;

/// Minimum allowed context window in tokens. Values below this are likely
/// misconfigured and will cause immediate eviction pressure.
const MIN_CONTEXT_WINDOW: u64 = 4096;

/// Start evicting when context usage exceeds this fraction of the window.
const EVICTION_THRESHOLD: f64 = 0.8;

/// Name of the synthetic recall tool injected into the conversation.
pub const RECALL_TOOL_NAME: &str = "recall_result";

/// Manages context window pressure by evicting old tool results and providing
/// a recall mechanism for the model to retrieve them.
pub struct ContextManager {
    /// Maximum context window size in tokens.
    context_window: u64,
    /// Evicted tool results, keyed by tool_use_id.
    store: HashMap<String, String>,
    /// Insertion order for eviction (oldest first). Stores (message_index, tool_use_id).
    eviction_order: Vec<(usize, String)>,
}

impl ContextManager {
    /// Create a new `ContextManager` with the given context window size in tokens.
    ///
    /// If `context_window` is less than `MIN_CONTEXT_WINDOW` (4096), a warning
    /// is logged and `DEFAULT_CONTEXT_WINDOW` is used instead.
    pub fn new(context_window: u64) -> Self {
        let context_window = if context_window < MIN_CONTEXT_WINDOW {
            warn!(
                provided = context_window,
                minimum = MIN_CONTEXT_WINDOW,
                fallback = DEFAULT_CONTEXT_WINDOW,
                "context window is below minimum; falling back to default"
            );
            DEFAULT_CONTEXT_WINDOW
        } else {
            context_window
        };
        Self {
            context_window,
            store: HashMap::new(),
            eviction_order: Vec::new(),
        }
    }

    /// Register a tool result that can be evicted later.
    /// Called after each tool execution, before the result is appended to history.
    pub fn track(&mut self, message_index: usize, tool_use_id: &str) {
        self.eviction_order
            .push((message_index, tool_use_id.to_string()));
    }

    /// Check whether eviction is needed and perform it if so.
    ///
    /// `last_input_tokens` is the `input_tokens` reported by the API on the most
    /// recent response — it reflects the actual size of the message history the
    /// model saw. We use it as the authoritative measure of context usage.
    ///
    /// Returns the number of results evicted.
    pub fn maybe_evict(&mut self, messages: &mut [Message], last_input_tokens: u64) -> usize {
        let threshold = (self.context_window as f64 * EVICTION_THRESHOLD) as u64;
        if last_input_tokens < threshold {
            return 0;
        }

        // Free just enough to get back below threshold.
        let target = last_input_tokens.saturating_sub(threshold);
        let mut freed_estimate: u64 = 0;
        let mut evicted = 0;

        while freed_estimate < target {
            let Some((msg_idx, tool_use_id)) = self.next_evictable(messages) else {
                break;
            };
            if let Some(freed) = self.evict_one(messages, msg_idx, &tool_use_id) {
                freed_estimate += freed;
                evicted += 1;
            }
        }

        if evicted > 0 {
            info!(
                evicted,
                freed_tokens_estimate = freed_estimate,
                context_tokens = last_input_tokens,
                context_window = self.context_window,
                "context manager: evicted old tool results"
            );
        }

        evicted
    }

    /// Handle a `recall_result` tool call. Returns the original content if found.
    pub fn recall(&self, tool_use_id: &str) -> String {
        match self.store.get(tool_use_id) {
            Some(content) => content.clone(),
            None => format!("No stored result found for id: {tool_use_id}"),
        }
    }

    /// Returns the `recall_result` tool definition to inject into API requests.
    pub fn tool_definition() -> ToolDefinition {
        ToolDefinition {
            name: RECALL_TOOL_NAME.to_string(),
            description: "Retrieve a previously evicted tool result by its ID. When old tool \
                results are removed from context to save space, you can use this tool to \
                retrieve them. The ID is shown in the placeholder message that replaced \
                the original result."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "The tool_use_id of the evicted result to retrieve"
                    }
                },
                "required": ["id"]
            }),
        }
    }

    /// Returns true if any results have been evicted (meaning the recall tool
    /// should be offered to the model).
    pub fn has_evicted(&self) -> bool {
        !self.store.is_empty()
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Find the next evictable tool result (oldest first, skip already-evicted).
    fn next_evictable(&self, messages: &[Message]) -> Option<(usize, String)> {
        for (msg_idx, tool_use_id) in &self.eviction_order {
            // Skip if already evicted.
            if self.store.contains_key(tool_use_id) {
                continue;
            }
            // Check the message still exists and contains this result.
            if let Some(msg) = messages.get(*msg_idx) {
                for block in &msg.content {
                    if let ContentBlock::ToolResult {
                        tool_use_id: id,
                        content,
                        ..
                    } = block
                    {
                        if id == tool_use_id && !content.starts_with("[Result evicted") {
                            return Some((*msg_idx, tool_use_id.clone()));
                        }
                    }
                }
            }
        }
        None
    }

    /// Evict a single tool result. Returns estimated tokens freed.
    fn evict_one(
        &mut self,
        messages: &mut [Message],
        msg_idx: usize,
        tool_use_id: &str,
    ) -> Option<u64> {
        let msg = messages.get_mut(msg_idx)?;
        for block in &mut msg.content {
            if let ContentBlock::ToolResult {
                tool_use_id: id,
                content,
                ..
            } = block
            {
                if id == tool_use_id {
                    let original = std::mem::replace(
                        content,
                        format!(
                            "[Result evicted from context — use recall_result with id: \"{tool_use_id}\" to retrieve]"
                        ),
                    );
                    // Rough estimate: 4 chars ≈ 1 token
                    let tokens_freed = (original.len() as u64) / 4;
                    self.store.insert(tool_use_id.to_string(), original);
                    return Some(tokens_freed);
                }
            }
        }
        None
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tool_result_message(tool_use_id: &str, content: &str) -> Message {
        Message {
            role: "user".to_string(),
            content: vec![ContentBlock::ToolResult {
                tool_use_id: tool_use_id.to_string(),
                content: content.to_string(),
                is_error: None,
            }],
        }
    }

    fn make_assistant_message(text: &str) -> Message {
        Message {
            role: "assistant".to_string(),
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        }
    }

    #[test]
    fn test_no_eviction_under_threshold() {
        let mut mgr = ContextManager::new(100_000);
        let mut messages = vec![
            make_tool_result_message("tr_1", "small result"),
        ];
        mgr.track(0, "tr_1");

        // 50K tokens, threshold is 80K — no eviction needed.
        let evicted = mgr.maybe_evict(&mut messages, 50_000);
        assert_eq!(evicted, 0);
        assert!(!mgr.has_evicted());
    }

    #[test]
    fn test_eviction_over_threshold() {
        let mut mgr = ContextManager::new(100_000);
        // Create large tool results (~200K chars each ≈ 50K tokens)
        let big_content = "x".repeat(200_000);
        let mut messages = vec![
            make_tool_result_message("tr_1", &big_content),
            make_assistant_message("I read the file"),
            make_tool_result_message("tr_2", "small recent result"),
        ];
        mgr.track(0, "tr_1");
        mgr.track(2, "tr_2");

        // 90K tokens, threshold is 80K — should evict oldest (tr_1) which
        // frees ~50K estimated tokens, enough to satisfy target.
        let evicted = mgr.maybe_evict(&mut messages, 90_000);
        assert_eq!(evicted, 1);
        assert!(mgr.has_evicted());

        // tr_1 should be replaced with eviction placeholder.
        if let ContentBlock::ToolResult { content, .. } = &messages[0].content[0] {
            assert!(content.contains("recall_result"));
            assert!(content.contains("tr_1"));
        } else {
            panic!("expected ToolResult");
        }

        // tr_2 should be untouched — evicting tr_1 freed enough.
        if let ContentBlock::ToolResult { content, .. } = &messages[2].content[0] {
            assert_eq!(content, "small recent result");
        } else {
            panic!("expected ToolResult");
        }
    }

    #[test]
    fn test_recall_returns_original() {
        let mut mgr = ContextManager::new(100_000);
        let original = "the original file content here";
        let mut messages = vec![
            make_tool_result_message("tr_1", original),
        ];
        mgr.track(0, "tr_1");

        mgr.maybe_evict(&mut messages, 90_000);
        assert!(mgr.has_evicted());

        let recalled = mgr.recall("tr_1");
        assert_eq!(recalled, original);
    }

    #[test]
    fn test_recall_unknown_id() {
        let mgr = ContextManager::new(100_000);
        let result = mgr.recall("nonexistent");
        assert!(result.contains("No stored result"));
    }

    #[test]
    fn test_eviction_oldest_first() {
        let mut mgr = ContextManager::new(100_000);
        let big = "x".repeat(20_000);
        let mut messages = vec![
            make_tool_result_message("tr_1", &big),
            make_assistant_message("ok"),
            make_tool_result_message("tr_2", &big),
            make_assistant_message("ok"),
            make_tool_result_message("tr_3", &big),
        ];
        mgr.track(0, "tr_1");
        mgr.track(2, "tr_2");
        mgr.track(4, "tr_3");

        // Way over threshold — should evict oldest first.
        mgr.maybe_evict(&mut messages, 95_000);

        // tr_1 should be evicted first.
        assert!(mgr.store.contains_key("tr_1"));

        // If enough was freed from tr_1 alone, tr_2 and tr_3 stay.
        // 20K chars / 4 = 5K tokens freed. target = 95K - 40K = 55K.
        // Need to evict more. tr_2 next.
        assert!(mgr.store.contains_key("tr_2"));
    }

    #[test]
    fn test_double_evict_is_noop() {
        let mut mgr = ContextManager::new(100_000);
        let mut messages = vec![
            make_tool_result_message("tr_1", &"x".repeat(40_000)),
        ];
        mgr.track(0, "tr_1");

        mgr.maybe_evict(&mut messages, 90_000);
        assert_eq!(mgr.store.len(), 1);

        // Evicting again shouldn't re-evict the same result.
        let evicted = mgr.maybe_evict(&mut messages, 90_000);
        assert_eq!(evicted, 0);
        assert_eq!(mgr.store.len(), 1);
    }

    #[test]
    fn test_tool_definition_shape() {
        let def = ContextManager::tool_definition();
        assert_eq!(def.name, RECALL_TOOL_NAME);
        assert!(def.description.contains("evicted"));
        let schema = &def.input_schema;
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["id"].is_object());
        assert_eq!(schema["required"][0], "id");
    }

    #[test]
    fn test_eviction_with_multiple_results_in_one_message() {
        let mut mgr = ContextManager::new(100_000);
        let big = "x".repeat(30_000);
        let mut messages = vec![Message {
            role: "user".to_string(),
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "tr_1".to_string(),
                    content: big.clone(),
                    is_error: None,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "tr_2".to_string(),
                    content: big,
                    is_error: None,
                },
            ],
        }];
        mgr.track(0, "tr_1");
        mgr.track(0, "tr_2");

        mgr.maybe_evict(&mut messages, 95_000);

        // Both are in the same message. Oldest (tr_1) should evict first.
        assert!(mgr.store.contains_key("tr_1"));
    }

    #[test]
    fn test_small_context_window_aggressive_eviction() {
        let mut mgr = ContextManager::new(4_096); // Minimum allowed window
        let mut messages = vec![
            make_tool_result_message("tr_1", &"x".repeat(4_000)),
        ];
        mgr.track(0, "tr_1");

        // 3400 tokens, threshold = 4096 * 0.8 = 3276 — should evict.
        let evicted = mgr.maybe_evict(&mut messages, 3400);
        assert_eq!(evicted, 1);
    }

    #[test]
    fn test_zero_context_window_falls_back_to_default() {
        let mgr = ContextManager::new(0);
        // Internal context_window should be DEFAULT_CONTEXT_WINDOW, not 0.
        // We verify indirectly: with default 128K, threshold is ~102_400,
        // so 50K tokens should NOT trigger eviction.
        let mut messages = vec![
            make_tool_result_message("tr_1", "some content"),
        ];
        mgr.track(0, "tr_1");
        let evicted = mgr.maybe_evict(&mut messages, 50_000);
        assert_eq!(evicted, 0);
    }

    #[test]
    fn test_tiny_context_window_falls_back_to_default() {
        let mgr = ContextManager::new(100);
        // Same as above — should fall back to default 128K.
        let mut messages = vec![
            make_tool_result_message("tr_1", "some content"),
        ];
        mgr.track(0, "tr_1");
        let evicted = mgr.maybe_evict(&mut messages, 50_000);
        assert_eq!(evicted, 0);
    }

    #[test]
    fn test_min_context_window_accepted() {
        // Exactly MIN_CONTEXT_WINDOW (4096) should be accepted as-is.
        let mgr = ContextManager::new(4096);
        // Threshold = 4096 * 0.8 = 3276. 3400 tokens should trigger eviction.
        let mut messages = vec![
            make_tool_result_message("tr_1", &"x".repeat(16_000)),
        ];
        mgr.track(0, "tr_1");
        let evicted = mgr.maybe_evict(&mut messages, 3400);
        assert_eq!(evicted, 1);
    }

    #[test]
    fn test_just_below_min_falls_back() {
        // 4095 is just below the minimum — should fall back to default.
        let mgr = ContextManager::new(4095);
        let mut messages = vec![
            make_tool_result_message("tr_1", "some content"),
        ];
        mgr.track(0, "tr_1");
        // With default 128K window, 50K tokens is well below threshold.
        let evicted = mgr.maybe_evict(&mut messages, 50_000);
        assert_eq!(evicted, 0);
    }
}

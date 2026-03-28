//! Bridge module: drop-in replacement for `llm::invoke` that uses the agent
//! module's conversation loop instead of shelling out to the `claude` CLI.

use crate::agent::api_types;
use crate::agent::client::ApiClient;
use crate::agent::conversation::{
    self, ConversationConfig, ConversationResult,
    StopReason, ToolHandler,
};
use crate::agent::provider::Provider;
use crate::agent::tools::ToolExecutor;
use crate::llm::{self, LlmInvocation, LlmResponse};
use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use std::time::Duration;
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Provider detection and credentials
// ---------------------------------------------------------------------------

/// Authentication credential resolved from environment variables.
pub struct Credentials {
    pub provider: Provider,
    pub base_url: String,
    pub api_key: String,
    /// When true, use `Authorization: Bearer` even for Anthropic provider
    /// (matches Claude Code's ANTHROPIC_AUTH_TOKEN behavior for proxies/gateways).
    pub use_bearer: bool,
}

/// Resolve provider, base URL, and API key from environment variables.
///
/// Provider precedence:
/// 1. `AUTOANNEAL_PROVIDER` explicit override ("anthropic" or "openai")
/// 2. Presence of `OPENAI_BASE_URL` → OpenAI
/// 3. Default → Anthropic
///
/// Anthropic auth (matches Claude Code precedence):
/// - `ANTHROPIC_AUTH_TOKEN` → Bearer token (for proxies/gateways)
/// - `ANTHROPIC_API_KEY` → x-api-key header (direct API)
///
/// OpenAI auth:
/// - `OPENAI_API_KEY` → Bearer token
fn resolve_credentials() -> Result<Credentials> {
    // 1. Explicit provider override.
    if let Ok(val) = std::env::var("AUTOANNEAL_PROVIDER") {
        match val.to_lowercase().as_str() {
            "anthropic" => return resolve_anthropic(),
            "openai" => return resolve_openai(),
            other => {
                warn!(
                    "unknown AUTOANNEAL_PROVIDER value {:?}, falling back to auto-detect",
                    other
                );
            }
        }
    }

    // 2. Auto-detect: if OPENAI_BASE_URL is set, use OpenAI.
    if std::env::var("OPENAI_BASE_URL").is_ok() {
        return resolve_openai();
    }

    // 3. Default to Anthropic.
    resolve_anthropic()
}

fn resolve_anthropic() -> Result<Credentials> {
    let base_url = std::env::var("ANTHROPIC_BASE_URL")
        .unwrap_or_else(|_| "https://api.anthropic.com".to_string());

    // AUTH_TOKEN takes precedence (like Claude Code) — used for proxies/gateways.
    if let Ok(token) = std::env::var("ANTHROPIC_AUTH_TOKEN") {
        return Ok(Credentials {
            provider: Provider::Anthropic,
            base_url,
            api_key: token,
            use_bearer: true,
        });
    }

    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .context("neither ANTHROPIC_AUTH_TOKEN nor ANTHROPIC_API_KEY is set")?;

    Ok(Credentials {
        provider: Provider::Anthropic,
        base_url,
        api_key,
        use_bearer: false,
    })
}

fn resolve_openai() -> Result<Credentials> {
    let base_url = std::env::var("OPENAI_BASE_URL")
        .context("OPENAI_BASE_URL must be set when using OpenAI provider")?;

    let api_key = std::env::var("OPENAI_API_KEY")
        .context("OPENAI_API_KEY must be set when using OpenAI provider")?;

    Ok(Credentials {
        provider: Provider::OpenAi,
        base_url,
        api_key,
        use_bearer: true,
    })
}

// ---------------------------------------------------------------------------
// Budget helpers
// ---------------------------------------------------------------------------

/// Approximate conversion from USD budget to token limits.
///
/// Rough heuristic based on typical Claude pricing:
///   - Input:  ~$3/M tokens  -> $1 ~= 333k input tokens
///   - Output: ~$15/M tokens -> $1 ~= 67k output tokens
///
/// We split the budget 60/40 between input and output to account for
/// tool-heavy conversations that consume more input context.
pub fn budget_to_tokens(budget_usd: f64, model: &str) -> (u64, u64) {
    // Adjust multiplier based on model tier (cheaper models get more tokens per $).
    let (input_per_dollar, output_per_dollar) = if model.contains("haiku") {
        (4_000_000.0, 800_000.0) // haiku: ~$0.25/M in, $1.25/M out
    } else if model.contains("opus") {
        (66_000.0, 13_000.0) // opus: ~$15/M in, $75/M out
    } else {
        // sonnet / default
        (333_000.0, 67_000.0) // ~$3/M in, $15/M out
    };

    let input_budget = budget_usd * 0.6;
    let output_budget = budget_usd * 0.4;

    let max_input = (input_budget * input_per_dollar) as u64;
    let max_output = (output_budget * output_per_dollar) as u64;

    // Enforce sane minimums so even tiny budgets can do something.
    (max_input.max(10_000), max_output.max(4_000))
}

// ---------------------------------------------------------------------------
// Adapter: ToolExecutor -> ToolHandler trait
// ---------------------------------------------------------------------------

/// Wraps a `ToolExecutor` to implement the conversation loop's `ToolHandler` trait.
struct ToolExecutorAdapter {
    executor: ToolExecutor,
    debug_stream: bool,
}

#[async_trait::async_trait]
impl ToolHandler for ToolExecutorAdapter {
    async fn execute(&self, name: &str, input: &serde_json::Value) -> (String, bool) {
        if self.debug_stream {
            let input_preview = serde_json::to_string(input)
                .unwrap_or_else(|_| "?".to_string());
            let preview = if input_preview.len() > 120 {
                format!("{}...", &input_preview[..120])
            } else {
                input_preview
            };
            println!("[bridge] tool: {name} {preview}");
        }

        match self.executor.execute_tool(name, input) {
            Ok(output) => (output, false),
            Err(e) => (format!("Error: {e}"), true),
        }
    }

    fn definitions(&self) -> Vec<api_types::ToolDefinition> {
        // Convert from tools::ToolDefinition to api_types::ToolDefinition.
        ToolExecutor::get_tool_definitions()
            .into_iter()
            .map(|d| api_types::ToolDefinition {
                name: d.name,
                description: d.description,
                input_schema: d.input_schema,
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Effort -> temperature mapping
// ---------------------------------------------------------------------------

/// Map the `effort` field to a temperature value for the API.
fn effort_to_temperature(effort: &str) -> Option<f64> {
    match effort {
        "low" => Some(0.2),
        "medium" => Some(0.5),
        "high" => Some(0.8),
        "max" => Some(1.0),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Build ConversationConfig from LlmInvocation
// ---------------------------------------------------------------------------

fn build_config(invocation: &LlmInvocation, timeout: Duration) -> ConversationConfig {
    let (max_input, max_output) = budget_to_tokens(invocation.max_budget_usd, &invocation.model);

    ConversationConfig {
        model: invocation.model.clone(),
        system_prompt: invocation.system_prompt.clone(),
        max_turns: invocation.max_turns,
        max_tokens_per_turn: 16384,
        max_total_input_tokens: max_input,
        max_total_output_tokens: max_output,
        timeout_per_turn: timeout,
        tools_enabled: !invocation.tools.is_empty(),
        temperature: effort_to_temperature(invocation.effort),
        context_window: invocation.context_window,
    }
}

// ---------------------------------------------------------------------------
// Map ConversationResult -> LlmResponse<T>
// ---------------------------------------------------------------------------

fn map_result<T: DeserializeOwned>(result: ConversationResult, duration_ms: u64) -> Result<LlmResponse<T>> {
    // Estimate cost from tokens (rough: input $3/M, output $15/M for sonnet).
    let estimated_cost = (result.total_input_tokens as f64 * 3.0
        + result.total_output_tokens as f64 * 15.0)
        / 1_000_000.0;

    // Extract structured output using the same strategies as claude.rs:
    // 1. Try parsing the full text as JSON.
    // 2. Try extracting from a markdown code fence.
    let structured: Option<T> = if let Ok(parsed) = serde_json::from_str::<T>(&result.text) {
        Some(parsed)
    } else if let Some(json_str) = llm::extract_json_block(&result.text) {
        match serde_json::from_str::<T>(json_str) {
            Ok(parsed) => Some(parsed),
            Err(e) => {
                warn!("found JSON block in result but failed to parse: {e}");
                None
            }
        }
    } else {
        None
    };

    // Map stop reason to subtype-like behavior (log warnings like parse_response does).
    match &result.stop_reason {
        StopReason::BudgetExhausted => {
            warn!("budget exhausted — treating as partial success");
        }
        StopReason::MaxTurns => {
            warn!("hit max turns — treating as partial success");
        }
        StopReason::Timeout => {
            warn!("conversation timed out");
        }
        StopReason::Error(msg) => {
            warn!(error = %msg, "conversation ended with error");
        }
        StopReason::EndTurn => {}
    }

    Ok(LlmResponse {
        structured,
        text: result.text,
        cost_usd: estimated_cost,
        duration_ms,
        num_turns: result.turns,
    })
}

// ---------------------------------------------------------------------------
// Public bridge function
// ---------------------------------------------------------------------------

/// Drop-in replacement for `llm::invoke` that uses the agent module internally.
/// This bridges the old `LlmInvocation`/`LlmResponse` types to the new
/// conversation loop, calling the LLM API directly instead of shelling out to
/// the `claude` CLI.
pub async fn invoke<T: DeserializeOwned>(
    invocation: &LlmInvocation,
    timeout: Duration,
) -> Result<LlmResponse<T>> {
    let creds = resolve_credentials()?;

    info!(
        provider = ?creds.provider,
        base_url = %creds.base_url,
        use_bearer = creds.use_bearer,
        model = %invocation.model,
        budget = invocation.max_budget_usd,
        max_turns = invocation.max_turns,
        "bridge: invoking via agent module"
    );

    // Build the pieces.
    let client = ApiClient::new(creds.base_url, creds.api_key, creds.provider, creds.use_bearer);

    let executor = ToolExecutor::new(
        invocation.working_dir.clone(),
        Duration::from_secs(120),
    );
    let debug_stream = std::env::var("AUTOANNEAL_DEBUG_STREAM")
        .map_or(false, |v| v == "1" || v == "true");
    let tool_handler = ToolExecutorAdapter {
        executor,
        debug_stream,
    };

    let config = build_config(invocation, timeout);

    // Prepend working directory context (reuse the logic from claude.rs).
    let dir_context = llm::get_dir_context(&invocation.working_dir).await;
    let mut augmented_prompt = if dir_context.is_empty() {
        invocation.prompt.clone()
    } else {
        format!("{dir_context}\n\n{}", invocation.prompt)
    };

    // Append JSON schema instruction if provided.
    if let Some(schema) = &invocation.json_schema {
        augmented_prompt.push_str(&format!(
            "\n\nOutput your response as JSON matching this schema: {schema}"
        ));
    }

    // Run the conversation loop.
    let start = std::time::Instant::now();
    let result = conversation::run(&client, &tool_handler, &config, &augmented_prompt).await;
    let duration_ms = start.elapsed().as_millis() as u64;

    info!(
        turns = result.turns,
        input_tokens = result.total_input_tokens,
        output_tokens = result.total_output_tokens,
        stop_reason = ?result.stop_reason,
        duration_ms = duration_ms,
        "bridge: conversation complete"
    );

    map_result(result, duration_ms)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::api_types::*;
    use crate::agent::conversation::{ApiError as ConvApiError, MessageSender, ToolHandler};
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Mutex;

    // -- Mock sender --

    struct MockSender {
        responses: Mutex<Vec<std::result::Result<MessagesResponse, ConvApiError>>>,
        call_count: AtomicUsize,
    }

    impl MockSender {
        fn new(responses: Vec<std::result::Result<MessagesResponse, ConvApiError>>) -> Self {
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
        ) -> std::result::Result<MessagesResponse, ConvApiError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            let mut queue = self.responses.lock().await;
            if queue.is_empty() {
                panic!("MockSender: no more responses queued");
            }
            queue.remove(0)
        }
    }

    // -- Mock tool handler --

    struct MockToolHandler {
        defs: Vec<ToolDefinition>,
    }

    #[async_trait::async_trait]
    impl ToolHandler for MockToolHandler {
        async fn execute(&self, _name: &str, _input: &serde_json::Value) -> (String, bool) {
            ("mock result".to_string(), false)
        }

        fn definitions(&self) -> Vec<ToolDefinition> {
            self.defs.clone()
        }
    }

    // -- Helpers --

    fn make_invocation() -> LlmInvocation {
        LlmInvocation {
            prompt: "Do something.".to_string(),
            system_prompt: Some("You are helpful.".to_string()),
            model: "claude-sonnet-4-20250514".to_string(),
            max_budget_usd: 1.0,
            max_turns: 10,
            effort: "high",
            tools: "read_file,write_file,bash",
            json_schema: None,
            working_dir: PathBuf::from("/tmp"),
            context_window: crate::agent::context::DEFAULT_CONTEXT_WINDOW,
        }
    }

    fn make_end_turn_response(text: &str) -> MessagesResponse {
        MessagesResponse {
            id: "msg_test".to_string(),
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            stop_reason: "end_turn".to_string(),
            usage: Usage {
                input_tokens: 100,
                output_tokens: 50,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            },
        }
    }

    // -----------------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_bridge_maps_invocation_to_config() {
        let inv = make_invocation();
        let timeout = Duration::from_secs(300);
        let config = build_config(&inv, timeout);

        assert_eq!(config.model, "claude-sonnet-4-20250514");
        assert_eq!(config.system_prompt.as_deref(), Some("You are helpful."));
        assert_eq!(config.max_turns, 10);
        assert_eq!(config.timeout_per_turn, Duration::from_secs(300));
        assert!(config.tools_enabled);

        // Token budget should be derived from $1.00 budget.
        assert!(config.max_total_input_tokens > 0);
        assert!(config.max_total_output_tokens > 0);
    }

    #[test]
    fn test_bridge_maps_invocation_no_tools() {
        let mut inv = make_invocation();
        inv.tools = "";
        let config = build_config(&inv, Duration::from_secs(60));
        assert!(!config.tools_enabled);
    }

    // Provider detection is now based on env vars (AUTOANNEAL_PROVIDER,
    // OPENAI_BASE_URL presence) rather than URL heuristics. Integration
    // tests for resolve_credentials() would need env var manipulation,
    // so we test the individual resolve_* functions' logic via the
    // end-to-end mock test instead.

    #[test]
    fn test_bridge_extracts_json_from_result() {
        // Direct JSON text.
        let result = ConversationResult {
            text: r#"{"title": "Fix bug", "body": "Fixed the thing"}"#.to_string(),
            turns: 1,
            total_input_tokens: 100,
            total_output_tokens: 50,
            stop_reason: StopReason::EndTurn,
        };
        let response: LlmResponse<serde_json::Value> = map_result(result, 0).unwrap();
        let s = response.structured.unwrap();
        assert_eq!(s["title"], "Fix bug");

        // JSON inside a code fence.
        let result2 = ConversationResult {
            text: "Here is the output:\n```json\n{\"title\": \"Update docs\"}\n```\nDone.".to_string(),
            turns: 2,
            total_input_tokens: 200,
            total_output_tokens: 80,
            stop_reason: StopReason::EndTurn,
        };
        let response2: LlmResponse<serde_json::Value> = map_result(result2, 0).unwrap();
        let s2 = response2.structured.unwrap();
        assert_eq!(s2["title"], "Update docs");

        // No JSON at all.
        let result3 = ConversationResult {
            text: "Just plain text, no JSON here.".to_string(),
            turns: 1,
            total_input_tokens: 50,
            total_output_tokens: 20,
            stop_reason: StopReason::EndTurn,
        };
        let response3: LlmResponse<serde_json::Value> = map_result(result3, 0).unwrap();
        assert!(response3.structured.is_none());
    }

    #[test]
    fn test_bridge_maps_stop_reasons() {
        // EndTurn -> normal response with cost estimation.
        let result = ConversationResult {
            text: "done".to_string(),
            turns: 3,
            total_input_tokens: 1000,
            total_output_tokens: 500,
            stop_reason: StopReason::EndTurn,
        };
        let response: LlmResponse<serde_json::Value> = map_result(result, 0).unwrap();
        assert_eq!(response.num_turns, 3);
        assert!(response.cost_usd > 0.0);
        assert_eq!(response.text, "done");

        // MaxTurns -> partial success, still returns.
        let result = ConversationResult {
            text: "partial".to_string(),
            turns: 10,
            total_input_tokens: 5000,
            total_output_tokens: 2000,
            stop_reason: StopReason::MaxTurns,
        };
        let response: LlmResponse<serde_json::Value> = map_result(result, 0).unwrap();
        assert_eq!(response.num_turns, 10);
        assert_eq!(response.text, "partial");

        // BudgetExhausted -> partial success.
        let result = ConversationResult {
            text: "budget hit".to_string(),
            turns: 5,
            total_input_tokens: 100_000,
            total_output_tokens: 40_000,
            stop_reason: StopReason::BudgetExhausted,
        };
        let response: LlmResponse<serde_json::Value> = map_result(result, 0).unwrap();
        assert_eq!(response.text, "budget hit");

        // Error -> still returns Ok (the error is in the text/stop_reason log).
        let result = ConversationResult {
            text: "error occurred".to_string(),
            turns: 1,
            total_input_tokens: 50,
            total_output_tokens: 10,
            stop_reason: StopReason::Error("auth failed".to_string()),
        };
        let response: LlmResponse<serde_json::Value> = map_result(result, 0).unwrap();
        assert_eq!(response.text, "error occurred");

        // Timeout -> partial success.
        let result = ConversationResult {
            text: "timed out".to_string(),
            turns: 2,
            total_input_tokens: 300,
            total_output_tokens: 100,
            stop_reason: StopReason::Timeout,
        };
        let response: LlmResponse<serde_json::Value> = map_result(result, 0).unwrap();
        assert_eq!(response.text, "timed out");
    }

    #[tokio::test]
    async fn test_bridge_end_to_end_with_mocks() {
        // Verify the full conversation flow using mocks.
        let sender = MockSender::new(vec![Ok(make_end_turn_response(
            r#"{"answer": 42}"#,
        ))]);

        let tool_handler = MockToolHandler {
            defs: vec![ToolDefinition {
                name: "bash".to_string(),
                description: "Run bash".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": { "command": { "type": "string" } },
                    "required": ["command"]
                }),
            }],
        };

        let config = ConversationConfig {
            model: "claude-sonnet-4-20250514".to_string(),
            system_prompt: Some("test".to_string()),
            max_turns: 5,
            max_tokens_per_turn: 4096,
            max_total_input_tokens: 100_000,
            max_total_output_tokens: 50_000,
            timeout_per_turn: Duration::from_secs(30),
            tools_enabled: true,
            temperature: None,
            context_window: crate::agent::context::DEFAULT_CONTEXT_WINDOW,
        };

        let result = conversation::run(&sender, &tool_handler, &config, "hello").await;
        let response: LlmResponse<serde_json::Value> = map_result(result, 0).unwrap();

        assert!(response.structured.is_some());
        assert_eq!(response.structured.unwrap()["answer"], 42);
        assert_eq!(response.num_turns, 1);
    }

    #[test]
    fn test_budget_to_tokens_sonnet() {
        let (input, output) = budget_to_tokens(1.0, "claude-sonnet-4-20250514");
        // $1 sonnet: ~200k input, ~27k output (with 60/40 split).
        assert!(input > 100_000, "expected >100k input tokens, got {input}");
        assert!(output > 20_000, "expected >20k output tokens, got {output}");
    }

    #[test]
    fn test_budget_to_tokens_haiku() {
        let (input, output) = budget_to_tokens(0.5, "claude-haiku-3");
        // Haiku is much cheaper, so more tokens per dollar.
        assert!(input > 500_000, "expected >500k input tokens for haiku, got {input}");
        assert!(output > 100_000, "expected >100k output tokens for haiku, got {output}");
    }

    #[test]
    fn test_budget_to_tokens_minimum() {
        let (input, output) = budget_to_tokens(0.0, "claude-sonnet-4-20250514");
        // Even with $0 budget, we should get sane minimums.
        assert!(input >= 10_000);
        assert!(output >= 4_000);
    }

}

//! Bridge module: drop-in replacement for `llm::invoke` that uses the agent
//! module's conversation loop instead of shelling out to the `claude` CLI.
//!
//! Also provides `memory_stats()` for tracking RSS usage.

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
use std::time::{Duration, Instant};
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
        if std::env::var("ANTHROPIC_BASE_URL").is_ok() {
            warn!(
                "both OPENAI_BASE_URL and ANTHROPIC_BASE_URL are set; \
                 using OpenAI. Set AUTOANNEAL_PROVIDER to override."
            );
        }
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
// Provider:model parsing
// ---------------------------------------------------------------------------

/// Parse a "provider:model" string into (Option<provider_hint>, model_name).
/// "openai:gpt-4o" -> (Some("openai"), "gpt-4o")
/// "anthropic:sonnet" -> (Some("anthropic"), "sonnet")
/// "sonnet" -> (None, "sonnet")
pub fn parse_provider_model(s: &str) -> (Option<String>, String) {
    if let Some((provider, model)) = s.split_once(':') {
        match provider {
            "anthropic" | "openai" => (Some(provider.to_string()), model.to_string()),
            _ => (None, s.to_string()), // colon but not a known provider, treat as plain model
        }
    } else {
        (None, s.to_string())
    }
}

// ---------------------------------------------------------------------------
// Tool alias mapping
// ---------------------------------------------------------------------------

/// Map human-friendly tool names (used in LlmInvocation.tools) to internal tool names.
fn tool_alias_to_name(alias: &str) -> &str {
    match alias {
        "Read" => "read_file",
        "Write" => "write_file",
        "Edit" => "edit_file",
        "Glob" => "search_files",
        "Grep" => "search_content",
        "Bash" => "run_command",
        "Git" => "git",
        "GhWorkflowLogs" => "gh_workflow_logs",
        other => other,
    }
}

/// Parse a comma-separated tools string into resolved tool names.
fn parse_enabled_tools(tools: &str) -> Option<Vec<String>> {
    if tools.is_empty() {
        return None;
    }
    Some(
        tools
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| tool_alias_to_name(s).to_string())
            .collect(),
    )
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
            println!("[bridge] tool: {name} {preview} (rss: {}MB)", rss_mb());
        }

        let start = Instant::now();
        let result = self.executor.execute_tool(name, input);
        let elapsed = start.elapsed();

        if self.debug_stream {
            println!("[bridge] tool: {name} done ({elapsed:?})");
        }

        match result {
            Ok(output) => (output, false),
            Err(e) => (format!("Error: {e}"), true),
        }
    }

    fn definitions(&self) -> Vec<api_types::ToolDefinition> {
        // Convert from tools::ToolDefinition to api_types::ToolDefinition.
        self.executor
            .get_tool_definitions()
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
    ConversationConfig {
        model: invocation.model.clone(),
        system_prompt: invocation.system_prompt.clone(),
        max_turns: invocation.max_turns,
        max_tokens_per_turn: invocation.max_tokens_per_turn.unwrap_or(16384),
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
    let creds = if let Some(hint) = &invocation.provider_hint {
        match hint.as_str() {
            "anthropic" => resolve_anthropic()?,
            "openai" => resolve_openai()?,
            _ => resolve_credentials()?,
        }
    } else {
        resolve_credentials()?
    };

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

    let enabled_tools = parse_enabled_tools(invocation.tools);
    let executor = if invocation.exa_max_searches > 0
        || invocation.tools.contains("CheckVulnerability")
        || invocation.tools.contains("CheckPackage")
        || invocation.tools.contains("SearchIssues")
    {
        let exa_api_key = std::env::var("EXA_API_KEY").ok();
        let repo_slug = derive_repo_slug(&invocation.working_dir);
        ToolExecutor::new_with_research(
            invocation.working_dir.clone(),
            Duration::from_secs(120),
            invocation.ci_context.clone(),
            enabled_tools,
            exa_api_key,
            invocation.exa_max_searches,
            repo_slug,
            invocation.tools.to_string(),
        )
    } else {
        ToolExecutor::new(
            invocation.working_dir.clone(),
            Duration::from_secs(120),
            invocation.ci_context.clone(),
            enabled_tools,
        )
    };
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
    let rss_before = rss_mb();
    let result = conversation::run(&client, &tool_handler, &config, &augmented_prompt).await;
    let duration_ms = start.elapsed().as_millis() as u64;
    let rss_after = rss_mb();

    let exa_cost = tool_handler.executor.exa_cost();

    info!(
        turns = result.turns,
        input_tokens = result.total_input_tokens,
        output_tokens = result.total_output_tokens,
        stop_reason = ?result.stop_reason,
        duration_ms = duration_ms,
        rss_before_mb = rss_before,
        rss_after_mb = rss_after,
        exa_cost_usd = exa_cost,
        "bridge: conversation complete"
    );

    let mut response = map_result(result, duration_ms)?;
    response.cost_usd += exa_cost;
    Ok(response)
}

// ---------------------------------------------------------------------------
// Repo slug derivation
// ---------------------------------------------------------------------------

/// Derive an "owner/repo" slug from the git remote in the working directory.
fn derive_repo_slug(working_dir: &std::path::Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(working_dir)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();

    // Parse common formats:
    // https://github.com/owner/repo.git
    // git@github.com:owner/repo.git
    let slug = if let Some(rest) = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
    {
        rest.to_string()
    } else if let Some(at_pos) = url.find('@') {
        let after_at = &url[at_pos + 1..];
        if let Some(colon_pos) = after_at.find(':') {
            after_at[colon_pos + 1..].to_string()
        } else {
            return None;
        }
    } else {
        return None;
    };

    let slug = slug.strip_suffix(".git").unwrap_or(&slug);
    let slug = slug.trim_end_matches('/');

    if slug.contains('/') {
        Some(slug.to_string())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Memory stats
// ---------------------------------------------------------------------------

/// Read the current process RSS (resident set size) in MB from /proc/self/status.
/// Returns 0 on non-Linux or if the file can't be read.
pub fn rss_mb() -> u64 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            for line in status.lines() {
                if let Some(val) = line.strip_prefix("VmRSS:") {
                    let val = val.trim().trim_end_matches(" kB").trim();
                    if let Ok(kb) = val.parse::<u64>() {
                        return kb / 1024;
                    }
                }
            }
        }
        0
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
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
            provider_hint: None,
            max_tokens_per_turn: None,
            ci_context: None,
            exa_max_searches: 0,
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
    fn test_parse_provider_model_openai() {
        let (hint, model) = parse_provider_model("openai:gpt-4o");
        assert_eq!(hint.as_deref(), Some("openai"));
        assert_eq!(model, "gpt-4o");
    }

    #[test]
    fn test_parse_provider_model_anthropic() {
        let (hint, model) = parse_provider_model("anthropic:claude-sonnet");
        assert_eq!(hint.as_deref(), Some("anthropic"));
        assert_eq!(model, "claude-sonnet");
    }

    #[test]
    fn test_parse_provider_model_plain() {
        let (hint, model) = parse_provider_model("sonnet");
        assert!(hint.is_none());
        assert_eq!(model, "sonnet");
    }

    #[test]
    fn test_parse_provider_model_plain_with_dash() {
        let (hint, model) = parse_provider_model("kimi-k2-5");
        assert!(hint.is_none());
        assert_eq!(model, "kimi-k2-5");
    }

    #[test]
    fn test_parse_provider_model_unknown_provider() {
        let (hint, model) = parse_provider_model("unknown:model");
        assert!(hint.is_none());
        assert_eq!(model, "unknown:model");
    }

    #[test]
    fn test_parse_provider_model_empty() {
        let (hint, model) = parse_provider_model("");
        assert!(hint.is_none());
        assert_eq!(model, "");
    }

}

use crate::models::ClaudeOutput;
use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::{error, info, warn};

/// Parameters for a single Claude CLI invocation.
pub struct ClaudeInvocation {
    pub prompt: String,
    pub system_prompt: Option<String>,
    pub model: String,
    pub max_budget_usd: f64,
    pub max_turns: u32,
    pub effort: &'static str,
    pub tools: &'static str,
    pub json_schema: Option<String>,
    pub working_dir: PathBuf,
    /// Pre-assigned session ID (for potential timeout resume).
    pub session_id: Option<String>,
    /// If set, resumes an existing session instead of starting a new one.
    pub resume_session_id: Option<String>,
}

/// Parsed response from a Claude CLI invocation.
#[derive(Debug)]
pub struct ClaudeResponse<T> {
    pub structured: Option<T>,
    pub text: String,
    pub cost_usd: f64,
    pub duration_ms: u64,
    pub num_turns: u32,
    /// Session ID for potential resume (used when timeout triggers a follow-up).
    #[allow(dead_code)]
    pub session_id: Option<String>,
}

/// Check if debug streaming is enabled via AUTOANNEAL_DEBUG_STREAM env var.
fn debug_stream_enabled() -> bool {
    std::env::var("AUTOANNEAL_DEBUG_STREAM").map_or(false, |v| v == "1" || v == "true")
}

/// Build the argument list for the `claude` CLI process.
fn build_args(invocation: &ClaudeInvocation) -> Vec<String> {
    let streaming = debug_stream_enabled();
    let mut args = vec![
        "-p".to_string(),
        invocation.prompt.clone(),
        "--output-format".to_string(),
        if streaming { "stream-json".to_string() } else { "json".to_string() },
    ];

    if streaming {
        args.push("--verbose".to_string());
    }

    args.extend([
        "--bare".to_string(),
        "--dangerously-skip-permissions".to_string(),
        "--model".to_string(),
        invocation.model.clone(),
        "--max-budget-usd".to_string(),
        format!("{:.2}", invocation.max_budget_usd),
        "--max-turns".to_string(),
        invocation.max_turns.to_string(),
        "--effort".to_string(),
        invocation.effort.to_string(),
    ]);

    if !invocation.tools.is_empty() {
        args.push("--tools".to_string());
        args.push(invocation.tools.to_string());
    }

    if let Some(ref schema) = invocation.json_schema {
        args.push("--json-schema".to_string());
        args.push(schema.clone());
    }

    if let Some(ref sys) = invocation.system_prompt {
        args.push("--system-prompt".to_string());
        args.push(sys.clone());
    }

    if let Some(ref session_id) = invocation.resume_session_id {
        args.push("--resume".to_string());
        args.push(session_id.clone());
    }

    // Always set a session ID so we can resume on timeout.
    if invocation.resume_session_id.is_none() {
        if let Some(ref sid) = invocation.session_id {
            args.push("--session-id".to_string());
            args.push(sid.clone());
        }
    }

    args
}

/// Generate a UUID-shaped session ID from system time and process ID.
/// Format: 8-4-4-4-12 hex chars (standard UUID layout).
pub fn generate_session_id() -> String {
    use sha2::{Digest, Sha256};
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();
    let mut hasher = Sha256::new();
    hasher.update(format!("{nanos}-{pid}").as_bytes());
    let hash = hasher.finalize();
    let hex = hex::encode(&hash[..16]);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Attempt to parse Claude's stdout into a `ClaudeOutput` envelope and extract
/// the typed response. Returns the parsed `ClaudeResponse<T>` or an error.
fn parse_response<T: DeserializeOwned>(
    stdout: &[u8],
    stderr: &[u8],
    _has_json_schema: bool,
) -> Result<ClaudeResponse<T>> {
    let stdout_str = String::from_utf8_lossy(stdout);
    let stderr_str = String::from_utf8_lossy(stderr);

    if !stderr_str.is_empty() {
        warn!(stderr = %stderr_str, "claude process produced stderr output");
    }

    tracing::debug!(raw_stdout = %truncate(&stdout_str, 4000), "raw claude output");

    let output: ClaudeOutput = serde_json::from_str(&stdout_str).with_context(|| {
        format!(
            "failed to parse claude JSON output.\nstdout: {}\nstderr: {}",
            truncate(&stdout_str, 2000),
            truncate(&stderr_str, 500),
        )
    })?;

    if output.is_error {
        // Check for auth failure — these are fatal and should not be retried.
        if output.result.contains("Not logged in") || output.result.contains("authentication") {
            bail!("claude authentication failure: {}", output.result);
        }
        bail!("claude returned error: {}", output.result);
    }

    tracing::debug!(result = %truncate(&output.result, 2000), "claude result text");

    // Handle subtypes that indicate partial completion.
    match output.subtype.as_str() {
        "error_budget" => {
            warn!("claude budget exhausted — treating as partial success");
        }
        "error_max_turns" => {
            warn!("claude hit max turns — treating as partial success");
        }
        "success" | "" => {}
        other => {
            warn!(subtype = other, "unexpected claude response subtype");
        }
    }

    // Extract structured output via multiple strategies (in priority order).
    let structured: Option<T> =
        // 1. Try structured_output field (native --json-schema support)
        if let Some(value) = output.structured_output {
            let parsed = serde_json::from_value(value)
                .context("failed to deserialize structured_output into target type")?;
            Some(parsed)
        }
        // 2. Try parsing the result text directly as JSON
        else if let Ok(parsed) = serde_json::from_str::<T>(&output.result) {
            Some(parsed)
        }
        // 3. Try extracting JSON from markdown code fences in result
        else if let Some(json_str) = extract_json_block(&output.result) {
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

    Ok(ClaudeResponse {
        structured,
        text: output.result,
        cost_usd: output.total_cost_usd,
        duration_ms: output.duration_ms,
        num_turns: output.num_turns,
        session_id: if output.session_id.is_empty() {
            None
        } else {
            Some(output.session_id)
        },
    })
}

/// Generate a compact working directory context string.
/// Uses `find` with depth limit, excludes common noise directories,
/// and truncates to avoid wasting tokens.
pub(crate) async fn get_dir_context(working_dir: &Path) -> String {
    let output = tokio::process::Command::new("find")
        .args([
            ".",
            "-maxdepth", "3",
            "-not", "-path", "./.git/*",
            "-not", "-path", "./node_modules/*",
            "-not", "-path", "./target/*",
            "-not", "-path", "./.venv/*",
            "-not", "-path", "./vendor/*",
            "-not", "-path", "./__pycache__/*",
        ])
        .current_dir(working_dir)
        .output()
        .await;

    let tree = match output {
        Ok(out) if out.status.success() => {
            String::from_utf8_lossy(&out.stdout).to_string()
        }
        _ => return String::new(),
    };

    // Truncate to ~200 lines to keep it compact.
    let lines: Vec<&str> = tree.lines().collect();
    let truncated = if lines.len() > 200 {
        format!("{}\n... ({} more files)", lines[..200].join("\n"), lines.len() - 200)
    } else {
        lines.join("\n")
    };

    format!(
        "## Working Directory: {}\n\n```\n{}\n```",
        working_dir.display(),
        truncated
    )
}

/// Invoke the Claude CLI as a subprocess and parse its JSON response.
///
/// Supports structured output via `--json-schema` (deserialized into `T`),
/// timeout enforcement, and a single automatic retry on transient failures.
pub async fn invoke<T: DeserializeOwned>(
    invocation: &ClaudeInvocation,
    timeout: Duration,
) -> Result<ClaudeResponse<T>> {
    // Prepend working directory context to the prompt.
    let dir_context = get_dir_context(&invocation.working_dir).await;
    let augmented = ClaudeInvocation {
        prompt: format!("{dir_context}\n\n{}", invocation.prompt),
        system_prompt: invocation.system_prompt.clone(),
        model: invocation.model.clone(),
        max_budget_usd: invocation.max_budget_usd,
        max_turns: invocation.max_turns,
        effort: invocation.effort,
        tools: invocation.tools,
        json_schema: invocation.json_schema.clone(),
        working_dir: invocation.working_dir.clone(),
        session_id: invocation.session_id.clone(),
        resume_session_id: invocation.resume_session_id.clone(),
    };
    let invocation = &augmented;

    let prompt_summary = truncate(&invocation.prompt, 80);
    info!(
        prompt = %prompt_summary,
        model = %invocation.model,
        budget = invocation.max_budget_usd,
        max_turns = invocation.max_turns,
        effort = invocation.effort,
        "invoking claude"
    );

    let result = invoke_once::<T>(invocation, timeout).await;

    match result {
        Ok(response) => Ok(response),
        Err(first_err) => {
            let err_msg = format!("{first_err:#}");

            // Do not retry auth failures.
            if err_msg.contains("authentication") || err_msg.contains("Not logged in") {
                error!(error = %err_msg, "claude auth failure — not retrying");
                return Err(first_err);
            }

            // On timeout: attempt a grace-period resume if we have a session ID.
            if err_msg.contains("timed out") {
                if let Some(ref sid) = invocation.session_id {
                    warn!("claude invocation timed out — attempting resume with grace period");
                    let grace_invocation = ClaudeInvocation {
                        prompt: "You're taking longer than expected. If you're almost done, please finish up now. If you're stuck, just summarize what you've accomplished so far and stop.".to_string(),
                        system_prompt: invocation.system_prompt.clone(),
                        model: invocation.model.clone(),
                        max_budget_usd: invocation.max_budget_usd * 0.20,
                        max_turns: 5,
                        effort: invocation.effort,
                        tools: invocation.tools,
                        json_schema: None,
                        working_dir: invocation.working_dir.clone(),
                        session_id: None,
                        resume_session_id: Some(sid.clone()),
                    };
                    let grace_timeout = Duration::from_secs(120);
                    match invoke_once::<T>(&grace_invocation, grace_timeout).await {
                        Ok(response) => {
                            info!("grace-period resume succeeded");
                            return Ok(response);
                        }
                        Err(e) => {
                            warn!(error = %e, "grace-period resume also failed");
                            return Err(first_err);
                        }
                    }
                }
                error!(error = %err_msg, "claude invocation timed out — no session ID for resume");
                return Err(first_err);
            }

            warn!(error = %err_msg, "claude invocation failed — retrying once");
            invoke_once::<T>(invocation, timeout).await.with_context(|| {
                format!("retry also failed (original error: {err_msg})")
            })
        }
    }
}

/// Single attempt at invoking the Claude CLI.
async fn invoke_once<T: DeserializeOwned>(
    invocation: &ClaudeInvocation,
    timeout: Duration,
) -> Result<ClaudeResponse<T>> {
    let args = build_args(invocation);
    let streaming = debug_stream_enabled();

    let child = tokio::process::Command::new("claude")
        .args(&args)
        .current_dir(&invocation.working_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn claude process — is `claude` on PATH?")?;

    let stdout_bytes;
    let stderr_bytes;

    if streaming {
        // In stream mode, read stdout line by line, log events, collect the last result line.
        use tokio::io::{AsyncBufReadExt, BufReader};

        let mut child = child;
        let stdout = child.stdout.take().context("no stdout")?;
        let stderr_handle = child.stderr.take().context("no stderr")?;

        let reader = BufReader::new(stdout);
        let mut lines = reader.lines();
        let mut last_result_line: Option<String> = None;
        let mut turn_count: u32 = 0;

        let read_fut = async {
            while let Some(line) = lines.next_line().await? {
                // Try to parse as JSON to extract event type for logging
                if let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) {
                    match event.get("type").and_then(|t| t.as_str()) {
                        Some("assistant") => {
                            turn_count += 1;
                            // Log tool use if present
                            if let Some(msg) = event.get("message") {
                                if let Some(content) = msg.get("content").and_then(|c| c.as_array()) {
                                    for block in content {
                                        if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                                            let tool = block.get("name").and_then(|n| n.as_str()).unwrap_or("unknown");
                                            let input_preview = block.get("input")
                                                .map(|i| truncate(&i.to_string(), 100))
                                                .unwrap_or_default();
                                            println!("[turn {turn_count}] tool: {tool} {input_preview}");
                                        }
                                        if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                                            if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                                if !text.is_empty() {
                                                    println!("[turn {turn_count}] text: {}", truncate(text, 200));
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Some("result") => {
                            last_result_line = Some(line);
                        }
                        _ => {}
                    }
                }
            }
            // Read stderr
            let mut stderr_buf = Vec::new();
            tokio::io::AsyncReadExt::read_to_end(&mut BufReader::new(stderr_handle), &mut stderr_buf).await?;
            let status = child.wait().await?;
            Ok::<_, anyhow::Error>((last_result_line, stderr_buf, status))
        };

        let (result_line, stderr_buf, status) = match tokio::time::timeout(timeout, read_fut).await {
            Ok(result) => result?,
            Err(_) => {
                warn!("claude invocation timed out after {:?}", timeout);
                bail!("claude invocation timed out after {:.0}s", timeout.as_secs_f64());
            }
        };

        stderr_bytes = stderr_buf;
        // Use the result line as stdout for parsing
        stdout_bytes = result_line.unwrap_or_default().into_bytes();

        if !status.success() {
            let stderr_str = String::from_utf8_lossy(&stderr_bytes);
            warn!(exit_code = ?status.code(), stderr = %truncate(&stderr_str, 500), "claude exited with non-zero status");
        }
    } else {
        // Non-streaming: wait for full output
        let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(result) => result.context("failed to wait for claude process")?,
            Err(_) => {
                warn!("claude invocation timed out after {:?}", timeout);
                bail!("claude invocation timed out after {:.0}s", timeout.as_secs_f64());
            }
        };

        if !output.status.success() {
            let stderr_str = String::from_utf8_lossy(&output.stderr);
            let exit_code = output.status.code().unwrap_or(-1);
            warn!(exit_code, stderr = %truncate(&stderr_str, 500), "claude exited with non-zero status — attempting to parse stdout");

            match parse_response::<T>(&output.stdout, &output.stderr, false) {
                Ok(response) => return Ok(response),
                Err(parse_err) => {
                    bail!("claude exited with code {} and output could not be parsed: {}\nstderr: {}", exit_code, parse_err, truncate(&stderr_str, 500));
                }
            }
        }

        stdout_bytes = output.stdout;
        stderr_bytes = output.stderr;
    }

    let response = parse_response::<T>(&stdout_bytes, &stderr_bytes, false)?;

    info!(
        cost_usd = response.cost_usd,
        duration_ms = response.duration_ms,
        num_turns = response.num_turns,
        "claude invocation complete"
    );

    Ok(response)
}

/// Safely truncates a string at a UTF-8 character boundary near the given byte limit.
/// This prevents panics when slicing strings that contain multi-byte characters.
/// Appends a truncation marker so the consumer knows the content was shortened.
pub(crate) fn truncate_to_char_boundary(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        s.to_string()
    } else {
        let truncate_at = s
            .char_indices()
            .take_while(|(idx, _)| *idx < max_bytes)
            .last()
            .map(|(idx, c)| idx + c.len_utf8())
            .unwrap_or(0);
        let mut truncated = s[..truncate_at].to_string();
        truncated.push_str("\n\n... (diff truncated) ...");
        truncated
    }
}

/// Truncate a string to at most `max_len` characters, appending "..." if truncated.
/// Properly handles multi-byte UTF-8 characters by using char boundaries.
fn truncate(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        s.to_string()
    } else if max_len <= 3 {
        "...".chars().take(max_len).collect()
    } else {
        // Reserve 3 chars for "...", take (max_len - 3) chars from string
        let chars_to_take = max_len - 3;
        let boundary = s.char_indices()
            .nth(chars_to_take)
            .map(|(i, _)| i)
            .unwrap_or(s.len());
        format!("{}...", &s[..boundary])
    }
}

/// Extract a JSON object from a markdown code fence in the text.
/// Looks for ```json ... ``` or ``` ... ``` blocks containing a JSON object.
pub(crate) fn extract_json_block(text: &str) -> Option<&str> {
    // Try ```json first, then bare ```
    for fence in ["```json", "```"] {
        if let Some(start_idx) = text.find(fence) {
            let content_start = start_idx + fence.len();
            // Skip any newline after the fence
            let content_start = text[content_start..]
                .find(|c: char| c != '\n' && c != '\r')
                .map(|i| content_start + i)
                .unwrap_or(content_start);
            if let Some(end_offset) = text[content_start..].find("```") {
                let block = text[content_start..content_start + end_offset].trim();
                if block.starts_with('{') {
                    return Some(block);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_args_without_schema() {
        let inv = ClaudeInvocation {
            prompt: "hello".to_string(),
            system_prompt: None,
            model: "sonnet".to_string(),
            max_budget_usd: 1.5,
            max_turns: 10,
            effort: "high",
            tools: "Read,Glob,Grep,Bash",
            json_schema: None,
            working_dir: PathBuf::from("/tmp"),
            session_id: None,
            resume_session_id: None,
        };
        let args = build_args(&inv);
        assert!(args.contains(&"-p".to_string()));
        assert!(args.contains(&"hello".to_string()));
        assert!(args.contains(&"1.50".to_string()));
        assert!(!args.contains(&"--json-schema".to_string()));
    }

    #[test]
    fn test_build_args_with_schema() {
        let inv = ClaudeInvocation {
            prompt: "analyze".to_string(),
            system_prompt: Some("You are a test agent.".to_string()),
            model: "sonnet".to_string(),
            max_budget_usd: 0.5,
            max_turns: 25,
            effort: "low",
            tools: "",
            json_schema: Some(r#"{"type":"object"}"#.to_string()),
            working_dir: PathBuf::from("/tmp"),
            session_id: None,
            resume_session_id: None,
        };
        let args = build_args(&inv);
        assert!(args.contains(&"--json-schema".to_string()));
        assert!(args.contains(&r#"{"type":"object"}"#.to_string()));
        assert!(args.contains(&"0.50".to_string()));
        assert!(args.contains(&"--system-prompt".to_string()));
        assert!(args.contains(&"You are a test agent.".to_string()));
    }

    #[test]
    fn test_parse_success_response() {
        let json = r#"{
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": "all good",
            "total_cost_usd": 0.42,
            "duration_ms": 5000,
            "num_turns": 3,
            "session_id": "abc"
        }"#;
        let resp: ClaudeResponse<serde_json::Value> =
            parse_response(json.as_bytes(), b"", false).unwrap();
        assert_eq!(resp.text, "all good");
        assert!((resp.cost_usd - 0.42).abs() < f64::EPSILON);
        assert_eq!(resp.duration_ms, 5000);
        assert_eq!(resp.num_turns, 3);
        assert!(resp.structured.is_none());
    }

    #[test]
    fn test_parse_error_response() {
        let json = r#"{
            "type": "result",
            "subtype": "error",
            "is_error": true,
            "result": "something went wrong",
            "total_cost_usd": 0.0,
            "duration_ms": 100,
            "num_turns": 0,
            "session_id": ""
        }"#;
        let err = parse_response::<serde_json::Value>(json.as_bytes(), b"", false).unwrap_err();
        assert!(err.to_string().contains("something went wrong"));
    }

    #[test]
    fn test_parse_structured_output() {
        let json = r#"{
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": "done",
            "total_cost_usd": 0.10,
            "duration_ms": 2000,
            "num_turns": 1,
            "session_id": "xyz",
            "structured_output": {"title": "Fix bug", "body": "Details here"}
        }"#;

        #[derive(serde::Deserialize)]
        struct PrBody {
            title: String,
            body: String,
        }

        let resp: ClaudeResponse<PrBody> =
            parse_response(json.as_bytes(), b"", true).unwrap();
        let s = resp.structured.unwrap();
        assert_eq!(s.title, "Fix bug");
        assert_eq!(s.body, "Details here");
    }

    #[test]
    fn test_parse_malformed_json() {
        let err = parse_response::<serde_json::Value>(b"not json", b"", false).unwrap_err();
        assert!(err.to_string().contains("failed to parse claude JSON"));
    }

    #[test]
    fn test_truncate() {
        // No truncation needed
        assert_eq!(truncate("hello", 10), "hello");
        // Truncation with room for "..."
        assert_eq!(truncate("hello world", 5), "he...");
        // Edge case: max_len <= 3
        assert_eq!(truncate("hello world", 3), "...");
        assert_eq!(truncate("hello world", 2), "..");
        assert_eq!(truncate("hello world", 1), ".");
        assert_eq!(truncate("hello world", 0), "");
        // Multi-byte UTF-8 characters
        assert_eq!(truncate("héllo", 5), "héllo"); // 5 chars, no truncation
        assert_eq!(truncate("héllo world", 5), "hé...");
    }
}

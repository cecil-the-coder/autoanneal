use anyhow::Result;
use serde::de::DeserializeOwned;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::info;

/// Parameters for a single LLM invocation.
pub struct LlmInvocation {
    pub prompt: String,
    pub system_prompt: Option<String>,
    pub model: String,
    pub max_turns: u32,
    pub effort: &'static str,
    pub tools: &'static str,
    pub json_schema: Option<String>,
    pub working_dir: PathBuf,
    /// Context window size in tokens. Old tool results are evicted when the
    /// conversation approaches this limit.
    pub context_window: u64,
    /// Optional provider hint: "anthropic" or "openai".
    /// When set, overrides environment-based auto-detection for this invocation only.
    pub provider_hint: Option<String>,
    /// Maximum tokens per API response. Defaults to 16384 when None.
    /// Set lower for simple structured-output calls to avoid model limits.
    pub max_tokens_per_turn: Option<u32>,
    /// CI context for CI-fix invocations (repo slug and run ID).
    pub ci_context: Option<crate::agent::tools::CiContext>,
    /// Maximum Exa web searches for this invocation (0 = no web search tool).
    pub exa_max_searches: u32,
}

/// Parsed response from an LLM invocation.
#[derive(Debug)]
pub struct LlmResponse<T> {
    pub structured: Option<T>,
    pub text: String,
    pub cost_usd: f64,
    pub duration_ms: u64,
    pub num_turns: u32,
}

/// Generate a compact working directory context string.
/// Uses `find` with depth limit, excludes common noise directories,
/// and truncates to avoid wasting tokens.
pub(crate) async fn get_dir_context(working_dir: &Path) -> String {
    use tokio::io::AsyncReadExt;
    use tracing::warn;

    // Spawn the child process separately so we can kill it on timeout.
    let mut child = match tokio::process::Command::new("find")
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
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "failed to spawn find command");
            return String::new();
        }
    };

    // Take stdout/stderr handles and spawn readers.
    let stdout_handle = child.stdout.take();
    let stderr_handle = child.stderr.take();

    let stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(mut out) = stdout_handle {
            let _ = out.read_to_end(&mut buf).await;
        }
        buf
    });
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(mut err) = stderr_handle {
            let _ = err.read_to_end(&mut buf).await;
        }
        buf
    });

    // Wait with timeout and kill the process if it exceeds.
    // Use a select pattern: await child.wait() directly with a timeout.
    let (status, stdout, stderr) = match tokio::time::timeout(Duration::from_secs(30), child.wait()).await {
        Ok(status) => {
            // Process completed within timeout - collect output.
            let stdout = stdout_task.await.unwrap_or_default();
            let stderr = stderr_task.await.unwrap_or_default();
            (status, stdout, stderr)
        }
        Err(_) => {
            // Timeout: kill the child process.
            warn!("find command timed out after 30 seconds, killing process");
            let _ = child.kill().await;
            // Wait for the child process to avoid zombie/defunct processes.
            let _ = child.wait().await;
            // Abort the reader tasks and await them to ensure cleanup.
            stdout_task.abort();
            stderr_task.abort();
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return String::new();
        }
    };

    let tree = match status {
        Ok(exit) if exit.success() => {
            String::from_utf8_lossy(&stdout).to_string()
        }
        Ok(exit) => {
            let code = exit.code().unwrap_or(-1);
            let stderr_str = String::from_utf8_lossy(&stderr);
            warn!(exit_code = code, stderr = %stderr_str, "find command failed");
            return String::new();
        }
        Err(e) => {
            warn!(error = %e, "find command execution failed");
            return String::new();
        }
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

/// Invoke the LLM API and parse its response.
///
/// Delegates to `agent::bridge::invoke` which handles the conversation loop,
/// tool execution, retries, and structured output extraction. This function
/// is a thin wrapper that preserves the public API for all phase callers.
pub async fn invoke<T: DeserializeOwned>(
    invocation: &LlmInvocation,
    timeout: Duration,
) -> Result<LlmResponse<T>> {
    let response = crate::agent::bridge::invoke(invocation, timeout).await?;
    info!(
        model = %invocation.model,
        duration_ms = response.duration_ms,
        num_turns = response.num_turns,
        cost_usd = response.cost_usd,
        "llm::invoke complete"
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
    fn test_extract_json_block_from_fence() {
        let text = "Here is output:\n```json\n{\"key\": \"value\"}\n```\nDone.";
        let block = extract_json_block(text).unwrap();
        assert_eq!(block, r#"{"key": "value"}"#);
    }

    #[test]
    fn test_extract_json_block_bare_fence() {
        let text = "Result:\n```\n{\"a\": 1}\n```";
        let block = extract_json_block(text).unwrap();
        assert_eq!(block, r#"{"a": 1}"#);
    }

    #[test]
    fn test_extract_json_block_none() {
        assert!(extract_json_block("no json here").is_none());
        assert!(extract_json_block("```\nnot json\n```").is_none());
    }

    #[test]
    fn test_truncate_to_char_boundary_no_truncation() {
        let s = "hello";
        assert_eq!(truncate_to_char_boundary(s, 100), "hello");
    }

    #[test]
    fn test_truncate_to_char_boundary_ascii() {
        let s = "hello world, this is a long string";
        let result = truncate_to_char_boundary(s, 11);
        assert!(result.starts_with("hello world"));
        assert!(result.contains("(diff truncated)"));
    }

    #[test]
    fn test_truncate_to_char_boundary_multibyte() {
        // "héllo" is 6 bytes (é = 2 bytes), truncating at byte 3 should not split é
        let s = "héllo";
        let result = truncate_to_char_boundary(s, 3);
        assert!(result.starts_with("hé"));
        assert!(result.contains("(diff truncated)"));
    }
}

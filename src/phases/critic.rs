use crate::claude::{self, ClaudeInvocation};
use crate::models::CriticResult;
use crate::prompts::critic::CRITIC_PROMPT;
use crate::prompts::system::critic_system_prompt;
use anyhow::{Context, Result};
use std::path::Path;
use std::time::Duration;
use tracing::info;

/// Maximum diff length (in characters) sent to the critic.
const MAX_DIFF_CHARS: usize = 50_000;

#[allow(dead_code)]
pub struct CriticOutput {
    pub score: u32,
    pub verdict: String,
    pub summary: String,
    pub cost_usd: f64,
}

pub async fn run(
    clone_path: &Path,
    default_branch: &str,
    model: &str,
    budget: f64,
) -> Result<CriticOutput> {
    // 1. Get the full diff between default branch and HEAD.
    let diff_output = tokio::process::Command::new("git")
        .args(["diff", &format!("{default_branch}..HEAD")])
        .current_dir(clone_path)
        .output()
        .await
        .context("failed to run git diff for critic review")?;

    let mut diff = String::from_utf8_lossy(&diff_output.stdout).to_string();

    // Truncate if too long (use floor_char_boundary to avoid panicking on multi-byte UTF-8).
    if diff.len() > MAX_DIFF_CHARS {
        let truncate_at = diff.floor_char_boundary(MAX_DIFF_CHARS);
        diff.truncate(truncate_at);
        diff.push_str("\n\n... (diff truncated) ...");
    }

    if diff.trim().is_empty() {
        // No diff means nothing to review.
        return Ok(CriticOutput {
            score: 0,
            verdict: "reject".to_string(),
            summary: "No changes found to review.".to_string(),
            cost_usd: 0.0,
        });
    }

    // 2. Build the prompt.
    let prompt = CRITIC_PROMPT.replace("{diff}", &diff);

    // 3. Build the invocation.
    let invocation = ClaudeInvocation {
        prompt,
        system_prompt: Some(critic_system_prompt()),
        model: model.to_string(),
        max_budget_usd: budget,
        max_turns: 30,
        effort: "high",
        tools: "Read,Glob,Grep,Bash",
        json_schema: None,
        working_dir: clone_path.to_path_buf(),
        session_id: None,
        resume_session_id: None,
    };

    // 4. Invoke Claude.
    let response = claude::invoke::<CriticResult>(&invocation, Duration::from_secs(300)).await?;

    let critic = response.structured.unwrap_or(CriticResult {
        score: 5,
        verdict: "needs_work".to_string(),
        summary: "Critic did not return structured output.".to_string(),
    });

    info!(
        score = critic.score,
        verdict = %critic.verdict,
        summary = %critic.summary,
        "critic review complete"
    );

    Ok(CriticOutput {
        score: critic.score,
        verdict: critic.verdict,
        summary: critic.summary,
        cost_usd: response.cost_usd,
    })
}

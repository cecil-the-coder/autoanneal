use crate::claude::{self, ClaudeInvocation};
use crate::models::{OpenPr, ReconResult, RepoInfo, StackInfo};
use crate::prompts::recon::RECON_PROMPT;
use crate::prompts::system::recon_system_prompt;
use crate::retry::gh_json;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::{info, warn};

/// Output produced by the recon phase.
pub struct ReconOutput {
    pub clone_path: PathBuf,
    pub stack_info: StackInfo,
    pub open_prs: Vec<OpenPr>,
    pub arch_summary: String,
    pub cost_usd: f64,
}

/// Run the recon phase: clone the repo, detect the stack, fetch open PRs,
/// and invoke Claude for an architecture summary.
pub async fn run(
    repo_info: &RepoInfo,
    work_dir: &Path,
    model: &str,
    budget: f64,
    setup_command: Option<&str>,
) -> Result<ReconOutput> {
    // 1. Clone the repository.
    let clone_path = clone_repo(repo_info, work_dir).await?;

    // 2. Disable git hooks (security).
    run_git(&clone_path, &["config", "core.hooksPath", "/dev/null"]).await?;

    // 3. Configure git identity.
    run_git(&clone_path, &["config", "user.email", "autoanneal[bot]@users.noreply.github.com"]).await?;
    run_git(&clone_path, &["config", "user.name", "autoanneal[bot]"]).await?;

    // 4. Run setup command if provided.
    if let Some(cmd) = setup_command {
        run_setup_command(&clone_path, cmd).await;
    }

    // 5. Detect project stack from well-known files.
    let mut stack_info = detect_stack(&clone_path).await?;

    // 6. Fetch open PRs.
    let open_prs = fetch_open_prs(repo_info).await?;

    // 7. Claude architecture summary.
    let (arch_summary, cost_usd) =
        claude_recon(&clone_path, model, budget, &mut stack_info).await?;

    Ok(ReconOutput {
        clone_path,
        stack_info,
        open_prs,
        arch_summary,
        cost_usd,
    })
}

/// Redact the GitHub token from a string to prevent credential leakage in logs/errors.
fn redact_token(s: &str, token: &str) -> String {
    s.replace(token, "[REDACTED]")
}

/// Clone the repository into `work_dir/<repo_name>`.
async fn clone_repo(repo_info: &RepoInfo, work_dir: &Path) -> Result<PathBuf> {
    let gh_token = std::env::var("GH_TOKEN")
        .or_else(|_| std::env::var("GITHUB_TOKEN"))
        .context("Neither GH_TOKEN nor GITHUB_TOKEN is set")?;

    let clone_url = format!(
        "https://x-access-token:{}@github.com/{}/{}.git",
        gh_token, repo_info.owner, repo_info.name
    );

    let clone_path = work_dir.join(&repo_info.name);

    let mut args = vec!["clone"];
    if repo_info.disk_usage_kb > 500_000 {
        info!(
            disk_usage_kb = repo_info.disk_usage_kb,
            "Large repo detected, using shallow clone"
        );
        args.push("--depth");
        args.push("250");
    }
    args.push(&clone_url);
    let clone_path_str = clone_path.to_string_lossy().to_string();
    args.push(&clone_path_str);

    info!(
        repo = %format!("{}/{}", repo_info.owner, repo_info.name),
        dest = %clone_path.display(),
        "Cloning repository"
    );

    let output = tokio::process::Command::new("git")
        .args(&args)
        .output()
        .await
        .context("Failed to spawn git clone process")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let sanitized_stderr = redact_token(&stderr, &gh_token);
        anyhow::bail!("git clone failed: {sanitized_stderr}");
    }

    info!("Clone complete: {}", clone_path.display());
    Ok(clone_path)
}

/// Run a git command in the given directory.
async fn run_git(dir: &Path, args: &[&str]) -> Result<()> {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .await
        .with_context(|| format!("Failed to run git {}", args.first().unwrap_or(&"")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git {} failed: {stderr}", args.first().unwrap_or(&""));
    }

    Ok(())
}

/// Run an optional setup command. Logs a warning on failure but does not bail.
async fn run_setup_command(clone_path: &Path, cmd: &str) {
    info!(cmd, "Running setup command");

    let result = tokio::time::timeout(
        Duration::from_secs(300),
        tokio::process::Command::new("bash")
            .args(["-c", cmd])
            .current_dir(clone_path)
            .output(),
    )
    .await;

    match result {
        Ok(Ok(output)) if output.status.success() => {
            info!("Setup command completed successfully");
        }
        Ok(Ok(output)) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!(
                exit_code = output.status.code().unwrap_or(-1),
                stderr = %stderr,
                "Setup command failed (continuing anyway)"
            );
        }
        Ok(Err(e)) => {
            warn!(error = %e, "Failed to spawn setup command (continuing anyway)");
        }
        Err(_) => {
            warn!("Setup command timed out after 5 minutes (continuing anyway)");
        }
    }
}

/// Detect the project stack by scanning for well-known files.
async fn detect_stack(clone_path: &Path) -> Result<StackInfo> {
    let checks: &[(&str, &str, &[&str], &[&str], &[&str])] = &[
        (
            "package.json",
            "JavaScript",
            &["npm run build"],
            &["npm test"],
            &["npm run lint"],
        ),
        (
            "Cargo.toml",
            "Rust",
            &["cargo build"],
            &["cargo test"],
            &["cargo clippy"],
        ),
        (
            "go.mod",
            "Go",
            &["go build ./..."],
            &["go test ./..."],
            &["golangci-lint run"],
        ),
        (
            "pyproject.toml",
            "Python",
            &[],
            &["pytest"],
            &["ruff check ."],
        ),
        (
            "setup.py",
            "Python",
            &["python setup.py build"],
            &["pytest"],
            &["ruff check ."],
        ),
        (
            "requirements.txt",
            "Python",
            &[],
            &["pytest"],
            &["ruff check ."],
        ),
        (
            "pom.xml",
            "Java",
            &["mvn compile"],
            &["mvn test"],
            &["mvn checkstyle:check"],
        ),
        (
            "build.gradle",
            "Java",
            &["gradle build"],
            &["gradle test"],
            &["gradle check"],
        ),
        (
            "Gemfile",
            "Ruby",
            &[],
            &["bundle exec rake test"],
            &["bundle exec rubocop"],
        ),
    ];

    let mut primary_language = "Unknown".to_string();
    let mut build_commands: Vec<String> = Vec::new();
    let mut test_commands: Vec<String> = Vec::new();
    let mut lint_commands: Vec<String> = Vec::new();

    for (file, lang, builds, tests, lints) in checks {
        if clone_path.join(file).exists() {
            if primary_language == "Unknown" {
                primary_language = lang.to_string();
            }
            build_commands.extend(builds.iter().map(|s| s.to_string()));
            test_commands.extend(tests.iter().map(|s| s.to_string()));
            lint_commands.extend(lints.iter().map(|s| s.to_string()));
        }
    }

    // Check for CI workflows.
    let workflows_dir = clone_path.join(".github/workflows");
    let has_ci = tokio::fs::try_exists(&workflows_dir)
        .await
        .unwrap_or(false);
    let mut ci_files = Vec::new();
    if has_ci {
        if let Ok(mut entries) = tokio::fs::read_dir(&workflows_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if let Some(name) = path.file_name() {
                    let name_str = name.to_string_lossy();
                    if name_str.ends_with(".yml") || name_str.ends_with(".yaml") {
                        ci_files.push(format!(".github/workflows/{name_str}"));
                    }
                }
            }
        }
    }

    let stack = StackInfo {
        primary_language,
        build_commands,
        test_commands,
        lint_commands,
        key_directories: Vec::new(),
        has_ci,
        ci_files,
    };

    info!(
        language = %stack.primary_language,
        has_ci = stack.has_ci,
        "Stack detected"
    );

    Ok(stack)
}

/// Fetch open pull requests from the repository.
async fn fetch_open_prs(repo_info: &RepoInfo) -> Result<Vec<OpenPr>> {
    let repo_slug = format!("{}/{}", repo_info.owner, repo_info.name);
    let dot = Path::new(".");

    #[derive(serde::Deserialize)]
    struct GhPr {
        number: u64,
        title: String,
        #[serde(rename = "headRefName")]
        head_ref_name: String,
        #[serde(default)]
        files: Vec<GhPrFile>,
    }

    #[derive(serde::Deserialize)]
    struct GhPrFile {
        path: String,
    }

    let prs: Vec<GhPr> = gh_json(
        dot,
        &[
            "pr",
            "list",
            "-R",
            &repo_slug,
            "--json",
            "number,title,headRefName,files",
            "--limit",
            "50",
        ],
    )
    .await
    .context("Failed to fetch open PRs")?;

    let open_prs: Vec<OpenPr> = prs
        .into_iter()
        .map(|pr| OpenPr {
            number: pr.number,
            title: pr.title,
            head_ref: pr.head_ref_name,
            files: pr.files.into_iter().map(|f| f.path).collect(),
        })
        .collect();

    info!(count = open_prs.len(), "Fetched open PRs");
    Ok(open_prs)
}

/// Invoke Claude for the architecture summary and update stack_info with
/// any more specific commands Claude discovers.
async fn claude_recon(
    clone_path: &Path,
    model: &str,
    budget: f64,
    stack_info: &mut StackInfo,
) -> Result<(String, f64)> {
    let invocation = ClaudeInvocation {
        prompt: RECON_PROMPT.to_string(),
        system_prompt: Some(recon_system_prompt()),
        model: model.to_string(),
        max_budget_usd: budget,
        max_turns: 25,
        effort: "low",
        tools: "Read,Glob,Grep,Bash",
        json_schema: None,
        working_dir: clone_path.to_path_buf(),
        session_id: None,
        resume_session_id: None,
    };

    let timeout = Duration::from_secs(300);
    let response = claude::invoke::<ReconResult>(&invocation, timeout)
        .await
        .context("Claude recon invocation failed")?;

    let cost_usd = response.cost_usd;

    let recon = match response.structured {
        Some(r) => r,
        None => {
            warn!("Claude did not return structured output; using text as summary");
            return Ok((response.text, cost_usd));
        }
    };

    // Update stack_info with Claude's findings if they are more specific.
    if !recon.build_commands.is_empty() {
        stack_info.build_commands = recon.build_commands;
    }
    if !recon.test_commands.is_empty() {
        stack_info.test_commands = recon.test_commands;
    }
    if !recon.lint_commands.is_empty() {
        stack_info.lint_commands = recon.lint_commands;
    }
    if !recon.key_directories.is_empty() {
        stack_info.key_directories = recon.key_directories;
    }
    if !recon.primary_language.is_empty() {
        stack_info.primary_language = recon.primary_language;
    }

    info!(
        cost_usd,
        summary_len = recon.summary.len(),
        "Claude recon complete"
    );

    Ok((recon.summary, cost_usd))
}

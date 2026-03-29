use std::path::PathBuf;

/// RAII guard that cleans up GitHub resources (branches, PRs) when dropped on failure.
///
/// Call [`CleanupGuard::disarm`] on successful completion to prevent cleanup.
/// If `keep_on_failure` is set, cleanup is also skipped.
///
/// The cleanup is performed using `spawn_blocking` to avoid blocking the async runtime.
/// If no tokio runtime is available, cleanup falls back to blocking execution.
pub struct CleanupGuard {
    pub repo_dir: PathBuf,
    pub branch_name: Option<String>,
    pub pr_number: Option<u64>,
    pub repo_slug: String,
    pub keep_on_failure: bool,
    pub has_successful_tasks: bool,
    disarmed: bool,
}

impl CleanupGuard {
    pub fn new(repo_dir: PathBuf, repo_slug: String, keep_on_failure: bool) -> Self {
        Self {
            repo_dir,
            branch_name: None,
            pr_number: None,
            repo_slug,
            keep_on_failure,
            has_successful_tasks: false,
            disarmed: false,
        }
    }

    /// Call this on successful completion to prevent cleanup.
    pub fn disarm(&mut self) {
        self.disarmed = true;
    }

    /// Best-effort cleanup of GitHub resources. Never panics.
    /// Uses `spawn_blocking` to avoid blocking the async runtime when available.
    ///
    /// Performs validation to avoid race conditions and inconsistent state:
    /// - Verifies `repo_dir` still exists before running git commands.
    /// - Validates that the PR's head branch matches `branch_name` before closing,
    ///   to prevent closing an unrelated PR if state becomes inconsistent.
    fn cleanup(&self) {
        // Clone values needed for the closure since it may outlive self
        let pr_number = self.pr_number;
        let has_successful_tasks = self.has_successful_tasks;
        let repo_slug = self.repo_slug.clone();
        let branch_name = self.branch_name.clone();
        let repo_dir = self.repo_dir.clone();

        let cleanup_fn = move || {
            match (pr_number, has_successful_tasks) {
                // PR exists but no successful tasks -- close it and delete the branch.
                (Some(pr), false) => {
                    tracing::info!(
                        pr_number = pr,
                        repo = %repo_slug,
                        "Closing PR and deleting branch (no successful tasks)"
                    );

                    // Validate that the PR's head branch matches our expected branch_name
                    // to avoid closing an unrelated PR due to inconsistent state.
                    if let Some(ref expected_branch) = branch_name {
                        if !validate_pr_branch(&pr, &repo_slug, expected_branch) {
                            tracing::warn!(
                                pr_number = pr,
                                expected_branch = %expected_branch,
                                "Skipping PR close: head branch mismatch or lookup failed, \
                                 state may be inconsistent"
                            );
                            return;
                        }
                    }

                    let output = std::process::Command::new("gh")
                        .args([
                            "pr",
                            "close",
                            &pr.to_string(),
                            "--delete-branch",
                            "-R",
                            &repo_slug,
                        ])
                        .output();

                    match output {
                        Ok(o) if o.status.success() => {
                            tracing::info!(pr_number = pr, "PR closed and branch deleted");
                        }
                        Ok(o) => {
                            let stderr = String::from_utf8_lossy(&o.stderr);
                            tracing::warn!(
                                pr_number = pr,
                                stderr = %stderr,
                                "Failed to close PR"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                pr_number = pr,
                                error = %e,
                                "Failed to run gh pr close"
                            );
                        }
                    }
                }

                // PR exists with some successful tasks -- leave as draft; partial work has value.
                (Some(pr), true) => {
                    tracing::info!(
                        pr_number = pr,
                        repo = %repo_slug,
                        "Leaving draft PR open (has successful tasks, partial work has value)"
                    );
                }

                // No PR, no successful tasks -- delete the remote branch (lock cleanup).
                (None, false) => {
                    if let Some(branch) = branch_name {
                        // Validate repo_dir still exists before running git commands
                        // to avoid race conditions where the directory was removed concurrently.
                        if !repo_dir.exists() {
                            tracing::warn!(
                                branch = %branch,
                                repo_dir = %repo_dir.display(),
                                "Skipping remote branch deletion: repo_dir no longer exists"
                            );
                            return;
                        }

                        tracing::info!(
                            branch = %branch,
                            repo = %repo_slug,
                            "Deleting remote branch (no PR created, no successful tasks)"
                        );

                        let output = std::process::Command::new("git")
                            .args(["push", "origin", "--delete", &branch])
                            .current_dir(&repo_dir)
                            .output();

                        match output {
                            Ok(o) if o.status.success() => {
                                tracing::info!(branch = %branch, "Remote branch deleted");
                            }
                            Ok(o) => {
                                let stderr = String::from_utf8_lossy(&o.stderr);
                                tracing::warn!(
                                    branch = %branch,
                                    stderr = %stderr,
                                    "Failed to delete remote branch"
                                );
                            }
                            Err(e) => {
                                tracing::warn!(
                                    branch = %branch,
                                    error = %e,
                                    "Failed to run git push --delete"
                                );
                            }
                        }
                    }
                }

                // No PR, but successful tasks -- leave branch for manual recovery.
                (None, true) => {
                    if let Some(branch) = branch_name {
                        tracing::info!(
                            branch = %branch,
                            repo = %repo_slug,
                            "Leaving remote branch (has successful tasks but PR creation failed)"
                        );
                    }
                }
            }
        };

        // Try to use spawn_blocking if we're in a tokio context to avoid blocking the async runtime
        if let Ok(_handle) = tokio::runtime::Handle::try_current() {
            // Use block_in_place to run blocking code without blocking the async runtime
            // This is safe to call from within a tokio runtime and allows the cleanup to complete
            tokio::task::block_in_place(cleanup_fn);
        } else {
            // No tokio runtime available, run blocking (e.g., in tests or non-async context)
            cleanup_fn();
        }
    }
}

/// Validates that the PR's head branch matches the expected branch name.
///
/// Returns `true` if the branches match or if the branch name cannot be determined
/// (in which case we log a warning but still allow the close to proceed only if
/// `expected_branch` is `None`). Returns `false` if there is a definitive mismatch.
fn validate_pr_branch(pr_number: &u64, repo_slug: &str, expected_branch: &str) -> bool {
    let output = match std::process::Command::new("gh")
        .args([
            "pr",
            "view",
            &pr_number.to_string(),
            "--json",
            "headRefName",
            "-R",
            repo_slug,
        ])
        .output()
    {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            tracing::warn!(
                pr_number = pr_number,
                stderr = %stderr,
                "Failed to query PR head branch for validation"
            );
            // If we can't verify, don't proceed — safer to skip than close wrong PR
            return false;
        }
        Err(e) => {
            tracing::warn!(
                pr_number = pr_number,
                error = %e,
                "Failed to run gh pr view for branch validation"
            );
            return false;
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Parse JSON like {"headRefName":"branch-name"}
    match extract_head_ref_name(&stdout) {
        Some(head_branch) => {
            if head_branch == expected_branch {
                true
            } else {
                tracing::warn!(
                    pr_number = pr_number,
                    expected_branch = %expected_branch,
                    actual_branch = %head_branch,
                    "PR head branch does not match expected branch name"
                );
                false
            }
        }
        None => {
            tracing::warn!(
                pr_number = pr_number,
                stdout = %stdout,
                "Could not parse headRefName from gh pr view output"
            );
            false
        }
    }
}

/// Extracts the `headRefName` value from a JSON string like `{"headRefName":"branch-name"}`.
///
/// This uses simple string matching rather than pulling in a JSON parser dependency.
fn extract_head_ref_name(json: &str) -> Option<String> {
    // Look for "headRefName":"<value>"
    let key = r#""headRefName""#;
    let start = json.find(key)?;
    let after_key = &json[start + key.len()..];

    // Skip optional whitespace and colon
    let after_key = after_key.trim_start();
    if !after_key.starts_with(':') {
        return None;
    }
    let after_colon = after_key[1..].trim_start();

    // Extract quoted value
    if !after_colon.starts_with('"') {
        return None;
    }
    let value_start = &after_colon[1..];
    let value_end = value_start.find('"')?;
    Some(value_start[..value_end].to_string())
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        if self.disarmed || self.keep_on_failure {
            return;
        }
        self.cleanup();
    }
}

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

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        if self.disarmed || self.keep_on_failure {
            return;
        }
        self.cleanup();
    }
}

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tracing::info;

/// Manages git worktrees off a canonical clone directory.
#[allow(dead_code)]
pub struct WorktreeManager {
    repo_dir: PathBuf,
}

impl WorktreeManager {
    pub fn new(repo_dir: PathBuf) -> Self {
        Self { repo_dir }
    }

    #[allow(dead_code)]
    pub fn repo_dir(&self) -> &Path {
        &self.repo_dir
    }

    /// Create a worktree from HEAD (same commit as canonical clone).
    pub async fn create_from_head(&self, name: &str) -> Result<PathBuf> {
        let wt_path = match self.repo_dir.parent() {
            Some(parent) => parent.join(format!(".worktree-{name}")),
            None => PathBuf::from(format!("/tmp/.worktree-{name}")),
        };

        // Remove stale worktree if it exists.
        if wt_path.exists() {
            let _ = self.remove(&wt_path).await;
        }

        let output = tokio::process::Command::new("git")
            .args([
                "worktree",
                "add",
                "--detach",
                &wt_path.to_string_lossy(),
                "HEAD",
            ])
            .current_dir(&self.repo_dir)
            .output()
            .await
            .context("failed to create worktree")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("git worktree add failed: {stderr}");
        }

        // Configure git identity in the worktree.
        configure_git_identity(&wt_path).await;

        info!(path = %wt_path.display(), "created git worktree");
        Ok(wt_path)
    }

    /// Create a worktree at a specific remote branch.
    pub async fn create_at_branch(&self, name: &str, remote_branch: &str) -> Result<PathBuf> {
        let wt_path = self.create_from_head(name).await?;

        // Fetch the branch.
        let fetch = tokio::process::Command::new("git")
            .args(["fetch", "origin", remote_branch])
            .current_dir(&wt_path)
            .output()
            .await?;

        if !fetch.status.success() {
            let stderr = String::from_utf8_lossy(&fetch.stderr);
            anyhow::bail!("git fetch failed: {stderr}");
        }

        // Checkout FETCH_HEAD.
        let checkout = tokio::process::Command::new("git")
            .args(["checkout", "FETCH_HEAD"])
            .current_dir(&wt_path)
            .output()
            .await?;

        if !checkout.status.success() {
            let stderr = String::from_utf8_lossy(&checkout.stderr);
            anyhow::bail!("git checkout FETCH_HEAD failed: {stderr}");
        }

        Ok(wt_path)
    }

    /// Remove a worktree.
    pub async fn remove(&self, worktree_path: &Path) -> Result<()> {
        let _ = tokio::process::Command::new("git")
            .args([
                "worktree",
                "remove",
                "--force",
                &worktree_path.to_string_lossy(),
            ])
            .current_dir(&self.repo_dir)
            .output()
            .await;

        // Fallback manual cleanup.
        if worktree_path.exists() {
            let _ = tokio::fs::remove_dir_all(worktree_path).await;
        }

        let _ = tokio::process::Command::new("git")
            .args(["worktree", "prune"])
            .current_dir(&self.repo_dir)
            .output()
            .await;

        info!(path = %worktree_path.display(), "removed git worktree");
        Ok(())
    }
}

/// Configure git identity in a directory.
async fn configure_git_identity(dir: &Path) {
    for (key, val) in [
        ("user.name", "autoanneal[bot]"),
        ("user.email", "autoanneal[bot]@users.noreply.github.com"),
    ] {
        let _ = tokio::process::Command::new("git")
            .args(["config", key, val])
            .current_dir(dir)
            .output()
            .await;
    }
}

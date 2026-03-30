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
        let wt_path = self
            .repo_dir
            .parent()
            .ok_or_else(|| anyhow::anyhow!("repo_dir has no parent directory: {}", self.repo_dir.display()))?
            .join(format!(".worktree-{name}"));

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

        // Git identity is inherited from the parent repo config (set in recon).

        info!(path = %wt_path.display(), "created git worktree");
        Ok(wt_path)
    }

    /// Create a worktree at a specific remote branch.
    pub async fn create_at_branch(&self, name: &str, remote_branch: &str) -> Result<PathBuf> {
        let wt_path = self.create_from_head(name).await?;

        // Fetch the branch with a 60-second timeout.
        let fetch = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            tokio::process::Command::new("git")
                .args(["fetch", "origin", remote_branch])
                .current_dir(&wt_path)
                .output(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("git fetch timed out after 60s"))??;

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

        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            tokio::process::Command::new("git")
                .args(["worktree", "prune"])
                .current_dir(&self.repo_dir)
                .output(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("git worktree prune timed out after 30s"))??;

        info!(path = %worktree_path.display(), "removed git worktree");
        Ok(())
    }
}

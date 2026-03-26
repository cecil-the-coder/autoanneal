use crate::models::DiffReport;
use std::path::Path;
use std::process::Command;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum GuardrailViolation {
    #[error("too many lines changed: {actual} (max {max})")]
    TooManyLinesChanged { actual: usize, max: usize },
    #[error("unauthorized files modified: {files:?}")]
    UnauthorizedFiles { files: Vec<String> },
    #[error("unauthorized file deletions: {files:?}")]
    UnauthorizedDeletion { files: Vec<String> },
    #[error("I/O error: {0}")]
    IoError(String),
}

/// Validate the current working-tree diff against guardrail constraints.
///
/// Runs `git diff --numstat HEAD` in `repo_dir`, parses the output, and checks:
/// 1. Total lines changed (added + removed) does not exceed `max_lines`.
/// 2. Extra files (not in `allowed_files`) do not exceed `max(2, allowed_files.len() / 5)`.
/// 3. If `!allow_deletions`, no files have been deleted.
pub fn validate_diff(
    repo_dir: &Path,
    allowed_files: &[String],
    max_lines: usize,
    allow_deletions: bool,
) -> Result<DiffReport, GuardrailViolation> {
    // --- 1. Run git diff --numstat HEAD ---
    let numstat_output = Command::new("git")
        .args(["diff", "--numstat", "HEAD"])
        .current_dir(repo_dir)
        .output()
        .map_err(|e| GuardrailViolation::IoError(format!("git diff --numstat: {e}")))?;

    let numstat_stdout = String::from_utf8_lossy(&numstat_output.stdout);

    let mut files_changed: Vec<String> = Vec::new();
    let mut lines_added: usize = 0;
    let mut lines_removed: usize = 0;

    // --- 2. Parse each line: <added>\t<removed>\t<filename> ---
    // Binary files show: -\t-\t<filename>
    for line in numstat_stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.splitn(3, '\t').collect();
        if parts.len() < 3 {
            continue;
        }
        let added_str = parts[0];
        let removed_str = parts[1];
        let filename = parts[2].to_string();

        // Binary files use "-" for both added and removed; count as 0 lines.
        let added: usize = added_str.parse().unwrap_or(0);
        let removed: usize = removed_str.parse().unwrap_or(0);

        lines_added += added;
        lines_removed += removed;
        files_changed.push(filename);
    }

    // --- 3. Build extra_files list ---
    let extra_files: Vec<String> = files_changed
        .iter()
        .filter(|f| !allowed_files.contains(f))
        .cloned()
        .collect();

    let report = DiffReport {
        files_changed,
        lines_added,
        lines_removed,
        extra_files: extra_files.clone(),
    };

    // --- 4. Check total lines ---
    let total_lines = lines_added + lines_removed;
    if total_lines > max_lines {
        return Err(GuardrailViolation::TooManyLinesChanged {
            actual: total_lines,
            max: max_lines,
        });
    }

    // --- 5. Check extra_files count ---
    let max_extra = std::cmp::max(2, allowed_files.len() / 5);
    if extra_files.len() > max_extra {
        return Err(GuardrailViolation::UnauthorizedFiles {
            files: extra_files,
        });
    }

    // --- 6. Check deletions ---
    if !allow_deletions {
        let status_output = Command::new("git")
            .args(["diff", "--name-status", "HEAD"])
            .current_dir(repo_dir)
            .output()
            .map_err(|e| GuardrailViolation::IoError(format!("git diff --name-status: {e}")))?;

        let status_stdout = String::from_utf8_lossy(&status_output.stdout);
        let deleted_files: Vec<String> = status_stdout
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                if line.starts_with("D\t") {
                    Some(line[2..].to_string())
                } else {
                    None
                }
            })
            .collect();

        if !deleted_files.is_empty() {
            return Err(GuardrailViolation::UnauthorizedDeletion {
                files: deleted_files,
            });
        }
    }

    Ok(report)
}

/// Discard all working-tree changes by running `git checkout .` and `git clean -fd`.
pub fn discard_changes(repo_dir: &Path) -> anyhow::Result<()> {
    let checkout = Command::new("git")
        .args(["checkout", "."])
        .current_dir(repo_dir)
        .output()?;

    if !checkout.status.success() {
        let stderr = String::from_utf8_lossy(&checkout.stderr);
        anyhow::bail!("git checkout . failed: {stderr}");
    }

    let clean = Command::new("git")
        .args(["clean", "-fd"])
        .current_dir(repo_dir)
        .output()?;

    if !clean.status.success() {
        let stderr = String::from_utf8_lossy(&clean.stderr);
        anyhow::bail!("git clean -fd failed: {stderr}");
    }

    Ok(())
}

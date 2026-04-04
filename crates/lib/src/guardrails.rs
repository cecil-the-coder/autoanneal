use crate::models::DiffReport;
use std::collections::HashSet;
use std::path::Path;
use tokio::process::Command;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum GuardrailViolation {
    #[error("too many lines changed: {actual} (max {max})")]
    TooManyLinesChanged { actual: usize, max: usize },
    #[error("unauthorized files modified: {files:?}")]
    UnauthorizedFiles { files: Vec<String> },
    #[error("unauthorized file deletions: {files:?}")]
    UnauthorizedDeletion { files: Vec<String> },
    #[error("arithmetic overflow counting diff lines: {counter} (value: {value}, addend: {addend})")]
    ArithmeticOverflow { counter: &'static str, value: usize, addend: usize },
    #[error("I/O error: {0}")]
    IoError(String),
}

/// Validate the current working-tree diff against guardrail constraints.
///
/// Runs `git diff --numstat HEAD` in `repo_dir`, parses the output, and checks:
/// 1. Total lines changed (added + removed) does not exceed `max_lines`.
/// 2. Extra files (not in `allowed_files`) do not exceed `max(2, allowed_files.len() / 5)`,
///    or 5 when `allowed_files` is empty.
/// 3. If `!allow_deletions`, no files have been deleted.
pub async fn validate_diff(
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
        .await
        .map_err(|e| GuardrailViolation::IoError(format!("git diff --numstat: {e}")))?;

    if !numstat_output.status.success() {
        // Lossy UTF-8 conversion is acceptable here because:
        // 1. This is an error message for logging/debugging purposes only
        // 2. Git error messages are typically ASCII, but even if not,
        //    we want to capture as much readable text as possible
        let stderr = String::from_utf8_lossy(&numstat_output.stderr);
        return Err(GuardrailViolation::IoError(format!(
            "git diff --numstat failed: {stderr}"
        )));
    }

    // Similarly, lossy conversion is acceptable for git diff output since:
    // 1. Git file paths are typically UTF-8 encoded
    // 2. Even with invalid UTF-8 sequences, we want to process what we can
    // 3. The alternative would be to skip files with non-UTF-8 names entirely
    let numstat_stdout = String::from_utf8_lossy(&numstat_output.stdout);

    let mut files_changed: Vec<String> = Vec::new();
    let mut lines_added: usize = 0;
    let mut lines_removed: usize = 0;

    // --- 2. Parse each line: <added>\t<removed>\t<filename> ---
    // Binary files show: -\t-\t<filename>
    // Expected format: exactly 3 tab-separated fields
    for line in numstat_stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Parse line into exactly 3 parts; skip malformed lines
        let parts: Vec<&str> = line.splitn(3, '\t').collect();
        if parts.len() != 3 {
            // Skip lines that don't match expected format
            continue;
        }
        let added_str = parts[0];
        let removed_str = parts[1];
        let filename = parts[2].to_string();

        // Skip empty filenames (edge case in git output)
        if filename.is_empty() {
            continue;
        }

        // Binary files use "-" for both added and removed; count as 0 lines.
        let added: usize = added_str.parse().unwrap_or_else(|_| {
            tracing::warn!(
                added_str,
                filename,
                "Failed to parse added lines from git diff --numstat, defaulting to 0"
            );
            0
        });
        let removed: usize = removed_str.parse().unwrap_or_else(|_| {
            tracing::warn!(
                removed_str,
                filename,
                "Failed to parse removed lines from git diff --numstat, defaulting to 0"
            );
            0
        });

        lines_added = lines_added.checked_add(added).ok_or(GuardrailViolation::ArithmeticOverflow { counter: "lines_added", value: lines_added, addend: added })?;
        lines_removed = lines_removed.checked_add(removed).ok_or(GuardrailViolation::ArithmeticOverflow { counter: "lines_removed", value: lines_removed, addend: removed })?;
        files_changed.push(filename);
    }

    // --- 3. Build extra_files list ---
    let allowed_set: HashSet<&str> = allowed_files.iter().map(|s| s.as_str()).collect();
    let extra_files: Vec<String> = files_changed
        .iter()
        .filter(|f| !allowed_set.contains(f.as_str()))
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
    // When allowed_files is empty (e.g. issue fixes with no pre-approved files),
    // use a more generous limit so legitimate multi-file fixes aren't rejected.
    let max_extra = if allowed_files.is_empty() {
        5
    } else {
        std::cmp::max(2, allowed_files.len() / 5)
    };
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
            .await
            .map_err(|e| GuardrailViolation::IoError(format!("git diff --name-status: {e}")))?;

        if !status_output.status.success() {
            // Lossy UTF-8 conversion acceptable for error reporting (see above)
            let stderr = String::from_utf8_lossy(&status_output.stderr);
            return Err(GuardrailViolation::IoError(format!(
                "git diff --name-status failed: {stderr}"
            )));
        }

        // Lossy UTF-8 conversion acceptable for git status output (see above)
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
pub async fn discard_changes(repo_dir: &Path) -> anyhow::Result<()> {
    let checkout = Command::new("git")
        .args(["checkout", "."])
        .current_dir(repo_dir)
        .output()
        .await?;

    if !checkout.status.success() {
        // Lossy UTF-8 conversion acceptable for error reporting (see above)
        let stderr = String::from_utf8_lossy(&checkout.stderr);
        anyhow::bail!("git checkout . failed: {stderr}");
    }

    let clean = Command::new("git")
        .args(["clean", "-fd"])
        .current_dir(repo_dir)
        .output()
        .await?;

    if !clean.status.success() {
        // Lossy UTF-8 conversion acceptable for error reporting (see above)
        let stderr = String::from_utf8_lossy(&clean.stderr);
        anyhow::bail!("git clean -fd failed: {stderr}");
    }

    Ok(())
}

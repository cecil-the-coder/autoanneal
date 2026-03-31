use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("file not found: {0}")]
    FileNotFound(String),
    #[error("command failed with exit code {code}:\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}")]
    CommandFailed { code: i32, stdout: String, stderr: String },
    #[error("command timed out after {0} seconds")]
    CommandTimeout(u64),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),
}

// ---------------------------------------------------------------------------
// Tool definition (returned to the model as JSON Schema)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

// ---------------------------------------------------------------------------
// CI context (passed into ToolExecutor for CI-fix phases)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CiContext {
    pub repo_slug: String,
    pub run_id: u64,
}

// ---------------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------------

/// Maximum bytes returned from a single command's stdout before truncation.
const MAX_OUTPUT_BYTES: usize = 128 * 1024;

pub struct ToolExecutor {
    working_dir: PathBuf,
    working_dir_canonical: Option<PathBuf>, // cached canonical path
    command_timeout: Duration,
    ci_context: Option<CiContext>,
    enabled_tools: Option<Vec<String>>,
    /// Optional research tools (web search, vulnerability check, etc.)
    research: Option<super::research_tools::ResearchToolExecutor>,
    /// The tools string from the invocation, used to filter definitions.
    tools_filter: String,
}

impl ToolExecutor {
    pub fn new(
        working_dir: PathBuf,
        command_timeout: Duration,
        ci_context: Option<CiContext>,
        enabled_tools: Option<Vec<String>>,
    ) -> Self {
        Self {
            working_dir,
            working_dir_canonical: None,
            command_timeout,
            ci_context,
            enabled_tools,
            research: None,
            tools_filter: String::new(),
        }
    }

    /// Create an executor with research tool support.
    pub fn new_with_research(
        working_dir: PathBuf,
        command_timeout: Duration,
        ci_context: Option<CiContext>,
        enabled_tools: Option<Vec<String>>,
        exa_api_key: Option<String>,
        exa_max_searches: u32,
        repo_slug: Option<String>,
        tools_filter: String,
    ) -> Self {
        let research = if exa_max_searches > 0
            || tools_filter.contains("CheckVulnerability")
            || tools_filter.contains("CheckPackage")
            || tools_filter.contains("SearchIssues")
        {
            Some(super::research_tools::ResearchToolExecutor::new(
                exa_api_key,
                exa_max_searches,
                repo_slug,
            ))
        } else {
            None
        };

        Self {
            working_dir,
            working_dir_canonical: None,
            command_timeout,
            ci_context,
            enabled_tools,
            research,
            tools_filter,
        }
    }

    /// Return accumulated Exa search cost in USD.
    pub fn exa_cost(&self) -> f64 {
        self.research.as_ref().map_or(0.0, |r| r.exa_cost())
    }

    // -- path helpers -------------------------------------------------------

    /// Resolve `raw` against `working_dir` and ensure it stays inside.
    fn safe_path(&mut self, raw: &str) -> Result<PathBuf, ToolError> {
        if raw.is_empty() {
            return Err(ToolError::InvalidInput("path must not be empty".into()));
        }
        
        // Cache working directory path and canonical path upfront to avoid
        // race conditions and borrow checker issues.
        let working_dir = self.working_dir.clone();
        let wd_canonical = self.working_dir_canonicalize()?;
        
        let candidate = if Path::new(raw).is_absolute() {
            PathBuf::from(raw)
        } else {
            working_dir.join(raw)
        };
        
        // Canonicalise as far as existing components allow.  For new files the
        // parent must already exist.
        // Walk up to find the deepest existing ancestor so we can
        // canonicalize it, then re-append the non-existent tail.
        // NOTE: We canonicalize first and store the result to avoid borrowing `self`
        // across the check, which would prevent calling `working_dir_canonicalize()`.
        let canonical_result = candidate.canonicalize();
        let path_existed = canonical_result.is_ok();
        let resolved = if let Ok(canonical) = canonical_result {
            // Verify the canonical path is within working directory before accepting it.
            // Use proper boundary check: canonical path must either be exactly the working
            // directory, or start with "{working_dir}/" to prevent prefix attacks
            // (e.g., /tmp/abcdef being accepted as inside /tmp/abc).
            let wd_str = wd_canonical.to_string_lossy();
            let canonical_str = canonical.to_string_lossy();
            if canonical != wd_canonical && !canonical_str.starts_with(&format!("{}/", wd_str))
            {
                return Err(ToolError::InvalidInput(format!(
                    "path escapes working directory: {raw}"
                )));
            }
            canonical
        } else {
            let mut ancestor = candidate.clone();
            let mut tail_parts: Vec<std::ffi::OsString> = Vec::new();
            loop {
                // Attempt to canonicalize; if it succeeds, we've found our
                // deepest existing ancestor atomically without a separate
                // existence check (avoiding a TOCTOU race).
                match ancestor.canonicalize() {
                    Ok(canonical_ancestor) => {
                        // Re-verify the ancestor is still the same path component
                        // by checking it hasn't been swapped with a symlink.
                        // Use proper boundary check with path separator to prevent
                        // prefix attacks (e.g., /tmp/abcdef being accepted as inside /tmp/abc).
                        let wd_str = wd_canonical.to_string_lossy();
                        let ancestor_str = canonical_ancestor.to_string_lossy();
                        if canonical_ancestor != wd_canonical
                            && !ancestor_str.starts_with(&format!("{}/", wd_str))
                        {
                            return Err(ToolError::InvalidInput(format!(
                                "path escapes working directory: {raw}"
                            )));
                        }
                        let mut base = canonical_ancestor;
                        for part in tail_parts.into_iter().rev() {
                            base = base.join(part);
                        }
                        break base;
                    }
                    Err(_) => {
                        // Ancestor doesn't exist or isn't accessible yet.
                        // Walk up one level.
                        match ancestor.file_name() {
                            Some(part) => {
                                tail_parts.push(part.to_os_string());
                                ancestor = ancestor
                                    .parent()
                                    .ok_or_else(|| {
                                        ToolError::InvalidInput("no parent directory".into())
                                    })?
                                    .to_path_buf();
                            }
                            None => {
                                // Reached root without finding an existing ancestor.
                                return Err(ToolError::InvalidInput(format!(
                                    "cannot canonicalize path: no existing ancestor for {raw}"
                                )));
                            }
                        }
                    }
                }
            }
        };

        // In the non-canonical case, the tail may contain symlinks created
        // during the walk. If the path now exists (created concurrently),
        // re-canonicalize and verify; if not, we can only validate the
        // ancestor bound.
        if !path_existed {
            // Path didn't exist when we started; if it exists now, re-verify.
            if let Ok(canonical_now) = resolved.canonicalize() {
                let wd_str = wd_canonical.to_string_lossy();
                let canonical_now_str = canonical_now.to_string_lossy();
                if canonical_now != wd_canonical
                    && !canonical_now_str.starts_with(&format!("{}/", wd_str))
                {
                    return Err(ToolError::InvalidInput(format!(
                        "path escapes working directory: {raw}"
                    )));
                }
            }
            // For non-existent paths, verify each reconstructed component stays within bounds.
            // For absolute paths, we need to skip components that are part of wd_canonical
            // and only check the relative portion.
            let relative_path = if candidate.is_absolute() {
                // Get the path relative to the canonical working directory
                match candidate.strip_prefix(&wd_canonical) {
                    Ok(rel) => rel,
                    Err(_) => {
                        // Absolute path that doesn't start with working_dir - reject it
                        return Err(ToolError::InvalidInput(format!(
                            "path escapes working directory: {raw}"
                        )));
                    }
                }
            } else {
                &candidate
            };
            
            let mut check_path = wd_canonical.clone();
            for component in relative_path.components() {
                use std::path::Component;
                match component {
                    Component::Normal(name) => {
                        check_path = check_path.join(name);
                        // Check if this is a symlink that points outside.
                        if let Ok(link_target) = std::fs::read_link(&check_path) {
                            let combined = if link_target.is_absolute() {
                                link_target
                            } else {
                                check_path.parent().unwrap_or(&wd_canonical).join(link_target)
                            };
                            if let Ok(canonical_target) = combined.canonicalize() {
                                let wd_str = wd_canonical.to_string_lossy();
                                let target_str = canonical_target.to_string_lossy();
                                if canonical_target != wd_canonical
                                    && !target_str.starts_with(&format!("{}/", wd_str))
                                {
                                    return Err(ToolError::InvalidInput(format!(
                                        "path escapes working directory: {raw}"
                                    )));
                                }
                            }
                        }
                    }
                    Component::ParentDir => {
                        // Reject paths that would escape via .. components.
                        // Check BEFORE moving up that we're not at the boundary.
                        if !check_path.starts_with(&wd_canonical) || check_path == wd_canonical {
                            return Err(ToolError::InvalidInput(format!(
                                "path escapes working directory: {raw}"
                            )));
                        }
                        check_path = check_path.parent().unwrap_or(&wd_canonical).to_path_buf();
                    }
                    Component::CurDir => {
                        // . is harmless, skip
                    }
                    Component::RootDir | Component::Prefix(_) => {
                        // These shouldn't appear in relative paths, skip
                    }
                }
            }
        }

        // Final validation: ensure resolved path is within working directory.
        // This catches any edge cases where path resolution might have escaped.
        // Use proper boundary check with path separator to prevent prefix attacks.
        let wd_str = wd_canonical.to_string_lossy();
        let resolved_str = resolved.to_string_lossy();
        if resolved != wd_canonical && !resolved_str.starts_with(&format!("{}/", wd_str)) {
            return Err(ToolError::InvalidInput(format!(
                "path escapes working directory: {raw}"
            )));
        }

        Ok(resolved)
    }

    /// Canonicalize the working directory (cached).
    fn working_dir_canonicalize(&mut self) -> Result<PathBuf, ToolError> {
        if let Some(ref cached) = self.working_dir_canonical {
            return Ok(cached.clone());
        }
        let canonical = self.working_dir
            .canonicalize()
            .map_err(|e| ToolError::InvalidInput(format!("cannot canonicalize working directory: {e}")))?;
        self.working_dir_canonical = Some(canonical.clone());
        Ok(canonical)
    }

    /// Re-validate that a path is still inside the working directory after
    /// directory creation, guarding against symlink-based TOCTOU races.
    ///
    /// After `safe_path` resolves a non-existent path, an attacker could
    /// create a symlink in the non-existent portion of the path between the
    /// initial check and a subsequent file write.  Calling this method after
    /// `create_dir_all` re-canonicalizes the (now-existing) parent directory
    /// and verifies the full resolved path still lies within the working
    /// directory.
    fn validate_path_after_dir_creation(&mut self, path: &Path) -> Result<(), ToolError> {
        // Try to fully canonicalize; if the final component doesn't exist yet
        // (e.g. the target file), canonicalize the parent and re-append.
        let resolved = if path.exists() {
            path.canonicalize()
                .map_err(ToolError::IoError)?
        } else {
            let parent = path.parent().filter(|p| p.exists());
            match (parent, path.file_name()) {
                (Some(p), Some(name)) => p.canonicalize()
                    .map_err(ToolError::IoError)?
                    .join(name),
                _ => {
                    // Cannot canonicalize further — return error to maintain TOCTOU protection.
                    return Err(ToolError::InvalidInput(
                        "cannot canonicalize path: no existing parent directory".into(),
                    ));
                }
            }
        };

        let wd_canon = self.working_dir_canonicalize()?;
        let wd_str = wd_canon.to_string_lossy();
        let resolved_str = resolved.to_string_lossy();
        if !resolved.starts_with(&wd_canon) {
            return Err(ToolError::InvalidInput(
                "path escapes working directory after directory creation".into(),
            ));
        }
        // Also ensure proper boundary with path separator.
        if resolved != wd_canon && !resolved_str.starts_with(&format!("{}/", wd_str)) {
            return Err(ToolError::InvalidInput(
                "path escapes working directory after directory creation".into(),
            ));
        }
        Ok(())
    }

    // -- individual tools ---------------------------------------------------

    /// Read a file, optionally slicing by 1-based line offset and limit.
    pub fn read_file(
        &mut self,
        path: &str,
        offset: Option<usize>,
        limit: Option<usize>,
    ) -> Result<String, ToolError> {
        let resolved = self.safe_path(path)?;
        if !resolved.exists() {
            return Err(ToolError::FileNotFound(path.to_string()));
        }
        if resolved.is_dir() {
            return Err(ToolError::InvalidInput(format!(
                "path is a directory, not a file: {path}"
            )));
        }
        let bytes = std::fs::read(&resolved)?;
        let content = String::from_utf8_lossy(&bytes);

        let lines: Vec<&str> = content.lines().collect();
        let start = offset.unwrap_or(0).min(lines.len());
        let count = limit.unwrap_or(lines.len().saturating_sub(start));
        let end = (start + count).min(lines.len());

        if start >= lines.len() {
            return Ok(String::new());
        }

        Ok(lines[start..end].join("\n"))
    }

    /// Write content to a file, creating intermediate directories as needed.
    pub fn write_file(&mut self, path: &str, content: &str) -> Result<(), ToolError> {
        let resolved = self.safe_path(path)?;
        if let Some(parent) = resolved.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Re-validate after directory creation to catch symlink-based TOCTOU
        // races where an attacker places a symlink in the newly created path.
        self.validate_path_after_dir_creation(&resolved)?;
        std::fs::write(&resolved, content)?;
        Ok(())
    }

    /// Replace exactly one occurrence of `old_string` with `new_string`.
    pub fn edit_file(
        &mut self,
        path: &str,
        old_string: &str,
        new_string: &str,
    ) -> Result<(), ToolError> {
        let resolved = self.safe_path(path)?;
        if !resolved.exists() {
            return Err(ToolError::FileNotFound(path.to_string()));
        }
        let content = std::fs::read_to_string(&resolved)?;
        let count = content.matches(old_string).count();
        if count == 0 {
            return Err(ToolError::InvalidInput(format!(
                "old_string not found in {path}"
            )));
        }
        if count > 1 {
            return Err(ToolError::InvalidInput(format!(
                "old_string appears {count} times in {path} (must be unique)"
            )));
        }
        let updated = content.replacen(old_string, new_string, 1);
        std::fs::write(&resolved, updated)?;
        Ok(())
    }

    /// Return file paths matching a glob pattern relative to `base` (or
    /// working_dir if `base` is None).
    pub fn search_files(
        &mut self,
        pattern: &str,
        base: Option<&str>,
    ) -> Result<Vec<String>, ToolError> {
        let root = match base {
            Some(b) => self.safe_path(b)?,
            None => self.working_dir.clone(),
        };
        let full_pattern = root.join(pattern);
        let full_pattern_str = full_pattern.to_string_lossy().to_string();
        let timeout = self.command_timeout;

        let run = move |rt: tokio::runtime::Handle| {
            let pattern = full_pattern_str.clone();
            rt.block_on(async move {
                // Apply timeout to the entire glob operation
                let glob_future = async move {
                    let paths = tokio::task::spawn_blocking(move || {
                        glob::glob(&pattern)
                    })
                    .await
                    .map_err(|e| ToolError::InvalidInput(format!("glob task failed: {e}")))?;

                    let mut results: Vec<String> = Vec::new();
                    let entries = paths.map_err(|e| {
                        ToolError::InvalidInput(format!("invalid glob pattern: {e}"))
                    })?;

                    for entry in entries {
                        match entry {
                            Ok(p) => results.push(p.to_string_lossy().into_owned()),
                            Err(_) => continue,
                        }
                    }
                    results.sort();
                    Ok(results)
                };

                match tokio::time::timeout(timeout, glob_future).await {
                    Ok(result) => result,
                    Err(_) => Err(ToolError::CommandTimeout(timeout.as_secs())),
                }
            })
        };

        let handle = tokio::runtime::Handle::try_current();
        match handle {
            Ok(h) => {
                // Inside a tokio runtime — use block_in_place to allow block_on.
                tokio::task::block_in_place(|| run(h))
            }
            Err(_) => {
                // No runtime active — create a temporary one.
                let rt = tokio::runtime::Runtime::new()
                    .map_err(ToolError::IoError)?;
                run(rt.handle().clone())
            }
        }
    }

    /// Grep for `pattern` (regex) in files under `path`.
    pub fn search_content(
        &mut self,
        pattern: &str,
        path: Option<&str>,
        file_type: Option<&str>,
        case_insensitive: bool,
    ) -> Result<Vec<String>, ToolError> {
        // Validate the regex early.
        regex::Regex::new(pattern).map_err(|e| {
            ToolError::InvalidInput(format!("invalid regex: {e}"))
        })?;

        let search_dir = match path {
            Some(p) => self.safe_path(p)?,
            None => self.working_dir.clone(),
        };

        let timeout = self.command_timeout;
        // Clone pattern and file_type to owned strings to avoid lifetime issues in the closure
        let pattern_owned = pattern.to_owned();
        let file_type_owned = file_type.map(|ft| ft.to_owned());
        // Convert search_dir to string for use in the closure
        let search_dir_str = search_dir.to_string_lossy().into_owned();

        let run = move |rt: tokio::runtime::Handle| {
            rt.block_on(async move {
                let mut cmd = tokio::process::Command::new("grep");
                cmd.arg("-rn"); // recursive, line numbers
                if case_insensitive {
                    cmd.arg("-i");
                }
                cmd.arg("-E");

                // File type filter via --include.
                if let Some(ft) = file_type_owned {
                    cmd.arg("--include").arg(format!("*.{ft}"));
                }

                // -- separates options from pattern, preventing patterns starting
                // with '-' from being interpreted as flags.
                cmd.arg("--").arg(&pattern_owned).arg(&search_dir_str);
                cmd.stdout(std::process::Stdio::piped());
                cmd.stderr(std::process::Stdio::piped());

                let result = tokio::time::timeout(timeout, cmd.output()).await;

                match result {
                    Ok(Ok(output)) => {
                        // grep exits 1 when no matches – that is not an error for us.
                        if !output.status.success() && output.status.code() != Some(1) {
                            return Err(ToolError::CommandFailed {
                                code: output.status.code().unwrap_or(-1),
                                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                            });
                        }

                        let stdout = String::from_utf8_lossy(&output.stdout);
                        let lines: Vec<String> = stdout
                            .lines()
                            .filter(|l| !l.is_empty())
                            .map(|l| l.to_string())
                            .collect();
                        Ok(lines)
                    }
                    Ok(Err(e)) => Err(ToolError::IoError(e)),
                    Err(_) => Err(ToolError::CommandTimeout(timeout.as_secs())),
                }
            })
        };

        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                // Inside a tokio runtime — use block_in_place to allow block_on.
                tokio::task::block_in_place(|| run(handle))
            }
            Err(_) => {
                // No runtime active — create a temporary one.
                let rt = tokio::runtime::Runtime::new()
                    .map_err(ToolError::IoError)?;
                run(rt.handle().clone())
            }
        }
    }

    /// Run an arbitrary shell command inside `working_dir`.
    pub fn run_command(
        &self,
        command: &str,
        timeout: Option<Duration>,
    ) -> Result<String, ToolError> {
        if command.trim().is_empty() {
            return Err(ToolError::InvalidInput(
                "command must not be empty".into(),
            ));
        }

        let effective_timeout = timeout.unwrap_or(self.command_timeout);
        let timeout_secs = effective_timeout.as_secs();
        let working_dir = self.working_dir.clone();

        // Run the command using tokio::process::Command + tokio::time::timeout
        // so we can reliably kill the child on timeout.
        let run = move |rt: tokio::runtime::Handle| {
            rt.block_on(async move {
                use tokio::io::AsyncReadExt;

                let mut cmd = tokio::process::Command::new("sh");
                cmd.arg("-c")
                    .arg(command)
                    .current_dir(&working_dir)
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped());

                // On Linux, protect the agent from child process memory usage:
                // 1. OOM score 1000 = kernel kills this child first
                // 2. RLIMIT_AS caps virtual address space so malloc/mmap fail
                //    instead of triggering container-wide OOM kill
                #[cfg(target_os = "linux")]
                unsafe {
                    cmd.pre_exec(|| {
                        // Note: This runs in a forked process before exec().
                        // Only async-signal-safe operations are allowed here.
                        // We intentionally ignore errors silently to avoid
                        // potential deadlocks with logging or synchronization.
                        let _ = std::fs::write("/proc/self/oom_score_adj", "1000");

                        // Cap virtual memory at 3 GiB — child gets allocation
                        // failure instead of triggering container OOM kill.
                        const DESIRED_BYTES: u64 = 3 * 1024 * 1024 * 1024;
                        let rlim_val = DESIRED_BYTES as libc::rlim_t;

                        // On 32-bit or unusual platforms rlim_t may be too
                        // narrow to hold the desired limit.  Detect truncation
                        // so we don't silently apply a lower cap.
                        if rlim_val as u64 != DESIRED_BYTES {
                            tracing::warn!(
                                desired_bytes = DESIRED_BYTES,
                                "RLIMIT_AS value overflows rlim_t; \
                                 skipping virtual-memory cap"
                            );
                        } else {
                            let limit = libc::rlimit {
                                rlim_cur: rlim_val,
                                rlim_max: rlim_val,
                            };
                            if libc::setrlimit(libc::RLIMIT_AS, &limit) != 0 {
                                tracing::warn!(
                                    desired_bytes = DESIRED_BYTES,
                                    "setrlimit(RLIMIT_AS) failed; \
                                     virtual-memory cap not applied"
                                );
                            }
                        }

                        Ok(())
                    });
                }

                let mut child = cmd.spawn().map_err(ToolError::IoError)?;

                // Take stdout/stderr handles and spawn readers so `child`
                // remains available for kill() on timeout.
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

                tokio::select! {
                    status = child.wait() => {
                        let stdout_buf = stdout_task.await.unwrap_or_default();
                        let stderr_buf = stderr_task.await.unwrap_or_default();
                        match status {
                            Ok(exit) if !exit.success() => {
                                let mut stderr_str = String::from_utf8_lossy(&stderr_buf).into_owned();

                                // On Unix, detect signal kills (e.g., SIGKILL from OOM killer).
                                #[cfg(unix)]
                                {
                                    use std::os::unix::process::ExitStatusExt;
                                    if let Some(sig) = exit.signal() {
                                        let hint = if sig == 9 {
                                            " (SIGKILL — likely killed by OOM killer due to memory limit)"
                                        } else {
                                            ""
                                        };
                                        stderr_str.push_str(
                                            &format!("\n[process killed by signal {sig}{hint}]")
                                        );
                                    }
                                }

                                Err(ToolError::CommandFailed {
                                    code: exit.code().unwrap_or(-1),
                                    stdout: String::from_utf8_lossy(&stdout_buf).into_owned(),
                                    stderr: stderr_str,
                                })
                            }
                            Ok(_) => {
                                let mut stdout =
                                    String::from_utf8_lossy(&stdout_buf).into_owned();
                                if stdout.len() > MAX_OUTPUT_BYTES {
                                    let truncate_at = stdout
                                        .char_indices()
                                        .take_while(|(i, _)| *i <= MAX_OUTPUT_BYTES)
                                        .last()
                                        .map(|(i, _)| i)
                                        .unwrap_or(0);
                                    stdout.truncate(truncate_at);
                                    stdout.push_str("\n... [output truncated]");
                                }
                                Ok(stdout)
                            }
                            Err(e) => Err(ToolError::IoError(e)),
                        }
                    }
                    _ = tokio::time::sleep(effective_timeout) => {
                        // Timeout: kill the child process.
                        let _ = child.kill().await;
                        // Abort the reader tasks and await them to ensure
                        // they are fully cleaned up and don't leak as
                        // zombie tasks.
                        stdout_task.abort();
                        stderr_task.abort();
                        let _ = stdout_task.await;
                        let _ = stderr_task.await;
                        Err(ToolError::CommandTimeout(timeout_secs))
                    }
                }
            })
        };

        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                // Inside a tokio runtime — use block_in_place to allow block_on.
                tokio::task::block_in_place(|| run(handle))
            }
            Err(_) => {
                // No runtime active — create a temporary one.
                let rt = tokio::runtime::Runtime::new()
                    .map_err(ToolError::IoError)?;
                run(rt.handle().clone())
            }
        }
    }

    /// Run a read-only git command inside `working_dir`.
    ///
    /// Only an allowlist of safe, read-only subcommands is permitted:
    /// status, diff, log, show, rev-parse.
    ///
    /// The command is executed directly via `git` (no shell) to prevent
    /// injection through shell metacharacters like backticks or `$()`.
    pub fn git(&self, command: &str) -> Result<String, ToolError> {
        let trimmed = command.trim();
        let args: Vec<&str> = trimmed.split_whitespace().collect();

        // Accept both "git diff ..." and "diff ..."
        let (subcommand, git_args) = if args.first() == Some(&"git") {
            (args.get(1).copied().unwrap_or(""), &args[2..])
        } else {
            (args.first().copied().unwrap_or(""), &args[1..])
        };

        const ALLOWED: &[&str] = &["status", "diff", "log", "show", "rev-parse"];
        if !ALLOWED.contains(&subcommand) {
            return Err(ToolError::InvalidInput(format!(
                "git {subcommand} is not allowed — only {} are permitted",
                ALLOWED.join(", ")
            )));
        }

        // Execute directly without a shell to prevent injection.
        let working_dir = self.working_dir.clone();
        let subcommand = subcommand.to_string();
        let git_args: Vec<String> = git_args.iter().map(|s| s.to_string()).collect();
        let timeout = self.command_timeout;

        let run = move |rt: tokio::runtime::Handle| {
            rt.block_on(async move {
                let output = tokio::time::timeout(
                    timeout,
                    tokio::process::Command::new("git")
                        .arg(&subcommand)
                        .args(&git_args)
                        .current_dir(&working_dir)
                        .output(),
                )
                .await;

                match output {
                    Ok(Ok(out)) if out.status.success() => {
                        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
                        Ok(stdout)
                    }
                    Ok(Ok(out)) => {
                        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
                        Err(ToolError::CommandFailed {
                            code: out.status.code().unwrap_or(-1),
                            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                            stderr,
                        })
                    }
                    Ok(Err(e)) => Err(ToolError::IoError(e)),
                    Err(_) => Err(ToolError::CommandFailed {
                        code: -1,
                        stdout: String::new(),
                        stderr: format!("git {subcommand} timed out after {}s", timeout.as_secs()),
                    }),
                }
            })
        };

        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                // Inside a tokio runtime — use block_in_place to allow block_on.
                tokio::task::block_in_place(|| run(handle))
            }
            Err(_) => {
                // No runtime active — create a temporary one.
                let rt = tokio::runtime::Runtime::new()
                    .map_err(ToolError::IoError)?;
                run(rt.handle().clone())
            }
        }
    }

    /// Query GitHub Actions workflow data. Requires `ci_context` to be set.
    ///
    /// Actions:
    /// - `job_logs`:    fetch full logs for a specific job ID
    /// - `job_summary`: fetch structured job/step failure summary for the run
    pub fn gh_workflow_logs(&self, action: &str, job_id: Option<u64>) -> Result<String, ToolError> {
        let ctx = self.ci_context.as_ref().ok_or_else(|| {
            ToolError::InvalidInput("gh_workflow_logs requires CI context (run_id and repo_slug)".into())
        })?;
        match action {
            "job_logs" => {
                let jid = job_id.ok_or_else(|| {
                    ToolError::InvalidInput("job_id is required for action 'job_logs'".into())
                })?;
                let cmd = format!(
                    "gh run view {} --log --job {} -R {}",
                    ctx.run_id, jid, ctx.repo_slug
                );
                self.run_command(&cmd, None)
            }
            "job_summary" => {
                let cmd = format!(
                    "gh run view {} --json jobs -R {}",
                    ctx.run_id, ctx.repo_slug
                );
                self.run_command(&cmd, None)
            }
            other => Err(ToolError::InvalidInput(format!(
                "unknown action '{other}' — use 'job_logs' or 'job_summary'"
            ))),
        }
    }

    // -- catalogue ----------------------------------------------------------

    /// Return tool definitions, filtered by `enabled_tools` if set,
    /// including research tools when configured.
    pub fn get_tool_definitions(&self) -> Vec<ToolDefinition> {
        let all = Self::all_tool_definitions();
        let mut defs = match &self.enabled_tools {
            Some(enabled) => all
                .into_iter()
                .filter(|d| enabled.contains(&d.name))
                .collect(),
            None => all,
        };

        // Append research tool definitions when configured.
        if let Some(ref research) = self.research {
            defs.extend(research.tool_definitions(&self.tools_filter));
        }

        defs
    }

    /// Return the full set of tool definitions for the LLM API.
    fn all_tool_definitions() -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "read_file".into(),
                description: "Read a file, optionally slicing by line offset and limit.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path":   { "type": "string", "description": "File path (relative to working dir)" },
                        "offset": { "type": "integer", "description": "0-based line offset" },
                        "limit":  { "type": "integer", "description": "Max lines to return" }
                    },
                    "required": ["path"]
                }),
            },
            ToolDefinition {
                name: "write_file".into(),
                description: "Write content to a file, creating directories as needed.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path":    { "type": "string" },
                        "content": { "type": "string" }
                    },
                    "required": ["path", "content"]
                }),
            },
            ToolDefinition {
                name: "edit_file".into(),
                description: "Replace exactly one occurrence of old_string with new_string.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path":       { "type": "string" },
                        "old_string": { "type": "string" },
                        "new_string": { "type": "string" }
                    },
                    "required": ["path", "old_string", "new_string"]
                }),
            },
            ToolDefinition {
                name: "search_files".into(),
                description: "Find files matching a glob pattern.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string" },
                        "path":    { "type": "string", "description": "Base directory" }
                    },
                    "required": ["pattern"]
                }),
            },
            ToolDefinition {
                name: "search_content".into(),
                description: "Grep for a regex pattern in files.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern":          { "type": "string" },
                        "path":             { "type": "string" },
                        "type":             { "type": "string", "description": "File extension filter" },
                        "case_insensitive": { "type": "boolean" }
                    },
                    "required": ["pattern"]
                }),
            },
            ToolDefinition {
                name: "run_command".into(),
                description: "Run a shell command in the working directory.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string" },
                        "timeout": { "type": "integer", "description": "Timeout in seconds" }
                    },
                    "required": ["command"]
                }),
            },
            ToolDefinition {
                name: "git".into(),
                description: "Run a read-only git command. Allowed subcommands: status, diff, log, show, rev-parse.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "Full git command, e.g. 'git diff HEAD~1 --stat'" }
                    },
                    "required": ["command"]
                }),
            },
            ToolDefinition {
                name: "gh_workflow_logs".into(),
                description: "Query GitHub Actions workflow data for the current CI run. Use 'job_summary' to see which jobs/steps failed, then 'job_logs' to fetch full logs for a specific job.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "action":  { "type": "string", "enum": ["job_logs", "job_summary"], "description": "What to fetch" },
                        "job_id":  { "type": "integer", "description": "The CI job ID (required for job_logs, ignored for job_summary)" }
                    },
                    "required": ["action"]
                }),
            },
        ]
    }

    // -- dispatch -----------------------------------------------------------

    /// Route a tool call by name to the correct implementation.
    pub async fn execute_tool(&mut self, name: &str, input: &Value) -> Result<String, ToolError> {
        // Check enabled_tools filter.
        if let Some(ref enabled) = self.enabled_tools {
            if !enabled.iter().any(|t| t == name) {
                return Err(ToolError::InvalidInput(format!(
                    "tool '{name}' is not enabled for this invocation"
                )));
            }
        }
        match name {
            "read_file" => {
                let path = input
                    .get("path")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::InvalidInput("missing required field: path".into()))?;
                let offset = input.get("offset").and_then(|v| v.as_u64()).map(|v| v as usize);
                let limit = input.get("limit").and_then(|v| v.as_u64()).map(|v| v as usize);
                self.read_file(path, offset, limit)
            }
            "write_file" => {
                let path = input
                    .get("path")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::InvalidInput("missing required field: path".into()))?;
                let content = input
                    .get("content")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        ToolError::InvalidInput("missing required field: content".into())
                    })?;
                self.write_file(path, content)?;
                Ok("ok".into())
            }
            "edit_file" => {
                let path = input
                    .get("path")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::InvalidInput("missing required field: path".into()))?;
                let old_string = input
                    .get("old_string")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        ToolError::InvalidInput("missing required field: old_string".into())
                    })?;
                let new_string = input
                    .get("new_string")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        ToolError::InvalidInput("missing required field: new_string".into())
                    })?;
                self.edit_file(path, old_string, new_string)?;
                Ok("ok".into())
            }
            "search_files" => {
                let pattern = input
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        ToolError::InvalidInput("missing required field: pattern".into())
                    })?;
                let path = input.get("path").and_then(|v| v.as_str());
                let results = self.search_files(pattern, path)?;
                Ok(results.join("\n"))
            }
            "search_content" => {
                let pattern = input
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        ToolError::InvalidInput("missing required field: pattern".into())
                    })?;
                let path = input.get("path").and_then(|v| v.as_str());
                let file_type = input.get("type").and_then(|v| v.as_str());
                let case_insensitive = input
                    .get("case_insensitive")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let results = self.search_content(pattern, path, file_type, case_insensitive)?;
                Ok(results.join("\n"))
            }
            "run_command" => {
                let command = input
                    .get("command")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        ToolError::InvalidInput("missing required field: command".into())
                    })?;
                let timeout = input
                    .get("timeout")
                    .and_then(|v| v.as_u64())
                    .map(Duration::from_secs);
                self.run_command(command, timeout)
            }
            "git" => {
                let command = input
                    .get("command")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        ToolError::InvalidInput("missing required field: command".into())
                    })?;
                self.git(command)
            }
            "gh_workflow_logs" => {
                let action = input
                    .get("action")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        ToolError::InvalidInput("missing required field: action".into())
                    })?;
                let job_id = input.get("job_id").and_then(|v| v.as_u64());
                self.gh_workflow_logs(action, job_id)
            }
            other => {
                // Check if it's a research tool.
                if let Some(ref research) = self.research {
                    if research.handles_tool(other) {
                        return research.execute_tool(other, input).await;
                    }
                }
                Err(ToolError::InvalidInput(format!("unknown tool: {other}")))
            }
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Create a ToolExecutor rooted in a fresh temp directory and return both.
    fn make_executor() -> (ToolExecutor, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let mut exec = ToolExecutor::new(tmp.path().to_path_buf(), Duration::from_secs(30), None, None);
        (exec, tmp)
    }

    // -- read_file ----------------------------------------------------------

    #[test]
    fn read_file_existing() {
        let (mut exec, tmp) = make_executor();
        let file = tmp.path().join("hello.txt");
        fs::write(&file, "line1\nline2\nline3\n").unwrap();

        let content = exec.read_file("hello.txt", None, None).unwrap();
        assert!(content.contains("line1"));
        assert!(content.contains("line3"));
    }

    #[test]
    fn read_file_with_offset_and_limit() {
        let (mut exec, tmp) = make_executor();
        let file = tmp.path().join("data.txt");
        fs::write(&file, "a\nb\nc\nd\ne\n").unwrap();

        let content = exec.read_file("data.txt", Some(1), Some(2)).unwrap();
        assert_eq!(content, "b\nc");
    }

    #[test]
    fn read_file_nonexistent() {
        let (mut exec, _tmp) = make_executor();
        let err = exec.read_file("nope.txt", None, None).unwrap_err();
        assert!(
            matches!(err, ToolError::FileNotFound(_)),
            "expected FileNotFound, got: {err}"
        );
    }

    #[test]
    fn read_file_directory() {
        let (mut exec, tmp) = make_executor();
        fs::create_dir(tmp.path().join("subdir")).unwrap();
        let err = exec.read_file("subdir", None, None).unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(_)),
            "expected InvalidInput for directory, got: {err}"
        );
    }

    #[test]
    fn read_file_binary_lossy() {
        let (mut exec, tmp) = make_executor();
        let file = tmp.path().join("bin.dat");
        fs::write(&file, b"\x80\x81hello\xff").unwrap();

        let content = exec.read_file("bin.dat", None, None).unwrap();
        assert!(content.contains("hello"));
    }

    #[test]
    fn read_file_empty() {
        let (mut exec, tmp) = make_executor();
        fs::write(tmp.path().join("empty.txt"), "").unwrap();
        let content = exec.read_file("empty.txt", None, None).unwrap();
        assert_eq!(content, "");
    }

    #[test]
    fn read_file_offset_beyond_length() {
        let (mut exec, tmp) = make_executor();
        fs::write(tmp.path().join("short.txt"), "one\ntwo\n").unwrap();
        let content = exec.read_file("short.txt", Some(999), None).unwrap();
        assert_eq!(content, "");
    }

    #[test]
    fn read_file_path_traversal_rejected() {
        let (mut exec, _tmp) = make_executor();
        let err = exec
            .read_file("../../../etc/passwd", None, None)
            .unwrap_err();
        // Should be InvalidInput (path escapes) or FileNotFound – either is acceptable.
        assert!(
            matches!(err, ToolError::InvalidInput(_) | ToolError::FileNotFound(_)),
            "expected path traversal rejection, got: {err}"
        );
    }

    // -- write_file ---------------------------------------------------------

    #[test]
    fn write_file_new() {
        let (mut exec, tmp) = make_executor();
        exec.write_file("new.txt", "hello world").unwrap();
        let on_disk = fs::read_to_string(tmp.path().join("new.txt")).unwrap();
        assert_eq!(on_disk, "hello world");
    }

    #[test]
    fn write_file_overwrite() {
        let (mut exec, tmp) = make_executor();
        fs::write(tmp.path().join("over.txt"), "old").unwrap();
        exec.write_file("over.txt", "new").unwrap();
        assert_eq!(
            fs::read_to_string(tmp.path().join("over.txt")).unwrap(),
            "new"
        );
    }

    #[test]
    fn write_file_creates_intermediate_dirs() {
        let (mut exec, tmp) = make_executor();
        exec.write_file("a/b/c/deep.txt", "deep").unwrap();
        assert!(tmp.path().join("a/b/c/deep.txt").exists());
    }

    #[test]
    fn write_file_empty_content() {
        let (mut exec, tmp) = make_executor();
        exec.write_file("blank.txt", "").unwrap();
        assert_eq!(
            fs::read_to_string(tmp.path().join("blank.txt")).unwrap(),
            ""
        );
    }

    #[test]
    fn write_file_outside_working_dir_rejected() {
        let (mut exec, _tmp) = make_executor();
        let err = exec.write_file("/tmp/outside.txt", "bad").unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(_)),
            "expected rejection, got: {err}"
        );
    }

    /// Verify that `write_file` re-validates the path after creating parent
    /// directories, catching a symlink planted in the non-existent tail of
    /// the path (TOCTOU race condition).
    #[test]
    #[cfg(unix)]
    fn write_file_symlink_in_new_path_rejected_after_dir_creation() {
        let (mut exec, tmp) = make_executor();
        // Create an "outside" directory that is NOT inside the working dir.
        let outside = tempfile::tempdir().expect("create outside dir");

        // Create the parent directory inside working_dir so safe_path succeeds.
        let subdir = tmp.path().join("subdir");
        fs::create_dir_all(&subdir).unwrap();

        // Now plant a symlink: subdir/escape -> outside dir
        let link = subdir.join("escape");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();

        // Attempt to write to subdir/escape/target.txt — the path resolves
        // through the symlink to outside the working directory.
        let err = exec
            .write_file("subdir/escape/target.txt", "pwned")
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(_)),
            "write through symlink escaping working_dir should be rejected, got: {err}"
        );

        // Verify the file was NOT created outside.
        assert!(
            !outside.path().join("target.txt").exists(),
            "file must not have been written outside working_dir"
        );
    }

    // -- edit_file ----------------------------------------------------------

    #[test]
    fn edit_file_successful_replacement() {
        let (mut exec, tmp) = make_executor();
        let file = tmp.path().join("src.rs");
        fs::write(&file, "fn main() {\n    println!(\"old\");\n}\n").unwrap();

        exec.edit_file("src.rs", "println!(\"old\")", "println!(\"new\")")
            .unwrap();

        let updated = fs::read_to_string(&file).unwrap();
        assert!(updated.contains("println!(\"new\")"));
        assert!(!updated.contains("println!(\"old\")"));
    }

    #[test]
    fn edit_file_old_string_not_found() {
        let (mut exec, tmp) = make_executor();
        fs::write(tmp.path().join("a.txt"), "hello").unwrap();
        let err = exec.edit_file("a.txt", "missing", "x").unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[test]
    fn edit_file_ambiguous_multiple_matches() {
        let (mut exec, tmp) = make_executor();
        fs::write(tmp.path().join("dup.txt"), "aaa\naaa\n").unwrap();
        let err = exec.edit_file("dup.txt", "aaa", "bbb").unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(_)),
            "expected ambiguity error, got: {err}"
        );
    }

    #[test]
    fn edit_file_delete_by_replacing_with_empty() {
        let (mut exec, tmp) = make_executor();
        let file = tmp.path().join("del.txt");
        fs::write(&file, "keep\nremove_me\nkeep").unwrap();
        exec.edit_file("del.txt", "remove_me\n", "").unwrap();
        let updated = fs::read_to_string(&file).unwrap();
        assert!(!updated.contains("remove_me"));
        assert!(updated.contains("keep"));
    }

    #[test]
    fn edit_file_nonexistent() {
        let (mut exec, _tmp) = make_executor();
        let err = exec.edit_file("nope.txt", "a", "b").unwrap_err();
        assert!(matches!(err, ToolError::FileNotFound(_)));
    }

    #[test]
    fn edit_file_preserves_trailing_newline() {
        let (mut exec, tmp) = make_executor();
        let file = tmp.path().join("nl.txt");
        fs::write(&file, "first\nsecond\n").unwrap();
        exec.edit_file("nl.txt", "first", "replaced").unwrap();
        let updated = fs::read_to_string(&file).unwrap();
        assert!(updated.ends_with('\n'));
        assert_eq!(updated, "replaced\nsecond\n");
    }

    // -- search_files -------------------------------------------------------

    #[test]
    fn search_files_glob_matches() {
        let (mut exec, tmp) = make_executor();
        fs::write(tmp.path().join("a.rs"), "").unwrap();
        fs::write(tmp.path().join("b.rs"), "").unwrap();
        fs::write(tmp.path().join("c.txt"), "").unwrap();

        let results = exec.search_files("*.rs", None).unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|p| p.ends_with(".rs")));
    }

    #[test]
    fn search_files_no_matches() {
        let (mut exec, _tmp) = make_executor();
        let results = exec.search_files("*.zzz", None).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn search_files_recursive_glob() {
        let (mut exec, tmp) = make_executor();
        let nested = tmp.path().join("d1/d2");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("deep.rs"), "").unwrap();

        let results = exec.search_files("**/*.rs", None).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].contains("deep.rs"));
    }

    #[test]
    fn search_files_invalid_pattern() {
        let (mut exec, _tmp) = make_executor();
        let err = exec.search_files("[invalid", None).unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(_)),
            "expected InvalidInput, got: {err}"
        );
    }

    // -- search_content -----------------------------------------------------

    #[test]
    fn search_content_regex_matches() {
        let (mut exec, tmp) = make_executor();
        fs::write(tmp.path().join("haystack.txt"), "foo bar\nbaz quux\nfoo end\n").unwrap();

        let results = exec
            .search_content("foo", None, None, false)
            .unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn search_content_no_matches() {
        let (mut exec, tmp) = make_executor();
        fs::write(tmp.path().join("hay.txt"), "nothing here\n").unwrap();

        let results = exec
            .search_content("zzzzz", None, None, false)
            .unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn search_content_invalid_regex() {
        let (mut exec, _tmp) = make_executor();
        let err = exec
            .search_content("[invalid", None, None, false)
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[test]
    fn search_content_file_type_filter() {
        let (mut exec, tmp) = make_executor();
        fs::write(tmp.path().join("code.rs"), "fn main() {}\n").unwrap();
        fs::write(tmp.path().join("notes.txt"), "fn main() {}\n").unwrap();

        let results = exec
            .search_content("fn main", None, Some("rs"), false)
            .unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].contains("code.rs"));
    }

    #[test]
    fn search_content_case_insensitive() {
        let (mut exec, tmp) = make_executor();
        fs::write(tmp.path().join("mixed.txt"), "Hello HELLO hello\n").unwrap();

        let results = exec
            .search_content("hello", None, None, true)
            .unwrap();
        assert!(!results.is_empty());
    }

    // -- run_command --------------------------------------------------------

    #[test]
    fn run_command_success() {
        let (mut exec, _tmp) = make_executor();
        let output = exec.run_command("echo hello", None).unwrap();
        assert_eq!(output.trim(), "hello");
    }

    #[test]
    fn run_command_failure() {
        let (mut exec, _tmp) = make_executor();
        let err = exec.run_command("false", None).unwrap_err();
        assert!(
            matches!(err, ToolError::CommandFailed { .. }),
            "expected CommandFailed, got: {err}"
        );
    }

    #[test]
    fn run_command_timeout() {
        let (mut exec, _tmp) = make_executor();
        let err = exec
            .run_command("sleep 60", Some(Duration::from_millis(200)))
            .unwrap_err();
        assert!(
            matches!(err, ToolError::CommandTimeout(_)),
            "expected CommandTimeout, got: {err}"
        );
    }

    #[test]
    fn run_command_large_output_truncated() {
        let (mut exec, _tmp) = make_executor();
        // Generate output larger than MAX_OUTPUT_BYTES (128 KiB).
        let output = exec
            .run_command("yes | head -c 200000", None)
            .unwrap();
        assert!(output.len() <= MAX_OUTPUT_BYTES + 30); // +30 for the truncation message
        assert!(output.contains("[output truncated]"));
    }

    #[test]
    fn run_command_empty() {
        let (mut exec, _tmp) = make_executor();
        let err = exec.run_command("", None).unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    // -- get_tool_definitions -----------------------------------------------

    #[test]
    fn tool_definitions_complete() {
        let defs = ToolExecutor::all_tool_definitions();
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"write_file"));
        assert!(names.contains(&"edit_file"));
        assert!(names.contains(&"search_files"));
        assert!(names.contains(&"search_content"));
        assert!(names.contains(&"run_command"));
        assert!(names.contains(&"git"));
        assert!(names.contains(&"gh_workflow_logs"));
    }

    #[test]
    fn tool_definitions_valid_json_schema() {
        let defs = ToolExecutor::all_tool_definitions();
        for def in &defs {
            // Every schema must be an object type with "properties".
            assert_eq!(
                def.input_schema.get("type").and_then(|v| v.as_str()),
                Some("object"),
                "tool {} schema must have type=object",
                def.name
            );
            assert!(
                def.input_schema.get("properties").is_some(),
                "tool {} schema must have properties",
                def.name
            );
        }
    }

    // -- execute_tool dispatch ----------------------------------------------

    #[tokio::test]
    async fn execute_tool_routes_correctly() {
        let (mut exec, tmp) = make_executor();
        fs::write(tmp.path().join("routed.txt"), "content here").unwrap();

        let result = exec
            .execute_tool(
                "read_file",
                &serde_json::json!({ "path": "routed.txt" }),
            )
            .await
            .unwrap();
        assert!(result.contains("content here"));
    }

    #[tokio::test]
    async fn execute_tool_unknown_name() {
        let (mut exec, _tmp) = make_executor();
        let err = exec
            .execute_tool("does_not_exist", &serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(_)),
            "expected InvalidInput for unknown tool, got: {err}"
        );
    }

    #[tokio::test]
    async fn execute_tool_missing_required_field() {
        let (mut exec, _tmp) = make_executor();
        let err = exec
            .execute_tool("read_file", &serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(_)),
            "expected InvalidInput for missing field, got: {err}"
        );
    }

    // ===================================================================
    // Path security edge-case tests
    // ===================================================================

    #[test]
    #[cfg(unix)]
    fn test_symlink_escape() {
        let (mut exec, tmp) = make_executor();
        // Create a symlink inside working_dir that points outside it.
        let outside = tempfile::tempdir().expect("create outside dir");
        let outside_file = outside.path().join("secret.txt");
        fs::write(&outside_file, "top secret").unwrap();

        let link_path = tmp.path().join("escape_link");
        std::os::unix::fs::symlink(&outside_file, &link_path).unwrap();

        let err = exec.read_file("escape_link", None, None).unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(_)),
            "symlink escaping working_dir should be rejected, got: {err}"
        );
    }

    #[test]
    fn test_absolute_path_inside_working_dir() {
        let (mut exec, tmp) = make_executor();
        let file = tmp.path().join("inside.txt");
        fs::write(&file, "I am inside").unwrap();

        // Use the full absolute path.
        let abs = file.to_string_lossy().to_string();
        let content = exec.read_file(&abs, None, None).unwrap();
        assert_eq!(content, "I am inside");
    }

    #[test]
    fn test_path_with_dot_components() {
        let (mut exec, tmp) = make_executor();
        let sub = tmp.path().join("foo");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("bar.txt"), "dot-dot").unwrap();

        let content = exec.read_file("./foo/./bar.txt", None, None).unwrap();
        assert_eq!(content, "dot-dot");
    }

    #[test]
    fn test_double_slash_path() {
        let (mut exec, tmp) = make_executor();
        let sub = tmp.path().join("foo");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("bar.txt"), "double slash").unwrap();

        let content = exec.read_file("foo//bar.txt", None, None).unwrap();
        assert_eq!(content, "double slash");
    }

    #[test]
    fn test_safe_path_boundary() {
        // working_dir is e.g. /tmp/abc; a sibling /tmp/abcdef must be rejected.
        let parent = tempfile::tempdir().expect("create parent dir");
        let wd = parent.path().join("abc");
        fs::create_dir(&wd).unwrap();
        let sibling = parent.path().join("abcdef");
        fs::create_dir(&sibling).unwrap();
        fs::write(sibling.join("secret.txt"), "nope").unwrap();

        let mut exec = ToolExecutor::new(wd, Duration::from_secs(30), None, None);
        let sibling_file = sibling.join("secret.txt");
        let err = exec
            .read_file(&sibling_file.to_string_lossy(), None, None)
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(_)),
            "sibling path sharing prefix should be rejected, got: {err}"
        );
    }

    // ===================================================================
    // read_file edge-case tests
    // ===================================================================

    #[test]
    fn test_read_file_unicode_filename() {
        let (mut exec, tmp) = make_executor();
        let file = tmp.path().join("日本語.txt");
        fs::write(&file, "unicode name works").unwrap();

        let content = exec.read_file("日本語.txt", None, None).unwrap();
        assert_eq!(content, "unicode name works");
    }

    #[test]
    fn test_read_file_no_trailing_newline() {
        let (mut exec, tmp) = make_executor();
        fs::write(tmp.path().join("notail.txt"), "line1\nline2").unwrap();

        let content = exec.read_file("notail.txt", None, None).unwrap();
        assert_eq!(content, "line1\nline2");
    }

    #[test]
    fn test_read_file_offset_0_limit_0() {
        let (mut exec, tmp) = make_executor();
        fs::write(tmp.path().join("some.txt"), "a\nb\nc\n").unwrap();

        let content = exec.read_file("some.txt", Some(0), Some(0)).unwrap();
        assert_eq!(content, "");
    }

    #[test]
    fn test_read_file_limit_exceeds_file() {
        let (mut exec, tmp) = make_executor();
        fs::write(tmp.path().join("short.txt"), "one\ntwo\nthree").unwrap();

        let content = exec.read_file("short.txt", Some(0), Some(9999)).unwrap();
        assert_eq!(content, "one\ntwo\nthree");
    }

    #[test]
    #[cfg(unix)]
    fn test_read_file_readonly_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let (mut exec, tmp) = make_executor();
        let file = tmp.path().join("noperm.txt");
        fs::write(&file, "secret").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();

        let result = exec.read_file("noperm.txt", None, None);
        // Restore permissions so cleanup can remove the file.
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();

        assert!(
            result.is_err(),
            "reading a 0o000 file should produce an IO error"
        );
    }

    // ===================================================================
    // edit_file edge-case tests
    // ===================================================================

    #[test]
    fn test_edit_at_start_of_file() {
        let (mut exec, tmp) = make_executor();
        let file = tmp.path().join("start.txt");
        fs::write(&file, "ABCDEF").unwrap();

        exec.edit_file("start.txt", "ABC", "XYZ").unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "XYZDEF");
    }

    #[test]
    fn test_edit_at_end_of_file() {
        let (mut exec, tmp) = make_executor();
        let file = tmp.path().join("end.txt");
        fs::write(&file, "ABCDEF").unwrap();

        exec.edit_file("end.txt", "DEF", "123").unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "ABC123");
    }

    #[test]
    fn test_edit_spanning_multiple_lines() {
        let (mut exec, tmp) = make_executor();
        let file = tmp.path().join("multi.txt");
        fs::write(&file, "line1\nline2\nline3\n").unwrap();

        exec.edit_file("multi.txt", "line1\nline2", "replaced")
            .unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "replaced\nline3\n");
    }

    #[test]
    fn test_edit_changes_line_count() {
        let (mut exec, tmp) = make_executor();
        let file = tmp.path().join("grow.txt");
        fs::write(&file, "before\noriginal\nafter\n").unwrap();

        exec.edit_file("grow.txt", "original", "one\ntwo\nthree")
            .unwrap();
        let updated = fs::read_to_string(&file).unwrap();
        assert_eq!(updated, "before\none\ntwo\nthree\nafter\n");
    }

    #[test]
    fn test_edit_removes_lines() {
        let (mut exec, tmp) = make_executor();
        let file = tmp.path().join("shrink.txt");
        fs::write(&file, "keep\nremove1\nremove2\nremove3\nkeep2\n").unwrap();

        exec.edit_file("shrink.txt", "remove1\nremove2\nremove3", "single")
            .unwrap();
        let updated = fs::read_to_string(&file).unwrap();
        assert_eq!(updated, "keep\nsingle\nkeep2\n");
    }

    #[test]
    fn test_edit_old_equals_new() {
        let (mut exec, tmp) = make_executor();
        let file = tmp.path().join("noop.txt");
        fs::write(&file, "unchanged content here").unwrap();

        // Same old and new should succeed (it is a valid single match).
        exec.edit_file("noop.txt", "unchanged", "unchanged")
            .unwrap();
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "unchanged content here"
        );
    }

    #[test]
    fn test_edit_empty_old_string() {
        let (mut exec, tmp) = make_executor();
        let file = tmp.path().join("empty_old.txt");
        fs::write(&file, "some content").unwrap();

        // Empty string matches at every position in a non-empty file,
        // so the match count will exceed 1 and be rejected as ambiguous.
        let err = exec
            .edit_file("empty_old.txt", "", "inserted")
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(_)),
            "empty old_string should be rejected, got: {err}"
        );
    }

    #[test]
    fn test_edit_regex_special_chars() {
        let (mut exec, tmp) = make_executor();
        let file = tmp.path().join("regex.txt");
        fs::write(&file, "match this: .*+?()[]\n").unwrap();

        exec.edit_file("regex.txt", ".*+?()[]", "REPLACED")
            .unwrap();
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "match this: REPLACED\n"
        );
    }

    // ===================================================================
    // write_file edge-case tests
    // ===================================================================

    #[test]
    fn test_write_preserves_exact_bytes() {
        let (mut exec, tmp) = make_executor();
        let content = "col1\tcol2\r\nval\x00ue\n";
        exec.write_file("exact.bin", content).unwrap();

        let on_disk = fs::read(tmp.path().join("exact.bin")).unwrap();
        assert_eq!(on_disk, content.as_bytes());
    }

    #[test]
    fn test_write_path_is_existing_directory() {
        let (mut exec, tmp) = make_executor();
        fs::create_dir(tmp.path().join("adir")).unwrap();

        let err = exec.write_file("adir", "contents").unwrap_err();
        assert!(
            matches!(err, ToolError::IoError(_)),
            "writing to an existing directory should produce an IO error, got: {err}"
        );
    }

    #[test]
    fn test_write_unicode_content() {
        let (mut exec, tmp) = make_executor();
        let content = "日本語テスト 🦀 émojis café";
        exec.write_file("uni.txt", content).unwrap();

        let on_disk = fs::read_to_string(tmp.path().join("uni.txt")).unwrap();
        assert_eq!(on_disk, content);
    }

    // ===================================================================
    // run_command edge-case tests
    // ===================================================================

    #[test]
    fn test_command_stderr_only() {
        let (mut exec, _tmp) = make_executor();
        // Command that writes to stderr and exits 0.
        let result = exec.run_command("echo error >&2", None);
        // The command exits 0, so it succeeds. stdout should be empty.
        let output = result.unwrap();
        assert!(
            output.trim().is_empty(),
            "stdout should be empty when only stderr is written, got: {:?}",
            output
        );
    }

    #[test]
    fn test_command_working_dir() {
        let (mut exec, tmp) = make_executor();
        let output = exec.run_command("pwd", None).unwrap();
        let canonical_wd = tmp.path().canonicalize().unwrap();
        assert_eq!(
            output.trim(),
            canonical_wd.to_string_lossy(),
            "pwd should output the working directory"
        );
    }

    #[test]
    fn test_command_background_process() {
        let (mut exec, _tmp) = make_executor();
        // Background process should not block the command from returning.
        // Redirect background stdout/stderr to /dev/null so the shell does not
        // keep the pipe open waiting for it.
        let output = exec
            .run_command("sleep 100 >/dev/null 2>&1 & echo foreground", Some(Duration::from_secs(5)))
            .unwrap();
        assert!(output.contains("foreground"));
    }

    #[test]
    fn test_command_killed_by_signal() {
        let (mut exec, _tmp) = make_executor();
        // A process killed by signal has a non-success exit status.
        let err = exec.run_command("kill -9 $$", None).unwrap_err();
        assert!(
            matches!(err, ToolError::CommandFailed { .. }),
            "signal-killed process should report failure, got: {err}"
        );
    }

    #[test]
    fn test_command_output_exact_max() {
        let (mut exec, _tmp) = make_executor();
        // Generate exactly MAX_OUTPUT_BYTES of output.
        let cmd = format!("head -c {} /dev/zero | tr '\\0' 'A'", MAX_OUTPUT_BYTES);
        let output = exec.run_command(&cmd, None).unwrap();
        assert_eq!(output.len(), MAX_OUTPUT_BYTES);
        assert!(!output.contains("[output truncated]"));
    }

    #[test]
    fn test_command_output_max_plus_one() {
        let (mut exec, _tmp) = make_executor();
        let cmd = format!(
            "head -c {} /dev/zero | tr '\\0' 'B'",
            MAX_OUTPUT_BYTES + 1
        );
        let output = exec.run_command(&cmd, None).unwrap();
        assert!(output.contains("[output truncated]"));
        // The non-truncation-message portion should be exactly MAX_OUTPUT_BYTES.
        let truncated_prefix = &output[..MAX_OUTPUT_BYTES];
        assert!(truncated_prefix.chars().all(|c| c == 'B'));
    }

    #[test]
    fn test_command_invalid_utf8() {
        let (mut exec, _tmp) = make_executor();
        // Use Python to reliably output invalid UTF-8 bytes.
        let output = exec
            .run_command("python3 -c \"import sys; sys.stdout.buffer.write(bytes([0x80, 0x81, 0xfe, 0xff]))\"", None)
            .unwrap();
        // from_utf8_lossy replaces invalid bytes with U+FFFD.
        assert!(
            output.contains('\u{FFFD}'),
            "invalid UTF-8 should be replaced with replacement chars"
        );
    }

    // ===================================================================
    // execute_tool dispatch edge-case tests
    // ===================================================================

    #[tokio::test]
    async fn test_dispatch_wrong_type_for_path() {
        let (mut exec, _tmp) = make_executor();
        let err = exec
            .execute_tool("read_file", &serde_json::json!({ "path": 123 }))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(_)),
            "numeric path should be rejected, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_dispatch_null_required_field() {
        let (mut exec, _tmp) = make_executor();
        let err = exec
            .execute_tool("read_file", &serde_json::json!({ "path": null }))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(_)),
            "null path should be rejected, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_dispatch_extra_unknown_fields() {
        let (mut exec, tmp) = make_executor();
        fs::write(tmp.path().join("extra.txt"), "extra test").unwrap();

        // Unknown fields should be silently ignored.
        let result = exec
            .execute_tool(
                "read_file",
                &serde_json::json!({
                    "path": "extra.txt",
                    "unknown_field": "ignored",
                    "another": 42
                }),
            )
            .await
            .unwrap();
        assert_eq!(result, "extra test");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_dispatch_all_tools() {
        let (mut exec, tmp) = make_executor();
        // Set up a file for tools that need it.
        fs::write(tmp.path().join("dispatch.txt"), "dispatch content").unwrap();

        // read_file
        let r = exec.execute_tool(
            "read_file",
            &serde_json::json!({ "path": "dispatch.txt" }),
        ).await;
        assert!(r.is_ok(), "read_file dispatch failed: {:?}", r);

        // write_file
        let r = exec.execute_tool(
            "write_file",
            &serde_json::json!({ "path": "new_dispatch.txt", "content": "new" }),
        ).await;
        assert!(r.is_ok(), "write_file dispatch failed: {:?}", r);

        // edit_file
        let r = exec.execute_tool(
            "edit_file",
            &serde_json::json!({
                "path": "dispatch.txt",
                "old_string": "dispatch content",
                "new_string": "edited"
            }),
        ).await;
        assert!(r.is_ok(), "edit_file dispatch failed: {:?}", r);

        // search_files
        let r = exec.execute_tool(
            "search_files",
            &serde_json::json!({ "pattern": "*.txt" }),
        ).await;
        assert!(r.is_ok(), "search_files dispatch failed: {:?}", r);

        // search_content
        let r = exec.execute_tool(
            "search_content",
            &serde_json::json!({ "pattern": "edited" }),
        ).await;
        assert!(r.is_ok(), "search_content dispatch failed: {:?}", r);

        // run_command
        let r = exec.execute_tool(
            "run_command",
            &serde_json::json!({ "command": "echo hi" }),
        ).await;
        assert!(r.is_ok(), "run_command dispatch failed: {:?}", r);
    }

    #[tokio::test]
    async fn test_dispatch_boolean_as_path() {
        let (mut exec, _tmp) = make_executor();
        let err = exec
            .execute_tool("read_file", &serde_json::json!({ "path": true }))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(_)),
            "boolean path should be rejected, got: {err}"
        );
    }
}

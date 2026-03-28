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
    #[error("command failed with exit code {code}: {stderr}")]
    CommandFailed { code: i32, stderr: String },
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
// Executor
// ---------------------------------------------------------------------------

/// Maximum bytes returned from a single command's stdout before truncation.
const MAX_OUTPUT_BYTES: usize = 128 * 1024;

pub struct ToolExecutor {
    working_dir: PathBuf,
    command_timeout: Duration,
}

impl ToolExecutor {
    pub fn new(working_dir: PathBuf, command_timeout: Duration) -> Self {
        Self {
            working_dir,
            command_timeout,
        }
    }

    // -- path helpers -------------------------------------------------------

    /// Resolve `raw` against `working_dir` and ensure it stays inside.
    fn safe_path(&self, raw: &str) -> Result<PathBuf, ToolError> {
        if raw.is_empty() {
            return Err(ToolError::InvalidInput("path must not be empty".into()));
        }
        let candidate = if Path::new(raw).is_absolute() {
            PathBuf::from(raw)
        } else {
            self.working_dir.join(raw)
        };
        // Canonicalise as far as existing components allow.  For new files the
        // parent must already exist.
        // Walk up to find the deepest existing ancestor so we can
        // canonicalize it, then re-append the non-existent tail.
        let resolved = if candidate.exists() {
            candidate
                .canonicalize()
                .map_err(|e| ToolError::IoError(e))?
        } else {
            let mut ancestor = candidate.clone();
            let mut tail_parts: Vec<std::ffi::OsString> = Vec::new();
            loop {
                if ancestor.exists() {
                    break;
                }
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
                    None => break,
                }
            }
            let mut base = ancestor.canonicalize().unwrap_or(ancestor);
            for part in tail_parts.into_iter().rev() {
                base = base.join(part);
            }
            base
        };

        let wd_canon = self.working_dir.canonicalize().unwrap_or_else(|_| self.working_dir.clone());
        if !resolved.starts_with(&wd_canon) {
            return Err(ToolError::InvalidInput(format!(
                "path escapes working directory: {raw}"
            )));
        }
        Ok(resolved)
    }

    // -- individual tools ---------------------------------------------------

    /// Read a file, optionally slicing by 1-based line offset and limit.
    pub fn read_file(
        &self,
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
    pub fn write_file(&self, path: &str, content: &str) -> Result<(), ToolError> {
        let resolved = self.safe_path(path)?;
        if let Some(parent) = resolved.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&resolved, content)?;
        Ok(())
    }

    /// Replace exactly one occurrence of `old_string` with `new_string`.
    pub fn edit_file(
        &self,
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
        &self,
        pattern: &str,
        base: Option<&str>,
    ) -> Result<Vec<String>, ToolError> {
        let root = match base {
            Some(b) => self.safe_path(b)?,
            None => self.working_dir.clone(),
        };
        let full_pattern = root.join(pattern);
        let full_pattern_str = full_pattern.to_string_lossy();

        let mut results: Vec<String> = Vec::new();
        let paths = glob::glob(&full_pattern_str).map_err(|e| {
            ToolError::InvalidInput(format!("invalid glob pattern: {e}"))
        })?;
        for entry in paths {
            match entry {
                Ok(p) => results.push(p.to_string_lossy().into_owned()),
                Err(_) => continue,
            }
        }
        results.sort();
        Ok(results)
    }

    /// Grep for `pattern` (regex) in files under `path`.
    pub fn search_content(
        &self,
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

        let mut cmd = std::process::Command::new("grep");
        cmd.arg("-rn"); // recursive, line numbers
        if case_insensitive {
            cmd.arg("-i");
        }
        cmd.arg("-E").arg(pattern);

        // File type filter via --include.
        if let Some(ft) = file_type {
            cmd.arg("--include").arg(format!("*.{ft}"));
        }

        cmd.arg(&search_dir);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let output = cmd.output()?;

        // grep exits 1 when no matches – that is not an error for us.
        if !output.status.success() && output.status.code() != Some(1) {
            return Err(ToolError::CommandFailed {
                code: output.status.code().unwrap_or(-1),
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

        let child = std::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(&self.working_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;

        let timeout_secs = effective_timeout.as_secs();

        // Use wait_with_output via a thread + channel for timeout.
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let result = child.wait_with_output();
            let _ = tx.send(result);
        });

        match rx.recv_timeout(effective_timeout) {
            Ok(Ok(output)) => {
                let _ = handle.join();
                if !output.status.success() {
                    return Err(ToolError::CommandFailed {
                        code: output.status.code().unwrap_or(-1),
                        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                    });
                }
                let mut stdout = String::from_utf8_lossy(&output.stdout).into_owned();
                if stdout.len() > MAX_OUTPUT_BYTES {
                    stdout.truncate(MAX_OUTPUT_BYTES);
                    stdout.push_str("\n... [output truncated]");
                }
                Ok(stdout)
            }
            Ok(Err(e)) => {
                let _ = handle.join();
                Err(ToolError::IoError(e))
            }
            Err(_) => {
                // Timed out – best-effort kill; the spawned thread still owns
                // the child so we cannot kill directly, but the thread will
                // clean up when dropped.
                Err(ToolError::CommandTimeout(timeout_secs))
            }
        }
    }

    // -- catalogue ----------------------------------------------------------

    /// Return the full set of tool definitions for the Claude API.
    pub fn get_tool_definitions() -> Vec<ToolDefinition> {
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
        ]
    }

    // -- dispatch -----------------------------------------------------------

    /// Route a tool call by name to the correct implementation.
    pub fn execute_tool(&self, name: &str, input: &Value) -> Result<String, ToolError> {
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
            other => Err(ToolError::InvalidInput(format!("unknown tool: {other}"))),
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
        let exec = ToolExecutor::new(tmp.path().to_path_buf(), Duration::from_secs(30));
        (exec, tmp)
    }

    // -- read_file ----------------------------------------------------------

    #[test]
    fn read_file_existing() {
        let (exec, tmp) = make_executor();
        let file = tmp.path().join("hello.txt");
        fs::write(&file, "line1\nline2\nline3\n").unwrap();

        let content = exec.read_file("hello.txt", None, None).unwrap();
        assert!(content.contains("line1"));
        assert!(content.contains("line3"));
    }

    #[test]
    fn read_file_with_offset_and_limit() {
        let (exec, tmp) = make_executor();
        let file = tmp.path().join("data.txt");
        fs::write(&file, "a\nb\nc\nd\ne\n").unwrap();

        let content = exec.read_file("data.txt", Some(1), Some(2)).unwrap();
        assert_eq!(content, "b\nc");
    }

    #[test]
    fn read_file_nonexistent() {
        let (exec, _tmp) = make_executor();
        let err = exec.read_file("nope.txt", None, None).unwrap_err();
        assert!(
            matches!(err, ToolError::FileNotFound(_)),
            "expected FileNotFound, got: {err}"
        );
    }

    #[test]
    fn read_file_directory() {
        let (exec, tmp) = make_executor();
        fs::create_dir(tmp.path().join("subdir")).unwrap();
        let err = exec.read_file("subdir", None, None).unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(_)),
            "expected InvalidInput for directory, got: {err}"
        );
    }

    #[test]
    fn read_file_binary_lossy() {
        let (exec, tmp) = make_executor();
        let file = tmp.path().join("bin.dat");
        fs::write(&file, b"\x80\x81hello\xff").unwrap();

        let content = exec.read_file("bin.dat", None, None).unwrap();
        assert!(content.contains("hello"));
    }

    #[test]
    fn read_file_empty() {
        let (exec, tmp) = make_executor();
        fs::write(tmp.path().join("empty.txt"), "").unwrap();
        let content = exec.read_file("empty.txt", None, None).unwrap();
        assert_eq!(content, "");
    }

    #[test]
    fn read_file_offset_beyond_length() {
        let (exec, tmp) = make_executor();
        fs::write(tmp.path().join("short.txt"), "one\ntwo\n").unwrap();
        let content = exec.read_file("short.txt", Some(999), None).unwrap();
        assert_eq!(content, "");
    }

    #[test]
    fn read_file_path_traversal_rejected() {
        let (exec, _tmp) = make_executor();
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
        let (exec, tmp) = make_executor();
        exec.write_file("new.txt", "hello world").unwrap();
        let on_disk = fs::read_to_string(tmp.path().join("new.txt")).unwrap();
        assert_eq!(on_disk, "hello world");
    }

    #[test]
    fn write_file_overwrite() {
        let (exec, tmp) = make_executor();
        fs::write(tmp.path().join("over.txt"), "old").unwrap();
        exec.write_file("over.txt", "new").unwrap();
        assert_eq!(
            fs::read_to_string(tmp.path().join("over.txt")).unwrap(),
            "new"
        );
    }

    #[test]
    fn write_file_creates_intermediate_dirs() {
        let (exec, tmp) = make_executor();
        exec.write_file("a/b/c/deep.txt", "deep").unwrap();
        assert!(tmp.path().join("a/b/c/deep.txt").exists());
    }

    #[test]
    fn write_file_empty_content() {
        let (exec, tmp) = make_executor();
        exec.write_file("blank.txt", "").unwrap();
        assert_eq!(
            fs::read_to_string(tmp.path().join("blank.txt")).unwrap(),
            ""
        );
    }

    #[test]
    fn write_file_outside_working_dir_rejected() {
        let (exec, _tmp) = make_executor();
        let err = exec.write_file("/tmp/outside.txt", "bad").unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(_)),
            "expected rejection, got: {err}"
        );
    }

    // -- edit_file ----------------------------------------------------------

    #[test]
    fn edit_file_successful_replacement() {
        let (exec, tmp) = make_executor();
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
        let (exec, tmp) = make_executor();
        fs::write(tmp.path().join("a.txt"), "hello").unwrap();
        let err = exec.edit_file("a.txt", "missing", "x").unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[test]
    fn edit_file_ambiguous_multiple_matches() {
        let (exec, tmp) = make_executor();
        fs::write(tmp.path().join("dup.txt"), "aaa\naaa\n").unwrap();
        let err = exec.edit_file("dup.txt", "aaa", "bbb").unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(_)),
            "expected ambiguity error, got: {err}"
        );
    }

    #[test]
    fn edit_file_delete_by_replacing_with_empty() {
        let (exec, tmp) = make_executor();
        let file = tmp.path().join("del.txt");
        fs::write(&file, "keep\nremove_me\nkeep").unwrap();
        exec.edit_file("del.txt", "remove_me\n", "").unwrap();
        let updated = fs::read_to_string(&file).unwrap();
        assert!(!updated.contains("remove_me"));
        assert!(updated.contains("keep"));
    }

    #[test]
    fn edit_file_nonexistent() {
        let (exec, _tmp) = make_executor();
        let err = exec.edit_file("nope.txt", "a", "b").unwrap_err();
        assert!(matches!(err, ToolError::FileNotFound(_)));
    }

    #[test]
    fn edit_file_preserves_trailing_newline() {
        let (exec, tmp) = make_executor();
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
        let (exec, tmp) = make_executor();
        fs::write(tmp.path().join("a.rs"), "").unwrap();
        fs::write(tmp.path().join("b.rs"), "").unwrap();
        fs::write(tmp.path().join("c.txt"), "").unwrap();

        let results = exec.search_files("*.rs", None).unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|p| p.ends_with(".rs")));
    }

    #[test]
    fn search_files_no_matches() {
        let (exec, _tmp) = make_executor();
        let results = exec.search_files("*.zzz", None).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn search_files_recursive_glob() {
        let (exec, tmp) = make_executor();
        let nested = tmp.path().join("d1/d2");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("deep.rs"), "").unwrap();

        let results = exec.search_files("**/*.rs", None).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].contains("deep.rs"));
    }

    #[test]
    fn search_files_invalid_pattern() {
        let (exec, _tmp) = make_executor();
        let err = exec.search_files("[invalid", None).unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(_)),
            "expected InvalidInput, got: {err}"
        );
    }

    // -- search_content -----------------------------------------------------

    #[test]
    fn search_content_regex_matches() {
        let (exec, tmp) = make_executor();
        fs::write(tmp.path().join("haystack.txt"), "foo bar\nbaz quux\nfoo end\n").unwrap();

        let results = exec
            .search_content("foo", None, None, false)
            .unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn search_content_no_matches() {
        let (exec, tmp) = make_executor();
        fs::write(tmp.path().join("hay.txt"), "nothing here\n").unwrap();

        let results = exec
            .search_content("zzzzz", None, None, false)
            .unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn search_content_invalid_regex() {
        let (exec, _tmp) = make_executor();
        let err = exec
            .search_content("[invalid", None, None, false)
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[test]
    fn search_content_file_type_filter() {
        let (exec, tmp) = make_executor();
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
        let (exec, tmp) = make_executor();
        fs::write(tmp.path().join("mixed.txt"), "Hello HELLO hello\n").unwrap();

        let results = exec
            .search_content("hello", None, None, true)
            .unwrap();
        assert!(!results.is_empty());
    }

    // -- run_command --------------------------------------------------------

    #[test]
    fn run_command_success() {
        let (exec, _tmp) = make_executor();
        let output = exec.run_command("echo hello", None).unwrap();
        assert_eq!(output.trim(), "hello");
    }

    #[test]
    fn run_command_failure() {
        let (exec, _tmp) = make_executor();
        let err = exec.run_command("false", None).unwrap_err();
        assert!(
            matches!(err, ToolError::CommandFailed { .. }),
            "expected CommandFailed, got: {err}"
        );
    }

    #[test]
    fn run_command_timeout() {
        let (exec, _tmp) = make_executor();
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
        let (exec, _tmp) = make_executor();
        // Generate output larger than MAX_OUTPUT_BYTES (128 KiB).
        let output = exec
            .run_command("yes | head -c 200000", None)
            .unwrap();
        assert!(output.len() <= MAX_OUTPUT_BYTES + 30); // +30 for the truncation message
        assert!(output.contains("[output truncated]"));
    }

    #[test]
    fn run_command_empty() {
        let (exec, _tmp) = make_executor();
        let err = exec.run_command("", None).unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    // -- get_tool_definitions -----------------------------------------------

    #[test]
    fn tool_definitions_complete() {
        let defs = ToolExecutor::get_tool_definitions();
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"write_file"));
        assert!(names.contains(&"edit_file"));
        assert!(names.contains(&"search_files"));
        assert!(names.contains(&"search_content"));
        assert!(names.contains(&"run_command"));
    }

    #[test]
    fn tool_definitions_valid_json_schema() {
        let defs = ToolExecutor::get_tool_definitions();
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

    #[test]
    fn execute_tool_routes_correctly() {
        let (exec, tmp) = make_executor();
        fs::write(tmp.path().join("routed.txt"), "content here").unwrap();

        let result = exec
            .execute_tool(
                "read_file",
                &serde_json::json!({ "path": "routed.txt" }),
            )
            .unwrap();
        assert!(result.contains("content here"));
    }

    #[test]
    fn execute_tool_unknown_name() {
        let (exec, _tmp) = make_executor();
        let err = exec
            .execute_tool("does_not_exist", &serde_json::json!({}))
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(_)),
            "expected InvalidInput for unknown tool, got: {err}"
        );
    }

    #[test]
    fn execute_tool_missing_required_field() {
        let (exec, _tmp) = make_executor();
        let err = exec
            .execute_tool("read_file", &serde_json::json!({}))
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(_)),
            "expected InvalidInput for missing field, got: {err}"
        );
    }
}

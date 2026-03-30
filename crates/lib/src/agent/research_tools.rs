//! Research tools for the critic panel's research agent.
//!
//! Four tools that return pre-digested, verdict-shaped responses:
//! - `WebSearch` — Exa web search
//! - `CheckVulnerability` — OSV.dev vulnerability lookup
//! - `CheckPackage` — Registry status check (crates.io, npm, PyPI)
//! - `SearchIssues` — GitHub issue search via `gh`

use crate::agent::tools::{ToolDefinition, ToolError};
use serde::Deserialize;
use serde_json::Value;
use std::sync::atomic::{AtomicU64, AtomicU32, Ordering};

// ---------------------------------------------------------------------------
// Response size limit
// ---------------------------------------------------------------------------

/// Maximum allowed API response body size in bytes (10 MB).
const MAX_RESPONSE_SIZE: usize = 10 * 1024 * 1024;

/// Check if content length exceeds MAX_RESPONSE_SIZE.
/// Returns an error message if too large, None if acceptable.
fn check_content_length(len: u64) -> Option<ToolError> {
    // Safely handle conversion from u64 to usize for 32-bit targets
    let len_usize = match len.try_into() {
        Ok(n) => n,
        Err(_) => return Some(ToolError::InvalidInput(format!(
            "Response body too large: {len} bytes exceeds the {}-byte limit",
            MAX_RESPONSE_SIZE
        ))),
    };
    
    if len_usize > MAX_RESPONSE_SIZE {
        Some(ToolError::InvalidInput(format!(
            "Response body too large: {len} bytes exceeds the {}-byte limit",
            MAX_RESPONSE_SIZE
        )))
    } else {
        None
    }
}

/// Read a response body as text, enforcing a maximum size limit.
/// Returns a descriptive error if the body exceeds `MAX_RESPONSE_SIZE`.
async fn read_response_text(
    resp: reqwest::Response,
    api_name: &str,
) -> Result<String, ToolError> {
    if let Some(len) = resp.content_length() {
        if let Some(err) = check_content_length(len) {
            return Err(err);
        }
    }
    let body = resp
        .bytes()
        .await
        .map_err(|e| ToolError::InvalidInput(format!("Failed to read {api_name} response body: {e}")))?;
    if let Some(err) = check_content_length(body.len() as u64) {
        return Err(err);
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// Read a response body and deserialize as JSON, enforcing a maximum size limit.
/// Returns a descriptive error if the body exceeds `MAX_RESPONSE_SIZE`.
async fn read_response_json<T: serde::de::DeserializeOwned>(
    resp: reqwest::Response,
    api_name: &str,
) -> Result<T, ToolError> {
    let body = read_response_text(resp, api_name).await?;
    serde_json::from_str(&body)
        .map_err(|e| ToolError::InvalidInput(format!("Failed to parse {api_name} response: {e}")))
}

// ---------------------------------------------------------------------------
// Research tool executor
// ---------------------------------------------------------------------------

/// Holds state for research tools (API keys, counters, repo context).
pub struct ResearchToolExecutor {
    exa_api_key: Option<String>,
    exa_searches_remaining: AtomicU32,
    /// Accumulated Exa search cost in micro-dollars (1 USD = 1_000_000 micro-dollars)
    exa_cost_micro: AtomicU64,
    repo_slug: Option<String>,
    http_client: reqwest::Client,
}

impl ResearchToolExecutor {
    pub fn new(
        exa_api_key: Option<String>,
        exa_max_searches: u32,
        repo_slug: Option<String>,
    ) -> Self {
        Self {
            exa_api_key,
            exa_searches_remaining: AtomicU32::new(exa_max_searches),
            exa_cost_micro: AtomicU64::new(0),
            repo_slug,
            http_client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }

    /// Return accumulated Exa search cost in USD.
    pub fn exa_cost(&self) -> f64 {
        self.exa_cost_micro.load(Ordering::Relaxed) as f64 / 1_000_000.0
    }

    /// Return tool definitions for research tools, filtered by what is requested
    /// in the `tools_filter` string (comma-separated tool names).
    pub fn tool_definitions(&self, tools_filter: &str) -> Vec<ToolDefinition> {
        let mut defs = Vec::new();

        if tools_filter.contains("WebSearch")
            && self.exa_api_key.is_some()
            && self.exa_searches_remaining.load(Ordering::Relaxed) > 0
        {
            defs.push(ToolDefinition {
                name: "web_search".into(),
                description: "Search the web for documentation, best practices, and known issues. Returns pre-formatted findings.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Search query" }
                    },
                    "required": ["query"]
                }),
            });
        }

        if tools_filter.contains("CheckVulnerability") {
            defs.push(ToolDefinition {
                name: "check_vulnerability".into(),
                description: "Check if a package has known security vulnerabilities via OSV.dev.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "Package name" },
                        "ecosystem": { "type": "string", "description": "One of: crates.io, npm, PyPI, Go, Maven" }
                    },
                    "required": ["name", "ecosystem"]
                }),
            });
        }

        if tools_filter.contains("CheckPackage") {
            defs.push(ToolDefinition {
                name: "check_package".into(),
                description: "Check a package's current status, latest version, and deprecation status on its registry.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "Package name" },
                        "ecosystem": { "type": "string", "description": "One of: crates.io, npm, PyPI" }
                    },
                    "required": ["name", "ecosystem"]
                }),
            });
        }

        if tools_filter.contains("SearchIssues") {
            defs.push(ToolDefinition {
                name: "search_issues".into(),
                description: "Search the repository's GitHub issues for related discussions and bug reports.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Search query for issues" }
                    },
                    "required": ["query"]
                }),
            });
        }

        defs
    }

    /// Execute a research tool by name.
    pub async fn execute_tool(&self, name: &str, input: &Value) -> Result<String, ToolError> {
        match name {
            "web_search" => {
                let query = input.get("query").and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::InvalidInput("missing required field: query".into()))?;
                self.web_search(query).await
            }
            "check_vulnerability" => {
                let name = input.get("name").and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::InvalidInput("missing required field: name".into()))?;
                let ecosystem = input.get("ecosystem").and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::InvalidInput("missing required field: ecosystem".into()))?;
                self.check_vulnerability(name, ecosystem).await
            }
            "check_package" => {
                let name = input.get("name").and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::InvalidInput("missing required field: name".into()))?;
                let ecosystem = input.get("ecosystem").and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::InvalidInput("missing required field: ecosystem".into()))?;
                self.check_package(name, ecosystem).await
            }
            "search_issues" => {
                let query = input.get("query").and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::InvalidInput("missing required field: query".into()))?;
                self.search_issues(query).await
            }
            _ => Err(ToolError::InvalidInput(format!("unknown research tool: {name}"))),
        }
    }

    /// Returns true if this executor handles the given tool name.
    pub fn handles_tool(&self, name: &str) -> bool {
        matches!(name, "web_search" | "check_vulnerability" | "check_package" | "search_issues")
    }

    // -----------------------------------------------------------------------
    // WebSearch (Exa)
    // -----------------------------------------------------------------------

    async fn web_search(&self, query: &str) -> Result<String, ToolError> {
        let api_key = self.exa_api_key.as_deref().ok_or_else(|| {
            ToolError::InvalidInput("WebSearch unavailable: EXA_API_KEY not set".into())
        })?;

        // Decrement counter atomically; if already 0, reject.
        let prev = self.exa_searches_remaining.fetch_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |current| if current > 0 { Some(current - 1) } else { None },
        );
        if prev.is_err() {
            return Err(ToolError::InvalidInput(
                "WebSearch limit reached: no searches remaining".into(),
            ));
        }

        let body = serde_json::json!({
            "query": query,
            "type": "auto",
            "numResults": 5,
            "contents": {
                "highlights": { "query": query },
                "text": { "maxCharacters": 300 }
            }
        });

        let result = self.http_client
            .post("https://api.exa.ai/search")
            .header("Authorization", format!("Bearer {api_key}"))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await;

        match result {
            Ok(resp) if resp.status().is_success() => {
                match read_response_json::<ExaSearchResponse>(resp, "Exa").await {
                    Ok(data) => {
                        // Track cost (convert dollars to micro-dollars)
                        if let Some(cost) = &data.cost_dollars {
                            if let Some(total) = cost.total {
                                let micro = (total * 1_000_000.0) as u64;
                                self.exa_cost_micro.fetch_add(micro, Ordering::Relaxed);
                            }
                        }
                        Ok(format_web_search_results(query, &data))
                    }
                    Err(e) => Err(e),
                }
            }
            Ok(resp) => {
                let status = resp.status();
                let body = read_response_text(resp, "Exa").await?;
                Err(ToolError::InvalidInput(format!("Exa API returned {status}: {body}")))
            }
            Err(e) => Err(ToolError::InvalidInput(format!("Failed to reach Exa API: {e}"))),
        }
    }

    // -----------------------------------------------------------------------
    // CheckVulnerability (OSV.dev)
    // -----------------------------------------------------------------------

    async fn check_vulnerability(&self, pkg_name: &str, ecosystem: &str) -> Result<String, ToolError> {
        let valid_ecosystems = ["crates.io", "npm", "PyPI", "Go", "Maven"];
        if !valid_ecosystems.contains(&ecosystem) {
            return Err(ToolError::InvalidInput(format!(
                "Invalid ecosystem '{ecosystem}'. Must be one of: {}",
                valid_ecosystems.join(", ")
            )));
        }

        let body = serde_json::json!({
            "package": {
                "name": pkg_name,
                "ecosystem": ecosystem
            }
        });

        let result = self.http_client
            .post("https://api.osv.dev/v1/query")
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await;

        match result {
            Ok(resp) if resp.status().is_success() => {
                match read_response_json::<OsvResponse>(resp, "OSV").await {
                    Ok(data) => Ok(format_vulnerability_results(pkg_name, ecosystem, &data)),
                    Err(e) => Err(e),
                }
            }
            Ok(resp) => {
                let status = resp.status();
                let body = read_response_text(resp, "OSV").await?;
                Err(ToolError::InvalidInput(format!("OSV API returned {status}: {body}")))
            }
            Err(e) => Err(ToolError::InvalidInput(format!("Failed to reach OSV API: {e}"))),
        }
    }

    // -----------------------------------------------------------------------
    // CheckPackage (crates.io / npm / PyPI)
    // -----------------------------------------------------------------------

    async fn check_package(&self, pkg_name: &str, ecosystem: &str) -> Result<String, ToolError> {
        match ecosystem {
            "crates.io" => self.check_crates_io(pkg_name).await,
            "npm" => self.check_npm(pkg_name).await,
            "PyPI" => self.check_pypi(pkg_name).await,
            other => Err(ToolError::InvalidInput(format!(
                "Unknown ecosystem '{other}' for CheckPackage. Must be one of: crates.io, npm, PyPI"
            ))),
        }
    }

    async fn check_crates_io(
        &self,
        name: &str,
    ) -> Result<String, ToolError> {
        let url = format!("https://crates.io/api/v1/crates/{name}");
        let result = self.http_client
            .get(&url)
            .header(
                "User-Agent",
                "autoanneal/1.0 (https://github.com/cecil-the-coder/autoanneal)",
            )
            .send()
            .await;

        match result {
            Ok(resp) if resp.status().as_u16() == 404 => {
                Ok(format!("NOT FOUND: {name} not found on crates.io"))
            }
            Ok(resp) if resp.status().is_success() => {
                match read_response_json::<CratesIoResponse>(resp, "crates.io").await {
                    Ok(data) => Ok(format_crates_io_result(name, &data)),
                    Err(e) => Err(e),
                }
            }
            Ok(resp) => {
                let status = resp.status();
                Err(ToolError::InvalidInput(format!("crates.io returned {status}")))
            }
            Err(e) => Err(ToolError::InvalidInput(format!("Failed to reach crates.io: {e}"))),
        }
    }

    async fn check_npm(
        &self,
        name: &str,
    ) -> Result<String, ToolError> {
        let url = format!("https://registry.npmjs.org/{name}");
        let result = self.http_client.get(&url).send().await;

        match result {
            Ok(resp) if resp.status().as_u16() == 404 => {
                Ok(format!("NOT FOUND: {name} not found on npm"))
            }
            Ok(resp) if resp.status().is_success() => {
                match read_response_json::<NpmResponse>(resp, "npm").await {
                    Ok(data) => Ok(format_npm_result(name, &data)),
                    Err(e) => Err(e),
                }
            }
            Ok(resp) => {
                let status = resp.status();
                Err(ToolError::InvalidInput(format!("npm returned {status}")))
            }
            Err(e) => Err(ToolError::InvalidInput(format!("Failed to reach npm: {e}"))),
        }
    }

    async fn check_pypi(
        &self,
        name: &str,
    ) -> Result<String, ToolError> {
        let url = format!("https://pypi.org/pypi/{name}/json");
        let result = self.http_client.get(&url).send().await;

        match result {
            Ok(resp) if resp.status().as_u16() == 404 => {
                Ok(format!("NOT FOUND: {name} not found on PyPI"))
            }
            Ok(resp) if resp.status().is_success() => {
                match read_response_json::<PyPiResponse>(resp, "PyPI").await {
                    Ok(data) => Ok(format_pypi_result(name, &data)),
                    Err(e) => Err(e),
                }
            }
            Ok(resp) => {
                let status = resp.status();
                Err(ToolError::InvalidInput(format!("PyPI returned {status}")))
            }
            Err(e) => Err(ToolError::InvalidInput(format!("Failed to reach PyPI: {e}"))),
        }
    }

    // -----------------------------------------------------------------------
    // SearchIssues (gh CLI)
    // -----------------------------------------------------------------------

    async fn search_issues(&self, query: &str) -> Result<String, ToolError> {
        let repo = self.repo_slug.as_deref().ok_or_else(|| {
            ToolError::InvalidInput("SearchIssues unavailable: repository slug not configured".into())
        })?;

        let output = tokio::process::Command::new("gh")
            .args([
                "issue", "list",
                "--repo", repo,
                "--search", query,
                "--limit", "5",
                "--json", "number,title,state,labels,createdAt,body",
            ])
            .output()
            .await;

        match output {
            Ok(out) if out.status.success() => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                match serde_json::from_str::<Vec<GhIssue>>(&stdout) {
                    Ok(issues) => Ok(format_issues_results(query, &issues)),
                    Err(e) => Err(ToolError::InvalidInput(format!("Failed to parse gh output: {e}"))),
                }
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                Err(ToolError::CommandFailed {
                    code: out.status.code().unwrap_or(-1),
                    stdout: String::new(),
                    stderr: stderr.to_string(),
                })
            }
            Err(e) => Err(ToolError::IoError(e)),
        }
    }
}

// ===========================================================================
// API response structs (minimal, just the fields we need)
// ===========================================================================

// -- Exa -------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExaSearchResponse {
    #[serde(default)]
    pub results: Vec<ExaResult>,
    #[serde(default)]
    pub cost_dollars: Option<ExaCost>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ExaCost {
    pub total: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ExaResult {
    pub title: Option<String>,
    pub url: Option<String>,
    pub text: Option<String>,
    #[serde(default)]
    pub highlights: Vec<String>,
}

// -- OSV.dev ---------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct OsvResponse {
    #[serde(default)]
    pub vulns: Vec<OsvVuln>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OsvVuln {
    pub id: Option<String>,
    pub summary: Option<String>,
    #[serde(default)]
    pub severity: Vec<OsvSeverity>,
    #[serde(default)]
    pub affected: Vec<OsvAffected>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OsvSeverity {
    #[serde(rename = "type")]
    #[allow(dead_code)]
    pub severity_type: Option<String>,
    pub score: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OsvAffected {
    #[serde(default)]
    pub ranges: Vec<OsvRange>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OsvRange {
    #[serde(rename = "type")]
    #[allow(dead_code)]
    pub range_type: Option<String>,
    #[serde(default)]
    pub events: Vec<OsvEvent>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OsvEvent {
    pub introduced: Option<String>,
    pub fixed: Option<String>,
}

// -- crates.io -------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct CratesIoResponse {
    #[serde(rename = "crate")]
    pub krate: Option<CratesIoCrate>,
    #[serde(default)]
    pub versions: Vec<CratesIoVersion>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CratesIoCrate {
    pub description: Option<String>,
    pub max_version: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CratesIoVersion {
    pub num: Option<String>,
    #[serde(default)]
    pub yanked: bool,
    #[allow(dead_code)]
    pub updated_at: Option<String>,
}

// -- npm -------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct NpmResponse {
    #[allow(dead_code)]
    pub name: Option<String>,
    pub description: Option<String>,
    #[serde(rename = "dist-tags")]
    pub dist_tags: Option<NpmDistTags>,
    pub time: Option<serde_json::Map<String, Value>>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct NpmDistTags {
    pub latest: Option<String>,
}

// -- PyPI ------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct PyPiResponse {
    pub info: Option<PyPiInfo>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PyPiInfo {
    pub version: Option<String>,
    pub summary: Option<String>,
    #[serde(default)]
    pub yanked: bool,
    pub yanked_reason: Option<String>,
}

// -- GitHub Issues ---------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct GhIssue {
    pub number: Option<u64>,
    pub title: Option<String>,
    pub state: Option<String>,
    #[serde(default)]
    pub labels: Vec<GhLabel>,
    #[serde(rename = "createdAt")]
    pub created_at: Option<String>,
    pub body: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GhLabel {
    pub name: Option<String>,
}

// ===========================================================================
// Formatting functions (pure, testable without network)
// ===========================================================================

pub(crate) fn format_web_search_results(query: &str, data: &ExaSearchResponse) -> String {
    if data.results.is_empty() {
        return format!("NO RESULTS: No relevant web results found for \"{query}\"");
    }

    let mut lines = vec![format!("FINDINGS ({} sources):", data.results.len())];
    for (i, result) in data.results.iter().enumerate() {
        let title = result.title.as_deref().unwrap_or("(untitled)");
        let snippet = if !result.highlights.is_empty() {
            result.highlights.join(" ... ")
        } else {
            result.text.as_deref().unwrap_or("(no text)").to_string()
        };
        let url = result.url.as_deref().unwrap_or("(no url)");
        lines.push(format!("{}. [{}] {}", i + 1, title, snippet));
        lines.push(format!("   URL: {}", url));
    }
    lines.join("\n")
}

pub(crate) fn format_vulnerability_results(
    pkg_name: &str,
    ecosystem: &str,
    data: &OsvResponse,
) -> String {
    if data.vulns.is_empty() {
        return format!("NO KNOWN VULNERABILITIES: {pkg_name} ({ecosystem})");
    }

    let mut lines = vec![format!("VULNERABLE: {pkg_name} ({ecosystem})")];
    for vuln in &data.vulns {
        let id = vuln.id.as_deref().unwrap_or("unknown");
        let summary = vuln.summary.as_deref().unwrap_or("no description");
        let severity = vuln
            .severity
            .first()
            .and_then(|s| s.score.as_deref())
            .unwrap_or("UNKNOWN");
        lines.push(format!("- {id}: {summary} (severity: {severity})"));
    }

    // Collect affected ranges
    let mut ranges = Vec::new();
    for vuln in &data.vulns {
        for affected in &vuln.affected {
            for range in &affected.ranges {
                for event in &range.events {
                    if let Some(intro) = &event.introduced {
                        let fixed = event.fixed.as_deref().unwrap_or("unfixed");
                        ranges.push(format!("{intro}..{fixed}"));
                    }
                }
            }
        }
    }
    if !ranges.is_empty() {
        ranges.dedup();
        lines.push(format!("Affected versions: {}", ranges.join(", ")));
    }

    lines.join("\n")
}

pub(crate) fn format_crates_io_result(name: &str, data: &CratesIoResponse) -> String {
    let krate = match &data.krate {
        Some(k) => k,
        None => return format!("NOT FOUND: {name} not found on crates.io"),
    };

    let version = krate.max_version.as_deref().unwrap_or("unknown");
    let updated = krate.updated_at.as_deref().unwrap_or("unknown");
    let description = krate.description.as_deref().unwrap_or("(no description)");

    // Check if latest version is yanked
    let latest_yanked = data
        .versions
        .first()
        .map(|v| v.yanked)
        .unwrap_or(false);

    if latest_yanked {
        let non_yanked = data
            .versions
            .iter()
            .find(|v| !v.yanked)
            .and_then(|v| v.num.as_deref());
        let mut lines = vec![format!("DEPRECATED: {name}@{version} (crates.io)")];
        lines.push(format!("- Status: yanked"));
        if let Some(nv) = non_yanked {
            lines.push(format!("- Latest non-yanked: {nv}"));
        }
        lines.join("\n")
    } else {
        let mut lines = vec![format!("CURRENT: {name}@{version} (crates.io)")];
        lines.push(format!("- Last updated: {updated}"));
        lines.push(format!("- Description: {description}"));
        lines.join("\n")
    }
}

pub(crate) fn format_npm_result(name: &str, data: &NpmResponse) -> String {
    let version = data
        .dist_tags
        .as_ref()
        .and_then(|d| d.latest.as_deref())
        .unwrap_or("unknown");
    let description = data.description.as_deref().unwrap_or("(no description)");

    let updated = data
        .time
        .as_ref()
        .and_then(|t| t.get("modified"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    let mut lines = vec![format!("CURRENT: {name}@{version} (npm)")];
    lines.push(format!("- Last updated: {updated}"));
    lines.push(format!("- Description: {description}"));
    lines.join("\n")
}

pub(crate) fn format_pypi_result(name: &str, data: &PyPiResponse) -> String {
    let info = match &data.info {
        Some(i) => i,
        None => return format!("NOT FOUND: {name} not found on PyPI"),
    };

    let version = info.version.as_deref().unwrap_or("unknown");
    let summary = info.summary.as_deref().unwrap_or("(no description)");

    if info.yanked {
        let reason = info.yanked_reason.as_deref().unwrap_or("(no reason given)");
        let mut lines = vec![format!("DEPRECATED: {name}@{version} (PyPI)")];
        lines.push(format!("- Status: yanked"));
        lines.push(format!("- Message: \"{reason}\""));
        lines.join("\n")
    } else {
        let mut lines = vec![format!("CURRENT: {name}@{version} (PyPI)")];
        lines.push(format!("- Description: {summary}"));
        lines.join("\n")
    }
}

pub(crate) fn format_issues_results(query: &str, issues: &[GhIssue]) -> String {
    if issues.is_empty() {
        return format!("NO RELATED ISSUES: No issues found matching \"{query}\"");
    }

    let mut lines = vec![format!("RELATED ISSUES ({} found):", issues.len())];
    for (i, issue) in issues.iter().enumerate() {
        let num = issue.number.unwrap_or(0);
        let title = issue.title.as_deref().unwrap_or("(untitled)");
        let state = issue.state.as_deref().unwrap_or("unknown");
        let date = issue.created_at.as_deref().unwrap_or("unknown");

        lines.push(format!(
            "{}. #{} \"{}\" ({}, {})",
            i + 1,
            num,
            title,
            state,
            date
        ));

        let label_names: Vec<&str> = issue
            .labels
            .iter()
            .filter_map(|l| l.name.as_deref())
            .collect();
        if !label_names.is_empty() {
            lines.push(format!("   Labels: {}", label_names.join(", ")));
        }

        if let Some(body) = &issue.body {
            let char_count = body.chars().count();
            let truncated: String = body.chars().take(150).collect();
            let suffix = if char_count > 150 { "..." } else { "" };
            lines.push(format!("   Summary: {truncated}{suffix}"));
        }
    }
    lines.join("\n")
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- WebSearch formatting tests -----------------------------------------

    #[test]
    fn test_format_web_search_results() {
        let data = ExaSearchResponse {
            results: vec![
                ExaResult {
                    title: Some("Rust async patterns".into()),
                    url: Some("https://example.com/rust".into()),
                    text: Some("Some text about async".into()),
                    highlights: vec!["async patterns in Rust".into()],
                },
                ExaResult {
                    title: Some("Tokio guide".into()),
                    url: Some("https://tokio.rs/guide".into()),
                    text: None,
                    highlights: vec!["tokio runtime".into(), "spawn tasks".into()],
                },
                ExaResult {
                    title: Some("Third result".into()),
                    url: Some("https://example.com/3".into()),
                    text: Some("text content".into()),
                    highlights: vec![],
                },
            ],
            cost_dollars: Some(ExaCost { total: Some(0.01) }),
        };

        let result = format_web_search_results("rust async", &data);
        assert!(result.starts_with("FINDINGS (3 sources):"));
        assert!(result.contains("[Rust async patterns]"));
        assert!(result.contains("async patterns in Rust"));
        assert!(result.contains("URL: https://example.com/rust"));
        assert!(result.contains("tokio runtime ... spawn tasks"));
        // Third result uses text since highlights is empty
        assert!(result.contains("text content"));
    }

    #[test]
    fn test_format_web_search_no_results() {
        let data = ExaSearchResponse {
            results: vec![],
            cost_dollars: None,
        };
        let result = format_web_search_results("nonexistent thing", &data);
        assert_eq!(
            result,
            "NO RESULTS: No relevant web results found for \"nonexistent thing\""
        );
    }

    #[test]
    fn test_format_web_search_missing_fields() {
        let data = ExaSearchResponse {
            results: vec![ExaResult {
                title: None,
                url: None,
                text: None,
                highlights: vec![],
            }],
            cost_dollars: None,
        };
        let result = format_web_search_results("test", &data);
        assert!(result.contains("(untitled)"));
        assert!(result.contains("(no text)"));
        assert!(result.contains("(no url)"));
    }

    #[tokio::test]
    async fn test_web_search_limit_reached() {
        let exec = ResearchToolExecutor::new(
            Some("test-key".into()),
            0, // no searches remaining
            None,
        );
        let err = exec.web_search("test").await.unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(_)),
            "expected InvalidInput, got: {err}"
        );
        assert!(err.to_string().contains("limit reached"));
    }

    #[tokio::test]
    async fn test_web_search_no_api_key() {
        let exec = ResearchToolExecutor::new(None, 3, None);
        let err = exec.web_search("test").await.unwrap_err();
        assert!(err.to_string().contains("EXA_API_KEY"));
    }

    #[test]
    fn test_web_search_cost_tracking() {
        let exec = ResearchToolExecutor::new(Some("key".into()), 5, None);
        // Can't do a real HTTP call, but we can test the accumulator
        assert_eq!(exec.exa_cost(), 0.0);
        // Manually add cost via the atomic (50000 micro-dollars = 0.05 USD)
        exec.exa_cost_micro.store(50000, Ordering::Relaxed);
        assert!((exec.exa_cost() - 0.05).abs() < f64::EPSILON);
    }

    // -- CheckVulnerability formatting tests --------------------------------

    #[test]
    fn test_format_vulnerability_found() {
        let data = OsvResponse {
            vulns: vec![
                OsvVuln {
                    id: Some("CVE-2023-1234".into()),
                    summary: Some("Buffer overflow in parser".into()),
                    severity: vec![OsvSeverity {
                        severity_type: Some("CVSS_V3".into()),
                        score: Some("HIGH".into()),
                    }],
                    affected: vec![OsvAffected {
                        ranges: vec![OsvRange {
                            range_type: Some("SEMVER".into()),
                            events: vec![OsvEvent {
                                introduced: Some("0.1.0".into()),
                                fixed: Some("0.2.5".into()),
                            }],
                        }],
                    }],
                },
                OsvVuln {
                    id: Some("GHSA-abcd-efgh".into()),
                    summary: Some("Denial of service".into()),
                    severity: vec![OsvSeverity {
                        severity_type: Some("CVSS_V3".into()),
                        score: Some("CRITICAL".into()),
                    }],
                    affected: vec![],
                },
            ],
        };

        let result = format_vulnerability_results("serde", "crates.io", &data);
        assert!(result.starts_with("VULNERABLE: serde (crates.io)"));
        assert!(result.contains("CVE-2023-1234: Buffer overflow in parser (severity: HIGH)"));
        assert!(result.contains("GHSA-abcd-efgh: Denial of service (severity: CRITICAL)"));
        assert!(result.contains("Affected versions: 0.1.0..0.2.5"));
    }

    #[test]
    fn test_format_no_vulnerabilities() {
        let data = OsvResponse { vulns: vec![] };
        let result = format_vulnerability_results("tokio", "crates.io", &data);
        assert_eq!(result, "NO KNOWN VULNERABILITIES: tokio (crates.io)");
    }

    #[test]
    fn test_format_vulnerability_missing_severity() {
        let data = OsvResponse {
            vulns: vec![OsvVuln {
                id: Some("CVE-2024-0001".into()),
                summary: Some("Something bad".into()),
                severity: vec![], // no severity
                affected: vec![],
            }],
        };
        let result = format_vulnerability_results("pkg", "npm", &data);
        assert!(result.contains("severity: UNKNOWN"));
    }

    #[test]
    fn test_format_vulnerability_multiple_affected_ranges() {
        let data = OsvResponse {
            vulns: vec![OsvVuln {
                id: Some("CVE-2024-0001".into()),
                summary: Some("Issue".into()),
                severity: vec![],
                affected: vec![OsvAffected {
                    ranges: vec![OsvRange {
                        range_type: Some("SEMVER".into()),
                        events: vec![
                            OsvEvent {
                                introduced: Some("1.0.0".into()),
                                fixed: Some("1.0.5".into()),
                            },
                            OsvEvent {
                                introduced: Some("2.0.0".into()),
                                fixed: None,
                            },
                        ],
                    }],
                }],
            }],
        };
        let result = format_vulnerability_results("pkg", "npm", &data);
        assert!(result.contains("1.0.0..1.0.5"));
        assert!(result.contains("2.0.0..unfixed"));
    }

    #[tokio::test]
    async fn test_vulnerability_invalid_ecosystem() {
        let exec = ResearchToolExecutor::new(None, 0, None);
        let err = exec
            .check_vulnerability("pkg", "invalid")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Invalid ecosystem"));
    }

    // -- CheckPackage formatting tests --------------------------------------

    #[test]
    fn test_format_crates_io_current() {
        let data = CratesIoResponse {
            krate: Some(CratesIoCrate {
                description: Some("A serialization framework".into()),
                max_version: Some("1.0.200".into()),
                updated_at: Some("2024-01-15".into()),
            }),
            versions: vec![CratesIoVersion {
                num: Some("1.0.200".into()),
                yanked: false,
                updated_at: Some("2024-01-15".into()),
            }],
        };
        let result = format_crates_io_result("serde", &data);
        assert!(result.starts_with("CURRENT: serde@1.0.200 (crates.io)"));
        assert!(result.contains("Last updated: 2024-01-15"));
        assert!(result.contains("A serialization framework"));
    }

    #[test]
    fn test_format_crates_io_yanked() {
        let data = CratesIoResponse {
            krate: Some(CratesIoCrate {
                description: Some("Old crate".into()),
                max_version: Some("2.0.0".into()),
                updated_at: Some("2023-06-01".into()),
            }),
            versions: vec![
                CratesIoVersion {
                    num: Some("2.0.0".into()),
                    yanked: true,
                    updated_at: None,
                },
                CratesIoVersion {
                    num: Some("1.9.0".into()),
                    yanked: false,
                    updated_at: None,
                },
            ],
        };
        let result = format_crates_io_result("old-crate", &data);
        assert!(result.starts_with("DEPRECATED: old-crate@2.0.0 (crates.io)"));
        assert!(result.contains("Status: yanked"));
        assert!(result.contains("Latest non-yanked: 1.9.0"));
    }

    #[test]
    fn test_format_npm_current() {
        let mut time_map = serde_json::Map::new();
        time_map.insert("modified".into(), Value::String("2024-03-01".into()));

        let data = NpmResponse {
            name: Some("express".into()),
            description: Some("Fast web framework".into()),
            dist_tags: Some(NpmDistTags {
                latest: Some("4.18.2".into()),
            }),
            time: Some(time_map),
        };
        let result = format_npm_result("express", &data);
        assert!(result.starts_with("CURRENT: express@4.18.2 (npm)"));
        assert!(result.contains("Last updated: 2024-03-01"));
        assert!(result.contains("Fast web framework"));
    }

    #[test]
    fn test_format_pypi_current() {
        let data = PyPiResponse {
            info: Some(PyPiInfo {
                version: Some("3.12.0".into()),
                summary: Some("Python HTTP library".into()),
                yanked: false,
                yanked_reason: None,
            }),
        };
        let result = format_pypi_result("requests", &data);
        assert!(result.starts_with("CURRENT: requests@3.12.0 (PyPI)"));
        assert!(result.contains("Python HTTP library"));
    }

    #[test]
    fn test_format_pypi_yanked() {
        let data = PyPiResponse {
            info: Some(PyPiInfo {
                version: Some("1.0.0".into()),
                summary: Some("Bad package".into()),
                yanked: true,
                yanked_reason: Some("Security issue".into()),
            }),
        };
        let result = format_pypi_result("bad-pkg", &data);
        assert!(result.starts_with("DEPRECATED: bad-pkg@1.0.0 (PyPI)"));
        assert!(result.contains("Status: yanked"));
        assert!(result.contains("Security issue"));
    }

    #[test]
    fn test_format_package_not_found() {
        let data = CratesIoResponse {
            krate: None,
            versions: vec![],
        };
        let result = format_crates_io_result("nonexistent", &data);
        assert_eq!(result, "NOT FOUND: nonexistent not found on crates.io");
    }

    #[tokio::test]
    async fn test_format_package_unknown_ecosystem() {
        let exec = ResearchToolExecutor::new(None, 0, None);
        let err = exec.check_package("pkg", "rubygems").await.unwrap_err();
        assert!(err.to_string().contains("Unknown ecosystem"));
    }

    // -- SearchIssues formatting tests --------------------------------------

    #[test]
    fn test_format_issues_found() {
        let issues = vec![
            GhIssue {
                number: Some(42),
                title: Some("Fix memory leak".into()),
                state: Some("OPEN".into()),
                labels: vec![
                    GhLabel { name: Some("bug".into()) },
                    GhLabel { name: Some("priority:high".into()) },
                ],
                created_at: Some("2024-01-10".into()),
                body: Some("There is a memory leak in the parser module.".into()),
            },
            GhIssue {
                number: Some(38),
                title: Some("Add logging".into()),
                state: Some("CLOSED".into()),
                labels: vec![],
                created_at: Some("2024-01-05".into()),
                body: Some("We need better logging.".into()),
            },
            GhIssue {
                number: Some(50),
                title: Some("Update deps".into()),
                state: Some("OPEN".into()),
                labels: vec![GhLabel { name: Some("maintenance".into()) }],
                created_at: Some("2024-02-01".into()),
                body: None,
            },
        ];
        let result = format_issues_results("memory", &issues);
        assert!(result.starts_with("RELATED ISSUES (3 found):"));
        assert!(result.contains("#42 \"Fix memory leak\" (OPEN, 2024-01-10)"));
        assert!(result.contains("Labels: bug, priority:high"));
        assert!(result.contains("Summary: There is a memory leak"));
        assert!(result.contains("#38 \"Add logging\" (CLOSED, 2024-01-05)"));
        assert!(result.contains("#50 \"Update deps\""));
    }

    #[test]
    fn test_format_no_issues() {
        let result = format_issues_results("nonexistent topic", &[]);
        assert_eq!(
            result,
            "NO RELATED ISSUES: No issues found matching \"nonexistent topic\""
        );
    }

    #[test]
    fn test_format_issues_truncated_body() {
        let long_body = "a".repeat(200);
        let issues = vec![GhIssue {
            number: Some(1),
            title: Some("Test".into()),
            state: Some("OPEN".into()),
            labels: vec![],
            created_at: Some("2024-01-01".into()),
            body: Some(long_body),
        }];
        let result = format_issues_results("test", &issues);
        // Should contain exactly 150 'a' chars plus "..."
        assert!(result.contains(&format!("{}...", "a".repeat(150))));
    }

    #[test]
    fn test_format_issues_special_characters() {
        let issues = vec![GhIssue {
            number: Some(99),
            title: Some("Fix \"quoted\" & <escaped> title".into()),
            state: Some("OPEN".into()),
            labels: vec![],
            created_at: Some("2024-01-01".into()),
            body: Some("Body with \"quotes\" and <tags>".into()),
        }];
        let result = format_issues_results("test", &issues);
        assert!(result.contains("\"Fix \\\"quoted\\\" & <escaped> title\"")
            || result.contains("Fix \"quoted\" & <escaped> title"));
    }

    // -- Tool definitions tests ---------------------------------------------

    #[test]
    fn test_tool_definitions_with_exa_key() {
        let exec = ResearchToolExecutor::new(Some("key".into()), 3, Some("owner/repo".into()));
        let defs = exec.tool_definitions("WebSearch,CheckVulnerability,CheckPackage,SearchIssues");
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"web_search"));
        assert!(names.contains(&"check_vulnerability"));
        assert!(names.contains(&"check_package"));
        assert!(names.contains(&"search_issues"));
    }

    #[test]
    fn test_tool_definitions_without_exa_key() {
        let exec = ResearchToolExecutor::new(None, 3, Some("owner/repo".into()));
        let defs = exec.tool_definitions("WebSearch,CheckVulnerability,CheckPackage,SearchIssues");
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(!names.contains(&"web_search"));
        assert!(names.contains(&"check_vulnerability"));
        assert!(names.contains(&"check_package"));
        assert!(names.contains(&"search_issues"));
    }

    #[test]
    fn test_research_tools_always_included() {
        let exec = ResearchToolExecutor::new(None, 0, Some("owner/repo".into()));
        let defs = exec.tool_definitions("CheckVulnerability,CheckPackage,SearchIssues");
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"check_vulnerability"));
        assert!(names.contains(&"check_package"));
        assert!(names.contains(&"search_issues"));
    }

    #[test]
    fn test_tool_definitions_empty_filter() {
        let exec = ResearchToolExecutor::new(Some("key".into()), 3, Some("owner/repo".into()));
        let defs = exec.tool_definitions("");
        assert!(defs.is_empty());
    }
}

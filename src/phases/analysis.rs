use crate::claude::{self, ClaudeInvocation};
use crate::models::{AnalysisResult, Improvement, OpenPr, Risk, Severity, StackInfo};
use crate::prompts::analysis::ANALYSIS_PROMPT;
use crate::prompts::doc_analysis::DOC_ANALYSIS_PROMPT;
use crate::prompts::system::analysis_system_prompt;
use anyhow::Result;
use std::path::Path;
use std::time::Duration;
use tracing::info;

pub struct AnalysisOutput {
    pub improvements: Vec<Improvement>,
    pub cost_usd: f64,
}

/// Map a `Severity` to a numeric value for comparison and sorting.
fn severity_rank(s: &Severity) -> u8 {
    match s {
        Severity::Minor => 0,
        Severity::Moderate => 1,
        Severity::Major => 2,
    }
}

/// Map a `Risk` to a numeric value for sorting.
fn risk_rank(r: &Risk) -> u8 {
    match r {
        Risk::Low => 0,
        Risk::Medium => 1,
        Risk::High => 2,
    }
}

/// Format `StackInfo` into a human-readable summary for the prompt.
fn format_stack_info(stack: &StackInfo) -> String {
    let mut lines = Vec::new();
    lines.push(format!("- Primary language: {}", stack.primary_language));
    if !stack.build_commands.is_empty() {
        lines.push(format!("- Build commands: {}", stack.build_commands.join(", ")));
    }
    if !stack.test_commands.is_empty() {
        lines.push(format!("- Test commands: {}", stack.test_commands.join(", ")));
    }
    if !stack.lint_commands.is_empty() {
        lines.push(format!("- Lint commands: {}", stack.lint_commands.join(", ")));
    }
    if !stack.key_directories.is_empty() {
        lines.push(format!("- Key directories: {}", stack.key_directories.join(", ")));
    }
    if stack.has_ci {
        lines.push(format!("- CI files: {}", stack.ci_files.join(", ")));
    }
    lines.join("\n")
}

/// Format open PRs as a markdown list for the prompt.
fn format_open_prs(prs: &[OpenPr]) -> String {
    if prs.is_empty() {
        return "(none)".to_string();
    }
    prs.iter()
        .map(|pr| {
            let files = if pr.files.is_empty() {
                "(unknown)".to_string()
            } else {
                pr.files.join(", ")
            };
            format!(
                "- #{}: {} (files: {})",
                pr.number,
                pr.title,
                files,
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub async fn run(
    clone_path: &Path,
    arch_summary: &str,
    stack_info: &StackInfo,
    open_prs: &[OpenPr],
    model: &str,
    budget: f64,
    max_tasks: usize,
    min_severity: &Severity,
) -> Result<AnalysisOutput> {
    // 1. Build the prompt.
    let prompt = ANALYSIS_PROMPT
        .replace("{arch_summary}", arch_summary)
        .replace("{stack_info}", &format_stack_info(stack_info))
        .replace("{open_prs}", &format_open_prs(open_prs));

    // 2. Build the invocation.
    let invocation = ClaudeInvocation {
        prompt,
        system_prompt: Some(analysis_system_prompt()),
        model: model.to_string(),
        max_budget_usd: budget,
        max_turns: 50,
        effort: "high",
        tools: "Read,Glob,Grep,Agent",
        json_schema: None,
        working_dir: clone_path.to_path_buf(),
        session_id: None,
        resume_session_id: None,
    };

    // 3. Invoke Claude.
    let response = claude::invoke::<AnalysisResult>(&invocation, Duration::from_secs(900)).await?;

    let analysis = response
        .structured
        .unwrap_or_else(|| AnalysisResult {
            improvements: Vec::new(),
        });

    let total_found = analysis.improvements.len();
    info!(total_found, "analysis phase: raw improvements from Claude");

    // 4. Post-process improvements.
    let min_rank = severity_rank(min_severity);

    let mut filtered: Vec<Improvement> = analysis
        .improvements
        .into_iter()
        .filter(|imp| imp.risk != Risk::High)
        .filter(|imp| imp.estimated_lines_changed <= 500)
        .filter(|imp| severity_rank(&imp.severity) >= min_rank)
        .collect();

    // Sort by severity descending, then risk ascending.
    filtered.sort_by(|a, b| {
        severity_rank(&b.severity)
            .cmp(&severity_rank(&a.severity))
            .then_with(|| risk_rank(&a.risk).cmp(&risk_rank(&b.risk)))
    });

    // Truncate to max_tasks.
    filtered.truncate(max_tasks);

    info!(
        total_found,
        after_filtering = filtered.len(),
        "analysis phase: improvements after filtering"
    );

    Ok(AnalysisOutput {
        improvements: filtered,
        cost_usd: response.cost_usd,
    })
}

/// Run documentation-focused analysis as a fallback when no code improvements are found.
pub async fn run_doc_analysis(
    clone_path: &Path,
    arch_summary: &str,
    stack_info: &StackInfo,
    model: &str,
    budget: f64,
    max_tasks: usize,
) -> Result<AnalysisOutput> {
    // 1. Build the doc-specific prompt.
    let prompt = DOC_ANALYSIS_PROMPT
        .replace("{arch_summary}", arch_summary)
        .replace("{stack_info}", &format_stack_info(stack_info));

    // 2. Build the invocation.
    let invocation = ClaudeInvocation {
        prompt,
        system_prompt: Some(analysis_system_prompt()),
        model: model.to_string(),
        max_budget_usd: budget,
        max_turns: 50,
        effort: "high",
        tools: "Read,Glob,Grep,Agent",
        json_schema: None,
        working_dir: clone_path.to_path_buf(),
        session_id: None,
        resume_session_id: None,
    };

    // 3. Invoke Claude.
    let response = claude::invoke::<AnalysisResult>(&invocation, Duration::from_secs(900)).await?;

    let analysis = response
        .structured
        .unwrap_or_else(|| AnalysisResult {
            improvements: Vec::new(),
        });

    let total_found = analysis.improvements.len();
    info!(total_found, "doc analysis phase: raw improvements from Claude");

    // 4. Post-process: lighter filtering for docs (no severity filter, allow docs category).
    let mut filtered: Vec<Improvement> = analysis
        .improvements
        .into_iter()
        .filter(|imp| imp.risk != Risk::High)
        .filter(|imp| imp.estimated_lines_changed <= 500)
        .collect();

    // Sort by severity descending, then risk ascending.
    filtered.sort_by(|a, b| {
        severity_rank(&b.severity)
            .cmp(&severity_rank(&a.severity))
            .then_with(|| risk_rank(&a.risk).cmp(&risk_rank(&b.risk)))
    });

    // Truncate to max_tasks.
    filtered.truncate(max_tasks);

    info!(
        total_found,
        after_filtering = filtered.len(),
        "doc analysis phase: improvements after filtering"
    );

    Ok(AnalysisOutput {
        improvements: filtered,
        cost_usd: response.cost_usd,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Category;

    #[test]
    fn test_severity_rank_ordering() {
        assert!(severity_rank(&Severity::Minor) < severity_rank(&Severity::Moderate));
        assert!(severity_rank(&Severity::Moderate) < severity_rank(&Severity::Major));
    }

    #[test]
    fn test_format_open_prs_empty() {
        assert_eq!(format_open_prs(&[]), "(none)");
    }

    #[test]
    fn test_format_open_prs_single() {
        let prs = vec![OpenPr {
            number: 42,
            title: "Fix widget".to_string(),
            head_ref: "fix/widget".to_string(),
            files: vec!["src/widget.rs".to_string(), "tests/widget.rs".to_string()],
        }];
        let output = format_open_prs(&prs);
        assert!(output.contains("#42"));
        assert!(output.contains("Fix widget"));
        assert!(output.contains("src/widget.rs, tests/widget.rs"));
    }

    #[test]
    fn test_format_stack_info() {
        let stack = StackInfo {
            primary_language: "Rust".to_string(),
            build_commands: vec!["cargo build".to_string()],
            test_commands: vec!["cargo test".to_string()],
            lint_commands: vec!["cargo clippy".to_string()],
            key_directories: vec!["src".to_string()],
            has_ci: true,
            ci_files: vec![".github/workflows/ci.yml".to_string()],
        };
        let output = format_stack_info(&stack);
        assert!(output.contains("Rust"));
        assert!(output.contains("cargo build"));
        assert!(output.contains("cargo test"));
        assert!(output.contains("cargo clippy"));
        assert!(output.contains("ci.yml"));
    }

    fn make_improvement(severity: Severity, risk: Risk, lines: u32) -> Improvement {
        Improvement {
            title: format!("{severity:?}/{risk:?}"),
            description: "test".to_string(),
            severity,
            category: Category::Bug,
            files_to_modify: vec!["test.rs".to_string()],
            estimated_lines_changed: lines,
            risk,
        }
    }

    #[test]
    fn test_filtering_removes_high_risk() {
        let imps = vec![
            make_improvement(Severity::Major, Risk::High, 100),
            make_improvement(Severity::Major, Risk::Low, 100),
        ];
        let min_rank = severity_rank(&Severity::Minor);
        let filtered: Vec<_> = imps
            .into_iter()
            .filter(|i| i.risk != Risk::High)
            .filter(|i| i.estimated_lines_changed <= 500)
            .filter(|i| severity_rank(&i.severity) >= min_rank)
            .collect();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].risk, Risk::Low);
    }

    #[test]
    fn test_filtering_removes_large_changes() {
        let imps = vec![
            make_improvement(Severity::Major, Risk::Low, 501),
            make_improvement(Severity::Major, Risk::Low, 500),
        ];
        let min_rank = severity_rank(&Severity::Minor);
        let filtered: Vec<_> = imps
            .into_iter()
            .filter(|i| i.risk != Risk::High)
            .filter(|i| i.estimated_lines_changed <= 500)
            .filter(|i| severity_rank(&i.severity) >= min_rank)
            .collect();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].estimated_lines_changed, 500);
    }

    #[test]
    fn test_filtering_by_min_severity() {
        let imps = vec![
            make_improvement(Severity::Minor, Risk::Low, 50),
            make_improvement(Severity::Moderate, Risk::Low, 50),
            make_improvement(Severity::Major, Risk::Low, 50),
        ];
        let min_rank = severity_rank(&Severity::Moderate);
        let filtered: Vec<_> = imps
            .into_iter()
            .filter(|i| i.risk != Risk::High)
            .filter(|i| i.estimated_lines_changed <= 500)
            .filter(|i| severity_rank(&i.severity) >= min_rank)
            .collect();
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn test_sorting_severity_desc_risk_asc() {
        let mut imps = vec![
            make_improvement(Severity::Minor, Risk::Medium, 50),
            make_improvement(Severity::Major, Risk::Medium, 50),
            make_improvement(Severity::Major, Risk::Low, 50),
            make_improvement(Severity::Moderate, Risk::Low, 50),
        ];
        imps.sort_by(|a, b| {
            severity_rank(&b.severity)
                .cmp(&severity_rank(&a.severity))
                .then_with(|| risk_rank(&a.risk).cmp(&risk_rank(&b.risk)))
        });
        assert_eq!(imps[0].severity, Severity::Major);
        assert_eq!(imps[0].risk, Risk::Low);
        assert_eq!(imps[1].severity, Severity::Major);
        assert_eq!(imps[1].risk, Risk::Medium);
        assert_eq!(imps[2].severity, Severity::Moderate);
        assert_eq!(imps[3].severity, Severity::Minor);
    }
}

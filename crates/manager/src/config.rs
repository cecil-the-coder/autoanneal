use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize, Clone)]
pub struct ManagerConfig {
    pub manager: ManagerSettings,
    #[serde(default)]
    pub defaults: WorkerDefaults,
    pub repos: Vec<RepoEntry>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ManagerSettings {
    #[serde(default = "default_listen_addr")]
    pub listen_addr: String,
    #[serde(default = "default_concurrency")]
    pub global_concurrency: usize,
    #[serde(default)]
    pub webhook_secret: String,
    #[serde(default = "default_worker_image")]
    pub worker_image: String,
    #[serde(default = "default_result_path")]
    pub result_path: String,
    #[serde(default = "default_true")]
    pub docker_mode: bool,
}

fn default_listen_addr() -> String { "0.0.0.0:8080".into() }
fn default_concurrency() -> usize { 3 }
fn default_worker_image() -> String { "autoanneal:latest".into() }
fn default_result_path() -> String { "/tmp/autoanneal-result.json".into() }
fn default_true() -> bool { true }

#[derive(Debug, Deserialize, Clone)]
pub struct WorkerDefaults {
    #[serde(default = "default_max_budget")]
    pub max_budget: String,
    #[serde(default = "default_timeout")]
    pub timeout: String,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default)]
    pub model_recon: Option<String>,
    #[serde(default)]
    pub model_analysis: Option<String>,
    #[serde(default)]
    pub model_implement: Option<String>,
    #[serde(default)]
    pub model_critic: Option<String>,
    #[serde(default)]
    pub model_plan: Option<String>,
    #[serde(default = "default_max_tasks")]
    pub max_tasks: usize,
    #[serde(default = "default_min_severity")]
    pub min_severity: String,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub setup_command: Option<String>,
    #[serde(default = "default_skip_after")]
    pub skip_after: usize,
    #[serde(default = "default_cron_interval")]
    pub cron_interval: u64,
    #[serde(default = "default_true")]
    pub fix_ci: bool,
    #[serde(default = "default_true")]
    pub fix_conflicts: bool,
    #[serde(default)]
    pub critic_threshold: u32,
    #[serde(default)]
    pub critic_models: Option<String>,
    #[serde(default = "default_true")]
    pub improve_docs: bool,
    #[serde(default = "default_doc_critic_threshold")]
    pub doc_critic_threshold: u32,
    #[serde(default)]
    pub fix_external_ci: bool,
    #[serde(default)]
    pub review_prs: bool,
    #[serde(default = "default_review_filter")]
    pub review_filter: String,
    #[serde(default = "default_review_fix_threshold")]
    pub review_fix_threshold: u32,
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    #[serde(default = "default_max_open_prs")]
    pub max_open_prs: usize,
    #[serde(default)]
    pub investigate_issues: String,
    #[serde(default = "default_max_issues")]
    pub max_issues: usize,
    #[serde(default = "default_issue_budget")]
    pub issue_budget: String,
    #[serde(default = "default_context_window")]
    pub context_window: u64,
}

impl Default for WorkerDefaults {
    fn default() -> Self {
        Self {
            max_budget: default_max_budget(),
            timeout: default_timeout(),
            model: default_model(),
            model_recon: None,
            model_analysis: None,
            model_implement: None,
            model_critic: None,
            model_plan: None,
            max_tasks: default_max_tasks(),
            min_severity: default_min_severity(),
            log_level: default_log_level(),
            dry_run: false,
            setup_command: None,
            skip_after: default_skip_after(),
            cron_interval: default_cron_interval(),
            fix_ci: true,
            fix_conflicts: true,
            critic_threshold: 6,
            critic_models: None,
            improve_docs: true,
            doc_critic_threshold: default_doc_critic_threshold(),
            fix_external_ci: false,
            review_prs: false,
            review_filter: default_review_filter(),
            review_fix_threshold: default_review_fix_threshold(),
            concurrency: default_concurrency(),
            max_open_prs: default_max_open_prs(),
            investigate_issues: String::new(),
            max_issues: default_max_issues(),
            issue_budget: default_issue_budget(),
            context_window: default_context_window(),
        }
    }
}

fn default_max_budget() -> String { "5.00".into() }
fn default_timeout() -> String { "30m".into() }
fn default_model() -> String { "sonnet".into() }
fn default_max_tasks() -> usize { 3 }
fn default_min_severity() -> String { "minor".into() }
fn default_log_level() -> String { "info".into() }
fn default_skip_after() -> usize { 3 }
fn default_cron_interval() -> u64 { 10 }
fn default_doc_critic_threshold() -> u32 { 7 }
fn default_review_filter() -> String { "all".into() }
fn default_review_fix_threshold() -> u32 { 7 }
fn default_max_open_prs() -> usize { 5 }
fn default_max_issues() -> usize { 2 }
fn default_issue_budget() -> String { "3.00".into() }
fn default_context_window() -> u64 { 128000 }

#[derive(Debug, Deserialize, Clone)]
pub struct RepoEntry {
    pub name: String,
    pub repo: String,
    #[serde(default)]
    pub schedule: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    // Per-repo overrides (all optional, fall back to defaults)
    #[serde(default)]
    pub max_budget: Option<String>,
    #[serde(default)]
    pub timeout: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub model_recon: Option<String>,
    #[serde(default)]
    pub model_analysis: Option<String>,
    #[serde(default)]
    pub model_implement: Option<String>,
    #[serde(default)]
    pub model_critic: Option<String>,
    #[serde(default)]
    pub model_plan: Option<String>,
    #[serde(default)]
    pub max_tasks: Option<usize>,
    #[serde(default)]
    pub min_severity: Option<String>,
    #[serde(default)]
    pub log_level: Option<String>,
    #[serde(default)]
    pub dry_run: Option<bool>,
    #[serde(default)]
    pub setup_command: Option<String>,
    #[serde(default)]
    pub skip_after: Option<usize>,
    #[serde(default)]
    pub cron_interval: Option<u64>,
    #[serde(default)]
    pub fix_ci: Option<bool>,
    #[serde(default)]
    pub fix_conflicts: Option<bool>,
    #[serde(default)]
    pub critic_threshold: Option<u32>,
    #[serde(default)]
    pub critic_models: Option<String>,
    #[serde(default)]
    pub improve_docs: Option<bool>,
    #[serde(default)]
    pub doc_critic_threshold: Option<u32>,
    #[serde(default)]
    pub fix_external_ci: Option<bool>,
    #[serde(default)]
    pub review_prs: Option<bool>,
    #[serde(default)]
    pub review_filter: Option<String>,
    #[serde(default)]
    pub review_fix_threshold: Option<u32>,
    #[serde(default)]
    pub concurrency: Option<usize>,
    #[serde(default)]
    pub max_open_prs: Option<usize>,
    #[serde(default)]
    pub investigate_issues: Option<String>,
    #[serde(default)]
    pub max_issues: Option<usize>,
    #[serde(default)]
    pub issue_budget: Option<String>,
    #[serde(default)]
    pub context_window: Option<u64>,
}

impl RepoEntry {
    /// Build the CLI arguments for the worker binary, resolving overrides against defaults.
    pub fn to_worker_args(&self, defaults: &WorkerDefaults) -> Vec<String> {
        let mut args = vec![
            self.repo.clone(),
            "--max-budget".to_string(), self.max_budget.as_ref().unwrap_or(&defaults.max_budget).clone(),
            "--timeout".to_string(), self.timeout.as_ref().unwrap_or(&defaults.timeout).clone(),
            "--model".to_string(), self.model.as_ref().unwrap_or(&defaults.model).clone(),
            "--max-tasks".to_string(), self.max_tasks.unwrap_or(defaults.max_tasks).to_string(),
            "--min-severity".to_string(), self.min_severity.as_ref().unwrap_or(&defaults.min_severity).clone(),
            "--log-level".to_string(), self.log_level.as_ref().unwrap_or(&defaults.log_level).clone(),
            "--skip-after".to_string(), self.skip_after.unwrap_or(defaults.skip_after).to_string(),
            "--cron-interval".to_string(), self.cron_interval.unwrap_or(defaults.cron_interval).to_string(),
            "--critic-threshold".to_string(), self.critic_threshold.unwrap_or(defaults.critic_threshold).to_string(),
            "--doc-critic-threshold".to_string(), self.doc_critic_threshold.unwrap_or(defaults.doc_critic_threshold).to_string(),
            "--review-filter".to_string(), self.review_filter.as_ref().unwrap_or(&defaults.review_filter).clone(),
            "--review-fix-threshold".to_string(), self.review_fix_threshold.unwrap_or(defaults.review_fix_threshold).to_string(),
            "--concurrency".to_string(), self.concurrency.unwrap_or(defaults.concurrency).to_string(),
            "--max-open-prs".to_string(), self.max_open_prs.unwrap_or(defaults.max_open_prs).to_string(),
            "--max-issues".to_string(), self.max_issues.unwrap_or(defaults.max_issues).to_string(),
            "--issue-budget".to_string(), self.issue_budget.as_ref().unwrap_or(&defaults.issue_budget).clone(),
            "--context-window".to_string(), self.context_window.unwrap_or(defaults.context_window).to_string(),
        ];

        // Boolean flags
        let fix_ci = self.fix_ci.unwrap_or(defaults.fix_ci);
        args.push(if fix_ci { "--fix-ci".into() } else { "--no-fix-ci".into() });
        let fix_conflicts = self.fix_conflicts.unwrap_or(defaults.fix_conflicts);
        args.push(if fix_conflicts { "--fix-conflicts".into() } else { "--no-fix-conflicts".into() });
        let improve_docs = self.improve_docs.unwrap_or(defaults.improve_docs);
        args.push(if improve_docs { "--improve-docs".into() } else { "--no-improve-docs".into() });
        let dry_run = self.dry_run.unwrap_or(defaults.dry_run);
        if dry_run { args.push("--dry-run".into()); }
        let fix_external_ci = self.fix_external_ci.unwrap_or(defaults.fix_external_ci);
        if fix_external_ci { args.push("--fix-external-ci".into()); }
        let review_prs = self.review_prs.unwrap_or(defaults.review_prs);
        if review_prs { args.push("--review-prs".into()); }

        // Investigate issues
        let investigate = self.investigate_issues.as_deref().unwrap_or(&defaults.investigate_issues);
        if !investigate.is_empty() {
            args.extend_from_slice(&["--investigate-issues".into(), investigate.to_string()]);
        }

        // Optional string flags
        if let Some(v) = self.setup_command.as_ref().or(defaults.setup_command.as_ref()) {
            args.extend_from_slice(&["--setup-command".into(), v.clone()]);
        }
        if let Some(v) = self.model_recon.as_ref().or(defaults.model_recon.as_ref()) {
            args.extend_from_slice(&["--model-recon".into(), v.clone()]);
        }
        if let Some(v) = self.model_analysis.as_ref().or(defaults.model_analysis.as_ref()) {
            args.extend_from_slice(&["--model-analysis".into(), v.clone()]);
        }
        if let Some(v) = self.model_implement.as_ref().or(defaults.model_implement.as_ref()) {
            args.extend_from_slice(&["--model-implement".into(), v.clone()]);
        }
        if let Some(v) = self.model_critic.as_ref().or(defaults.model_critic.as_ref()) {
            args.extend_from_slice(&["--model-critic".into(), v.clone()]);
        }
        if let Some(v) = self.model_plan.as_ref().or(defaults.model_plan.as_ref()) {
            args.extend_from_slice(&["--model-plan".into(), v.clone()]);
        }
        if let Some(v) = self.critic_models.as_ref().or(defaults.critic_models.as_ref()) {
            args.extend_from_slice(&["--critic-models".into(), v.clone()]);
        }

        args
    }
}

/// CLI args for the manager binary.
#[derive(Debug, clap::Parser)]
#[command(name = "autoanneal-manager", about = "Manager for autoanneal workers")]
pub struct ManagerCli {
    /// Path to config file (YAML)
    #[arg(long, default_value = "/config/manager.yaml")]
    pub config: PathBuf,

    /// Log level
    #[arg(long, default_value = "info")]
    pub log_level: String,
}

pub fn load_config(path: &std::path::Path) -> anyhow::Result<ManagerConfig> {
    let content = std::fs::read_to_string(path)?;
    let config: ManagerConfig = serde_yaml::from_str(&content)?;
    Ok(config)
}

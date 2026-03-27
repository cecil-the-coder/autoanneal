use crate::models::PhaseReport;
use std::time::Duration;

/// Initialize tracing subscriber based on log level string.
/// Outputs structured JSON logs to stdout (same stream as summary).
/// Using a single stream prevents log loss in containerized environments.
pub fn init(log_level: &str) {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::new(log_level);

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .json()
        .with_target(false)
        .with_writer(std::io::stdout)
        .init();
}

/// Format a `Duration` in human-readable form (e.g. "3s", "1m 12s", "1h 30m 0s").
fn fmt_duration(d: Duration) -> String {
    let total_secs = d.as_secs();
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;
    if hours > 0 {
        format!("{}h {}m {}s", hours, mins, secs)
    } else if mins > 0 {
        format!("{}m {}s", mins, secs)
    } else {
        format!("{}s", secs)
    }
}

/// Format a cost as "$X.XX", or "—" when zero.
fn fmt_cost(cost: f64) -> String {
    if cost <= 0.0 {
        "\u{2014}".to_string() // em dash
    } else {
        format!("${:.2}", cost)
    }
}

/// Print the final summary report to stdout.
pub fn print_summary(
    repo_slug: &str,
    branch: Option<&str>,
    pr_url: Option<&str>,
    phases: &[PhaseReport],
    total_cost: f64,
) {
    let sep = "\u{2500}"; // box-drawing horizontal

    println!();
    println!("autoanneal run complete");
    println!("{}", sep.repeat(37));

    println!("{:<15}{}", "Repository:", repo_slug);
    if let Some(b) = branch {
        println!("{:<15}{}", "Branch:", b);
    }
    if let Some(url) = pr_url {
        println!("{:<15}{}", "PR:", url);
    }

    println!();

    // Column widths
    let w_name = 15;
    let w_dur = 10;
    let w_cost = 7;
    let w_status = 8;

    // Header
    println!(
        "{:<w_name$}  {:<w_dur$}  {:<w_cost$}  {:<w_status$}",
        "Phase", "Duration", "Cost", "Status",
    );

    let rule = format!(
        "{:\u{2500}<w_name$}  {:\u{2500}<w_dur$}  {:\u{2500}<w_cost$}  {:\u{2500}<w_status$}",
        "", "", "", "",
    );
    println!("{}", rule);

    let mut total_duration = Duration::ZERO;

    for phase in phases {
        total_duration += phase.duration;
        println!(
            "{:<w_name$}  {:<w_dur$}  {:<w_cost$}  {:<w_status$}",
            phase.name,
            fmt_duration(phase.duration),
            fmt_cost(phase.cost_usd),
            phase.status,
        );
    }

    println!("{}", rule);

    println!(
        "{:<w_name$}  {:<w_dur$}  {:<w_cost$}",
        "Total",
        fmt_duration(total_duration),
        fmt_cost(total_cost),
    );

    println!();
}

use autoanneal_lib::config::Config;
use autoanneal_lib::logging;
use autoanneal_lib::result::{PhaseResult, WorkerResult, WorkItemResult, RESULT_SCHEMA_VERSION};

mod orchestrator;

use clap::Parser;
use std::time::Instant;

#[tokio::main]
async fn main() {
    let config = Config::parse();
    let start = Instant::now();

    // Initialize logging
    logging::init(&config.log_level);

    // Run the orchestrator
    let output = match orchestrator::run(&config).await {
        Ok(output) => output,
        Err(e) => {
            tracing::error!(error = %e, "fatal error");
            eprintln!("Error: {e:#}");

            // Emit a minimal result even on fatal errors.
            let result = WorkerResult {
                version: RESULT_SCHEMA_VERSION,
                repo: config.repo_slug(),
                exit_code: 1,
                total_cost_usd: 0.0,
                total_duration_secs: start.elapsed().as_secs(),
                phases: vec![],
                pr_url: None,
                pr_number: None,
                branch_name: None,
                work_items: vec![],
            };
            let _ = result.emit();
            std::process::exit(1);
        }
    };

    let exit_code = output.exit_code;

    // Build the structured result from orchestrator output.
    let worker_result = WorkerResult {
        version: RESULT_SCHEMA_VERSION,
        repo: output.repo_slug,
        exit_code: output.exit_code,
        total_cost_usd: output.total_cost,
        total_duration_secs: start.elapsed().as_secs(),
        phases: output
            .phases
            .iter()
            .map(|p| PhaseResult {
                name: p.name.clone(),
                duration_secs: p.duration.as_secs(),
                cost_usd: p.cost_usd,
                status: p.status.clone(),
            })
            .collect(),
        pr_url: output.pr_url,
        pr_number: output.pr_number,
        branch_name: output.branch_name,
        work_items: output
            .work_items
            .into_iter()
            .map(|w| WorkItemResult {
                kind: w.kind,
                name: w.name,
                status: w.status,
                cost_usd: w.cost_usd,
                duration_secs: w.duration_secs,
                pr_url: w.pr_url,
            })
            .collect(),
    };

    // Emit result to stdout (and optionally to file).
    if let Err(e) = worker_result.emit() {
        tracing::warn!(error = %e, "failed to emit worker result");
    }

    std::process::exit(exit_code);
}

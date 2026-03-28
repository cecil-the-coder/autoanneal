mod agent;
mod llm;
mod cleanup;
mod config;
mod guardrails;
mod logging;
mod models;
mod orchestrator;
mod phases;
mod prompts;
mod retry;
mod worktree;

use clap::Parser;
use config::Config;

#[tokio::main]
async fn main() {
    let config = Config::parse();

    // Initialize logging
    logging::init(&config.log_level);

    // Run the orchestrator
    match orchestrator::run(&config).await {
        Ok(exit_code) => std::process::exit(exit_code),
        Err(e) => {
            tracing::error!(error = %e, "fatal error");
            eprintln!("Error: {e:#}");
            std::process::exit(1);
        }
    }
}

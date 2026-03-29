mod config;
mod executor;
mod metrics;
mod scheduler;
mod server;
mod state;
mod webhook;

use anyhow::Result;
use clap::Parser;
use config::ManagerCli;
use std::sync::Arc;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = ManagerCli::parse();

    // Init logging
    autoanneal_lib::logging::init(&cli.log_level);

    info!("autoanneal-manager starting");

    // Load config
    let config = config::load_config(&cli.config)?;
    info!(
        repos = config.repos.len(),
        concurrency = config.manager.global_concurrency,
        "loaded config"
    );

    // Create shared state
    let state_store = Arc::new(state::StateStore::new());

    // Create metrics
    let metrics = Arc::new(metrics::Metrics::new()?);

    // Create trigger channel
    let (trigger_tx, trigger_rx) = tokio::sync::mpsc::unbounded_channel();

    // Create executor based on mode
    let executor: Arc<dyn executor::Executor> = if config.manager.docker_mode {
        info!("using Docker executor");
        Arc::new(executor::docker::DockerExecutor)
    } else {
        info!("using Kubernetes executor");
        Arc::new(executor::kubernetes::KubernetesExecutor::new("default"))
    };

    // Build repo configs map for webhook routing
    let repo_configs = Arc::new(std::sync::Mutex::new(
        config.repos.iter().map(|r| (r.repo.clone(), r.name.clone())).collect()
    ));

    // Create scheduler
    let scheduler = scheduler::Scheduler::new(
        config.clone(),
        executor,
        state_store.clone(),
        trigger_rx,
        metrics.clone(),
    );

    // Start HTTP server in background
    let app_state = server::AppState {
        state_store,
        trigger_tx,
        metrics: Some(metrics),
        webhook_secret: config.manager.webhook_secret.clone(),
        repo_configs,
        webhook_cooldowns: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
    };
    let listen_addr = config.manager.listen_addr.clone();
    let server_handle = tokio::spawn(async move {
        if let Err(e) = server::run_server(app_state, &listen_addr).await {
            tracing::error!(error = %e, "HTTP server error");
        }
    });

    // Run scheduler (blocks)
    scheduler.run().await;

    // Clean up server
    server_handle.abort();

    Ok(())
}

use crate::config::ManagerConfig;
use crate::config::RepoEntry;
use crate::executor::{Executor, PendingRun};
use crate::metrics::Metrics;
use crate::state::{ActiveRun, RunRecord, StateStore, TriggerReason};
use chrono::Utc;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Semaphore, mpsc};
use tokio::time;
use tracing::{error, info, warn};

pub struct Scheduler {
    config: ManagerConfig,
    executor: Arc<dyn Executor>,
    state: Arc<StateStore>,
    semaphore: Arc<Semaphore>,
    trigger_rx: mpsc::UnboundedReceiver<TriggerMessage>,
    metrics: Arc<Metrics>,
    poll_interval_secs: u64,
}

pub struct TriggerMessage {
    pub repo_name: String,
    pub reason: TriggerReason,
}

impl Scheduler {
    pub fn new(
        config: ManagerConfig,
        executor: Arc<dyn Executor>,
        state: Arc<StateStore>,
        trigger_rx: mpsc::UnboundedReceiver<TriggerMessage>,
        metrics: Arc<Metrics>,
    ) -> Self {
        let permits = config.manager.global_concurrency.max(1);
        let poll_interval_secs = config.manager.poll_interval_secs;
        Self {
            config,
            executor,
            state,
            semaphore: Arc::new(Semaphore::new(permits)),
            trigger_rx,
            metrics,
            poll_interval_secs,
        }
    }

    pub async fn run(mut self) {
        // Set up interval-based timers for each repo with a schedule
        let repo_names: Vec<String> = self.config.repos.iter()
            .filter(|r| r.enabled)
            .map(|r| r.name.clone())
            .collect();

        let mut cron_interval = time::interval(Duration::from_secs(60));

        loop {
            tokio::select! {
                // Periodic cron tick
                _ = cron_interval.tick() => {
                    for repo_name in &repo_names {
                        if let Some(entry) = self.config.repos.iter().find(|r| &r.name == repo_name) {
                            if entry.enabled {
                                self.launch_run(entry, TriggerReason::Scheduled).await;
                            }
                        }
                    }
                }

                // External trigger (webhook or manual)
                msg = self.trigger_rx.recv() => {
                    match msg {
                        Some(msg) => {
                            if let Some(entry) = self.config.repos.iter().find(|r| r.name == msg.repo_name) {
                                self.launch_run(entry, msg.reason).await;
                            } else {
                                warn!(repo = %msg.repo_name, "trigger for unknown repo");
                            }
                        }
                        None => {
                            // Channel closed, exit
                            info!("trigger channel closed, shutting down scheduler");
                            return;
                        }
                    }
                }
            }
        }
    }

    async fn launch_run(&self, entry: &RepoEntry, trigger: TriggerReason) {
        // Skip if already running
        if self.state.is_active(&entry.name) {
            info!(repo = %entry.name, "skipping: run already active");
            return;
        }

        // Try to acquire a permit (non-blocking check)
        let permit = match self.semaphore.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                warn!(repo = %entry.name, "skipping: global concurrency limit reached");
                return;
            }
        };

        let run_id = uuid::Uuid::new_v4().to_string().replace('-', "");

        let active_run = ActiveRun {
            run_id: run_id.clone(),
            repo_name: entry.name.clone(),
            started_at: Utc::now(),
            trigger: trigger.clone(),
        };
        self.state.insert_active(active_run);

        let pending = PendingRun {
            run_id: run_id.clone(),
            repo_entry: entry.clone(),
            defaults: self.config.defaults.clone(),
            worker_image: self.config.manager.worker_image.clone(),
            trigger: trigger.clone(),
            result_path: self.config.manager.result_path.clone(),
        };

        let executor = self.executor.clone();
        let state = self.state.clone();
        let repo_name = entry.name.clone();
        let repo = entry.repo.clone();
        let poll_interval_secs = self.poll_interval_secs;

        tokio::spawn(async move {
            let _permit = permit; // Hold permit for the duration

            if let Err(e) = executor.launch(&pending).await {
                error!(repo = %repo_name, error = %e, "failed to launch worker");
                state.remove_active(&repo_name);
                return;
            }

            // Poll until completion
            let mut interval = time::interval(Duration::from_secs(poll_interval_secs));
            loop {
                interval.tick().await;
                match executor.collect(&repo_name, &run_id).await {
                    Ok(Some(outcome)) => {
                        let active = state.remove_active(&repo_name);
                        let started_at = active.map(|a| a.started_at).unwrap_or_else(Utc::now);
                        state.record_completed(RunRecord {
                            run_id,
                            repo_name: repo_name.clone(),
                            repo,
                            started_at,
                            finished_at: Utc::now(),
                            exit_code: outcome.exit_code,
                            trigger,
                            result: outcome.result,
                        });
                        info!(
                            repo = %repo_name,
                            exit_code = outcome.exit_code,
                            "worker completed"
                        );
                        return;
                    }
                    Ok(None) => {
                        // Still running
                    }
                    Err(e) => {
                        error!(repo = %repo_name, error = %e, "error checking worker status");
                        state.remove_active(&repo_name);
                        return;
                    }
                }
            }
        });
    }
}

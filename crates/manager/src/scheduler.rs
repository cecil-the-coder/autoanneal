use crate::config::ManagerConfig;
use crate::config::RepoEntry;
use crate::executor::{Executor, PendingRun};
use crate::metrics::Metrics;
use crate::state::{ActiveRun, RunRecord, StateStore, TriggerReason};
use chrono::{DateTime, Utc};
use cron::Schedule;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Semaphore, mpsc};
use tokio::time;
use tracing::{error, info, warn};

/// Maximum number of launch retries on failure.
const MAX_LAUNCH_RETRIES: u32 = 2;

/// Default timeout for a worker run (30 minutes).
const DEFAULT_TIMEOUT_SECS: u64 = 30 * 60;

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

/// Per-repo scheduling state: parsed cron schedule and next fire time.
struct RepoSchedule {
    schedule: Schedule,
    next_run: DateTime<Utc>,
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
        // Parse cron schedules for each enabled repo that has one.
        let mut schedules: HashMap<String, RepoSchedule> = HashMap::new();
        let now = Utc::now();

        for entry in &self.config.repos {
            if !entry.enabled || entry.schedule.is_empty() {
                continue;
            }
            match Schedule::from_str(&entry.schedule) {
                Ok(sched) => {
                    let next = sched.upcoming(Utc).next().unwrap_or(now);
                    info!(
                        repo = %entry.name,
                        schedule = %entry.schedule,
                        next_run = %next,
                        "parsed cron schedule"
                    );
                    schedules.insert(
                        entry.name.clone(),
                        RepoSchedule {
                            schedule: sched,
                            next_run: next,
                        },
                    );
                }
                Err(e) => {
                    error!(
                        repo = %entry.name,
                        schedule = %entry.schedule,
                        error = %e,
                        "invalid cron expression, repo will only run on triggers"
                    );
                }
            }
        }

        // Tick every 10 seconds to check cron schedules.
        let mut tick_interval = time::interval(Duration::from_secs(10));

        loop {
            tokio::select! {
                // Periodic cron tick
                _ = tick_interval.tick() => {
                    let now = Utc::now();
                    for entry in &self.config.repos {
                        if !entry.enabled {
                            continue;
                        }
                        if let Some(repo_sched) = schedules.get_mut(&entry.name) {
                            if now >= repo_sched.next_run {
                                self.launch_run(entry, TriggerReason::Scheduled).await;
                                // Advance to next scheduled time.
                                repo_sched.next_run = repo_sched
                                    .schedule
                                    .upcoming(Utc)
                                    .next()
                                    .unwrap_or(now);
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

        // Increment runs_total and active_workers gauge.
        self.metrics.runs_total.inc();
        self.metrics.active_workers.inc();

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
        let metrics = self.metrics.clone();
        let repo_name = entry.name.clone();
        let repo = entry.repo.clone();
        let timeout = parse_timeout_str(
            entry
                .timeout
                .as_deref()
                .unwrap_or(&self.config.defaults.timeout),
        );
        let poll_interval_secs = self.poll_interval_secs;

        tokio::spawn(async move {
            let _permit = permit; // Hold permit for the duration

            // Launch with retries (up to MAX_LAUNCH_RETRIES).
            let mut launched = false;
            for attempt in 0..=MAX_LAUNCH_RETRIES {
                match executor.launch(&pending).await {
                    Ok(()) => {
                        launched = true;
                        break;
                    }
                    Err(e) => {
                        if attempt < MAX_LAUNCH_RETRIES {
                            let backoff = Duration::from_secs(5u64 * 3u64.pow(attempt));
                            warn!(
                                repo = %repo_name,
                                attempt = attempt + 1,
                                max_retries = MAX_LAUNCH_RETRIES,
                                backoff_secs = backoff.as_secs(),
                                error = %e,
                                "launch failed, retrying"
                            );
                            time::sleep(backoff).await;
                        } else {
                            error!(
                                repo = %repo_name,
                                attempts = MAX_LAUNCH_RETRIES + 1,
                                error = %e,
                                "failed to launch worker after all retries"
                            );
                        }
                    }
                }
            }

            if !launched {
                metrics.runs_failure.inc();
                metrics.active_workers.dec();
                state.remove_active(&repo_name);
                return;
            }

            let start = Utc::now();

            // Poll until completion or timeout.
            let mut interval = time::interval(Duration::from_secs(poll_interval_secs));
            loop {
                interval.tick().await;

                // Check timeout.
                let elapsed = Utc::now().signed_duration_since(start);
                if elapsed.num_seconds() > timeout.as_secs() as i64 {
                    warn!(
                        repo = %repo_name,
                        run_id = %run_id,
                        elapsed_secs = elapsed.num_seconds(),
                        timeout_secs = timeout.as_secs(),
                        "worker timed out, cancelling"
                    );
                    if let Err(e) = executor.cancel(&repo_name, &run_id).await {
                        error!(
                            repo = %repo_name,
                            error = %e,
                            "failed to cancel timed-out worker"
                        );
                    }
                    metrics.runs_timeout.inc();
                    metrics.active_workers.dec();

                    let active = state.remove_active(&repo_name);
                    let started_at = active.map(|a| a.started_at).unwrap_or(start);
                    let finished_at = Utc::now();
                    let duration_secs = finished_at
                        .signed_duration_since(started_at)
                        .num_seconds()
                        .max(0) as f64;
                    metrics.run_duration.observe(duration_secs);

                    state.record_completed(RunRecord {
                        run_id,
                        repo_name: repo_name.clone(),
                        repo,
                        started_at,
                        finished_at,
                        exit_code: -1,
                        trigger,
                        result: None,
                    });
                    return;
                }

                match executor.collect(&repo_name, &run_id).await {
                    Ok(Some(outcome)) => {
                        let active = state.remove_active(&repo_name);
                        let started_at = active.map(|a| a.started_at).unwrap_or(start);
                        let finished_at = Utc::now();
                        let duration_secs = finished_at
                            .signed_duration_since(started_at)
                            .num_seconds()
                            .max(0) as f64;

                        // Record metrics.
                        metrics.run_duration.observe(duration_secs);
                        metrics.active_workers.dec();

                        if outcome.exit_code == 0 {
                            metrics.runs_success.inc();
                        } else {
                            metrics.runs_failure.inc();
                        }

                        // Count PRs created from worker result.
                        if let Some(ref result) = outcome.result {
                            if result.pr_url.is_some() {
                                metrics.prs_created.inc();
                            }
                            // Also count PRs from individual work items.
                            for item in &result.work_items {
                                if item.pr_url.is_some() {
                                    metrics.prs_created.inc();
                                }
                            }
                            metrics.run_cost.observe(result.total_cost_usd);
                        }

                        state.record_completed(RunRecord {
                            run_id,
                            repo_name: repo_name.clone(),
                            repo,
                            started_at,
                            finished_at,
                            exit_code: outcome.exit_code,
                            trigger,
                            result: outcome.result,
                        });
                        info!(
                            repo = %repo_name,
                            exit_code = outcome.exit_code,
                            duration_secs = duration_secs,
                            "worker completed"
                        );
                        return;
                    }
                    Ok(None) => {
                        // Still running
                    }
                    Err(e) => {
                        error!(repo = %repo_name, error = %e, "error checking worker status");
                        metrics.runs_failure.inc();
                        metrics.active_workers.dec();
                        state.remove_active(&repo_name);
                        return;
                    }
                }
            }
        });
    }
}

/// Parse a duration string like "30m", "1h", "1h30m", "90s" into a `Duration`.
/// Falls back to `DEFAULT_TIMEOUT_SECS` on failure.
fn parse_timeout_str(s: &str) -> Duration {
    let s = s.trim();
    if s.is_empty() {
        return Duration::from_secs(DEFAULT_TIMEOUT_SECS);
    }

    let mut total_secs: u64 = 0;
    let mut current_num = String::new();

    for c in s.chars() {
        if c.is_ascii_digit() {
            current_num.push(c);
        } else {
            let n: u64 = match current_num.parse() {
                Ok(v) => v,
                Err(_) => return Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            };
            current_num.clear();

            let secs = match c {
                'h' | 'H' => n * 3600,
                'm' | 'M' => n * 60,
                's' | 'S' => n,
                _ => return Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            };
            total_secs += secs;
        }
    }

    // Bare number with no suffix: treat as seconds.
    if !current_num.is_empty() {
        if let Ok(n) = current_num.parse::<u64>() {
            total_secs += n;
        }
    }

    if total_secs == 0 {
        Duration::from_secs(DEFAULT_TIMEOUT_SECS)
    } else {
        Duration::from_secs(total_secs)
    }
}

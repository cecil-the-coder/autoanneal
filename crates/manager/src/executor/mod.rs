use async_trait::async_trait;
use anyhow::Result;
use std::time::Duration;

use crate::config::WorkerDefaults;
use crate::config::RepoEntry;
use crate::state::TriggerReason;
use autoanneal_lib::result::WorkerResult;

#[derive(Debug, Clone)]
pub struct PendingRun {
    pub run_id: String,
    pub repo_entry: RepoEntry,
    pub defaults: WorkerDefaults,
    pub worker_image: String,
    pub trigger: TriggerReason,
    pub result_path: String,
}

#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub exit_code: i32,
    pub duration: Duration,
    pub result: Option<WorkerResult>,
}

#[async_trait]
pub trait Executor: Send + Sync {
    async fn launch(&self, run: &PendingRun) -> Result<()>;
    async fn is_running(&self, repo_name: &str, run_id: &str) -> Result<bool>;
    async fn collect(&self, repo_name: &str, run_id: &str) -> Result<Option<RunOutcome>>;
    async fn cancel(&self, repo_name: &str, run_id: &str) -> Result<()>;
}

pub mod docker;
pub mod kubernetes;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_run_record_default() {
        let outcome = RunOutcome {
            exit_code: 0,
            duration: Duration::from_secs(0),
            result: None,
        };
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.duration, Duration::from_secs(0));
        assert!(outcome.result.is_none());
    }
}

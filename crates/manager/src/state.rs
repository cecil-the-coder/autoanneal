use autoanneal_lib::result::WorkerResult;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TriggerReason {
    Scheduled,
    Webhook { event: String, ref_or_id: Option<String> },
    Manual,
}

#[derive(Debug, Clone)]
pub struct ActiveRun {
    pub run_id: String,
    pub repo_name: String,
    pub started_at: DateTime<Utc>,
    pub trigger: TriggerReason,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub run_id: String,
    pub repo_name: String,
    pub repo: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub exit_code: i32,
    pub trigger: TriggerReason,
    pub result: Option<WorkerResult>,
}

pub struct StateStore {
    active_runs: DashMap<String, ActiveRun>,
    recent_runs: RwLock<VecDeque<RunRecord>>,
    history_limit: usize,
}

impl StateStore {
    pub fn new(history_limit: usize) -> Self {
        Self {
            active_runs: DashMap::new(),
            recent_runs: RwLock::new(VecDeque::new()),
            history_limit,
        }
    }

    pub fn insert_active(&self, run: ActiveRun) {
        self.active_runs.insert(run.repo_name.clone(), run);
    }

    pub fn remove_active(&self, repo_name: &str) -> Option<ActiveRun> {
        self.active_runs.remove(repo_name).map(|(_, v)| v)
    }

    pub fn is_active(&self, repo_name: &str) -> bool {
        self.active_runs.contains_key(repo_name)
    }

    pub fn active_count(&self) -> usize {
        self.active_runs.len()
    }

    pub fn active_runs(&self) -> Vec<ActiveRun> {
        self.active_runs.iter().map(|r| r.value().clone()).collect()
    }

    pub fn record_completed(&self, record: RunRecord) {
        let mut runs = self.recent_runs.write().unwrap();
        if runs.len() >= self.history_limit {
            runs.pop_front();
        }
        runs.push_back(record);
    }

    pub fn recent_runs(&self) -> Vec<RunRecord> {
        self.recent_runs.read().unwrap().iter().cloned().collect()
    }
}

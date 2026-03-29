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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_active_run(repo: &str) -> ActiveRun {
        ActiveRun {
            run_id: format!("run-{repo}"),
            repo_name: repo.into(),
            started_at: Utc::now(),
            trigger: TriggerReason::Scheduled,
        }
    }

    fn make_run_record(repo: &str, idx: u32) -> RunRecord {
        RunRecord {
            run_id: format!("run-{repo}-{idx}"),
            repo_name: repo.into(),
            repo: format!("owner/{repo}"),
            started_at: Utc::now(),
            finished_at: Utc::now(),
            exit_code: 0,
            trigger: TriggerReason::Scheduled,
            result: None,
        }
    }

    #[test]
    fn test_add_and_check_active() {
        let store = StateStore::new();
        let run = make_active_run("my-repo");
        store.insert_active(run);
        assert!(store.is_active("my-repo"));
        assert!(!store.is_active("other-repo"));
        assert_eq!(store.active_count(), 1);
    }

    #[test]
    fn test_remove_active() {
        let store = StateStore::new();
        store.insert_active(make_active_run("my-repo"));
        assert!(store.is_active("my-repo"));

        let removed = store.remove_active("my-repo");
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().repo_name, "my-repo");
        assert!(!store.is_active("my-repo"));
        assert_eq!(store.active_count(), 0);

        // Removing again returns None
        assert!(store.remove_active("my-repo").is_none());
    }

    #[test]
    fn test_add_history() {
        let store = StateStore::new();
        store.record_completed(make_run_record("repo-a", 1));
        store.record_completed(make_run_record("repo-b", 2));

        let runs = store.recent_runs();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].repo_name, "repo-a");
        assert_eq!(runs[1].repo_name, "repo-b");
    }

    #[test]
    fn test_history_limit() {
        let store = StateStore::new();
        for i in 0..150 {
            store.record_completed(make_run_record("repo", i));
        }
        let runs = store.recent_runs();
        assert_eq!(runs.len(), 100);
        // Oldest entries should have been evicted; first run_id should be run-repo-50
        assert_eq!(runs[0].run_id, "run-repo-50");
        assert_eq!(runs[99].run_id, "run-repo-149");
    }

    #[test]
    fn test_recent_runs() {
        let store = StateStore::new();
        store.record_completed(make_run_record("first", 1));
        store.record_completed(make_run_record("second", 2));
        store.record_completed(make_run_record("third", 3));

        let runs = store.recent_runs();
        // Returns in insertion order (chronological)
        assert_eq!(runs[0].repo_name, "first");
        assert_eq!(runs[1].repo_name, "second");
        assert_eq!(runs[2].repo_name, "third");
    }

    #[test]
    fn test_concurrent_access() {
        use std::sync::Arc;
        let store = Arc::new(StateStore::new());
        let mut handles = vec![];

        // Spawn writers for active runs
        for i in 0..10 {
            let s = store.clone();
            handles.push(std::thread::spawn(move || {
                let run = ActiveRun {
                    run_id: format!("run-{i}"),
                    repo_name: format!("repo-{i}"),
                    started_at: Utc::now(),
                    trigger: TriggerReason::Scheduled,
                };
                s.insert_active(run);
            }));
        }

        // Spawn writers for history
        for i in 0..10 {
            let s = store.clone();
            handles.push(std::thread::spawn(move || {
                s.record_completed(make_run_record(&format!("hist-{i}"), i));
            }));
        }

        // Spawn readers
        for _ in 0..10 {
            let s = store.clone();
            handles.push(std::thread::spawn(move || {
                let _ = s.active_count();
                let _ = s.recent_runs();
                let _ = s.is_active("repo-0");
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(store.active_count(), 10);
        assert_eq!(store.recent_runs().len(), 10);
    }
}

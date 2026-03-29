use async_trait::async_trait;
use anyhow::Result;

use super::{Executor, PendingRun, RunOutcome};

pub struct KubernetesExecutor {
    _namespace: String,
}

impl KubernetesExecutor {
    pub fn new(namespace: &str) -> Self {
        Self { _namespace: namespace.to_string() }
    }
}

#[async_trait]
impl Executor for KubernetesExecutor {
    async fn launch(&self, _run: &PendingRun) -> Result<()> {
        anyhow::bail!("kubernetes executor not yet implemented")
    }

    async fn is_running(&self, _repo_name: &str, _run_id: &str) -> Result<bool> {
        anyhow::bail!("kubernetes executor not yet implemented")
    }

    async fn collect(&self, _repo_name: &str, _run_id: &str) -> Result<Option<RunOutcome>> {
        anyhow::bail!("kubernetes executor not yet implemented")
    }

    async fn cancel(&self, _repo_name: &str, _run_id: &str) -> Result<()> {
        anyhow::bail!("kubernetes executor not yet implemented")
    }
}

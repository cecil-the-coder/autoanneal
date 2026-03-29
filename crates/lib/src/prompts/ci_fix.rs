pub const CI_FIX_PROMPT: &str = r#"CI is failing on pull request #{pr_number} (branch: {branch_name}).

## CI Information

The job summary below shows which jobs and steps failed, along with their job IDs. You can use the `gh_workflow_logs` tool with action `job_logs` and a job_id to retrieve full logs for any specific job.

{ci_logs}

## PR Context

**Title**: {pr_title}

## Instructions

Diagnose the CI failures from the logs and job summary above, find the root cause in the source code, and fix it. If you need more detail about a specific job's logs, use `gh_workflow_logs` with action `job_logs` and the job ID from the summary. CI will run the full verification after push."#;

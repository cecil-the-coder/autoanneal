pub const CI_FIX_PROMPT: &str = r#"CI is failing on pull request #{pr_number} (branch: {branch_name}).

## Failed CI Logs

```
{ci_logs}
```

## PR Context

**Title**: {pr_title}

## Instructions

Diagnose the CI failures from the logs above, find the root cause in the source code, and fix it. After fixing, verify with a lightweight check if possible (e.g., cargo check, not cargo build). CI will run the full verification after push."#;

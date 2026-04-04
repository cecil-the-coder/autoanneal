pub const PR_REVIEW_FIX_PROMPT: &str = r#"A code review found issues in pull request #{pr_number} (branch: {branch}).

## Critic Review

Score: {score}/10
Summary: {summary}

{tasks}

## Diff Under Review

```
{diff}
```

## Instructions

Fix the issues identified by the critic. Make minimal, focused changes. After fixing, verify with a lightweight check if possible. CI will run the full verification after push."#;

/// Prompt for fixing a single deduction (used in per-deduction loop).
pub const PR_REVIEW_FIX_SINGLE_DEDUCTION_PROMPT: &str = r#"A code review found an issue in pull request #{pr_number} (branch: {branch}).

## Issue to Fix

{deduction}

## Current Diff Under Review

```
{diff}
```

## Instructions

Fix this specific issue. Do not change anything else. Make minimal, focused changes that address only the described problem."#;

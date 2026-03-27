pub const PR_REVIEW_FIX_PROMPT: &str = r#"A code review found issues in pull request #{pr_number} (branch: {branch}).

## Critic Review

Score: {score}/10
Summary: {summary}

## Diff Under Review

```
{diff}
```

## Instructions

Fix the issues identified by the critic. Make minimal, focused changes. After fixing, verify the build works."#;

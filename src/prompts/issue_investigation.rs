pub const ISSUE_INVESTIGATION_PROMPT: &str = r#"A user has filed the following GitHub issue:

## Issue #{issue_number}: {issue_title}

{issue_body}

## Repository Context

{arch_summary}

## Build / Test Commands

- Build: {build_commands}
- Test: {test_commands}

## Instructions

1. Investigate the issue by reading the relevant source code.
2. Try to understand the root cause.
3. If you can fix it, implement the fix. Make minimal, focused changes.
4. Do NOT run build, test, lint, or format commands — CI will verify after push.
5. If you cannot fix it, summarize your investigation findings.

Output a JSON code block at the end:
```json
{
  "fixed": true,
  "summary": "What you found and what you did"
}
```"#;

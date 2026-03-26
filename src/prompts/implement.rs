pub const IMPLEMENT_PROMPT: &str = r#"You are implementing a specific improvement to a codebase. Make the change described below, then verify it compiles and passes tests.

## Task

**Title**: {task_title}
**Category**: {task_category}
**Description**: {task_description}

## Constraints

- **Allowed files**: You may ONLY modify the following files (you may also create new test files if needed):
{allowed_files}
- Do NOT modify CI/CD configuration files (.github/workflows/*, .gitlab-ci.yml, etc.)
- Do NOT add new dependencies to package manifests (Cargo.toml, package.json, go.mod, etc.)
- Make minimal, focused changes. Do not refactor unrelated code, rename unrelated variables, or reformat surrounding lines.
- Primary language: {primary_language}

## Verification

After making your changes, run the build and test commands to verify nothing is broken:

- Build: `{build_command}`
- Test: `{test_command}`

If the build or tests fail due to your changes, fix the issues before finishing.

## When Done

Write a brief summary (2-3 sentences) of exactly what you changed and why."#;

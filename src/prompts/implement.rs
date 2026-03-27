pub const IMPLEMENT_PROMPT: &str = r#"You are implementing a specific improvement to a codebase.

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
- If your changes affect public APIs, update relevant documentation (README, doc comments, etc.)
- Add brief doc comments to any new public functions or structs you create
- Do NOT run build, test, lint, or format commands. CI will verify everything after push.
- Primary language: {primary_language}

## When Done

Write a brief summary (2-3 sentences) of exactly what you changed and why."#;

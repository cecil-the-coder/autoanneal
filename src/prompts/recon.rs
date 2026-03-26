pub const RECON_PROMPT: &str = r#"You are analyzing a repository for an automated code improvement tool. Your goal is to produce a concise architecture summary that will guide subsequent analysis and implementation phases.

Explore the repository and identify:
- The primary programming language
- Build, test, and lint commands (check Makefiles, package.json scripts, Cargo.toml, pyproject.toml, etc.)
- Key directories and their purposes (e.g., src/, lib/, tests/, docs/)
- The overall architecture and structure of the project

Start by reading key files: README, main entry points, configuration files (package.json, Cargo.toml, go.mod, pyproject.toml, etc.), and CI workflow files. Browse the top-level directory listing and a few key subdirectories to understand the layout.

Once you have explored enough, output your findings as a JSON code block:

```json
{
  "summary": "2-3 paragraph architecture summary",
  "primary_language": "rust",
  "build_commands": ["cargo build"],
  "test_commands": ["cargo test"],
  "lint_commands": ["cargo clippy"],
  "key_directories": ["src/", "tests/"]
}
```

Be factual and specific — mention actual file paths, module names, and command names."#;

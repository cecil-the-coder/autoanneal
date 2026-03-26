# Prompt strategy

## Overview

Each phase that invokes Claude uses a dedicated prompt defined as a `const` string in `src/prompts/`. Prompts are injected with runtime context (architecture summary, open PRs, improvement details) before being passed to `claude -p`.

All Claude invocations use `--output-format json`, `--bare`, and a custom `--system-prompt` (defined in `src/prompts/system.rs`). Structured output is requested via prompt instructions with JSON code block examples. The response parser extracts JSON from the `result` text using multiple fallback strategies (direct parse, code-block extraction).

## Prompts by phase

### Recon (`src/prompts/recon.rs`)

**Purpose:** Produce an architecture summary and extract build/test/lint commands.

**Context injected:** None beyond the codebase itself. Claude has access to `Read`, `Glob`, `Grep`, and `Bash` to explore the repo.

**Configuration:** Low effort, 25 max turns, $0.50 budget.

**Structured output:** JSON code block in response text, parsed by the orchestrator.

### Analysis (`src/prompts/analysis.rs`)

**Purpose:** Identify concrete, actionable improvements in the codebase.

**Context injected:**
- The architecture summary from recon.
- List of open PRs (to avoid duplicating in-flight work).

**Configuration:** High effort, 50 max turns, 20% of remaining budget. Has access to `Agent` tool for parallel subagent exploration.

**Structured output:** JSON code block in response text. Lenient deserialization accepts common aliases (e.g., "medium" for "moderate" severity).

### Plan / PR body (`src/prompts/plan.rs`)

**Purpose:** Generate a PR title and markdown body from the improvement list.

**Context injected:**
- The full list of filtered improvements (titles, descriptions, severities).

**Configuration:** Low effort, 10 max turns, $0.10 budget, no tools.

**Structured output:** JSON code block in response text.

### Implement (`src/prompts/implement.rs`)

**Purpose:** Implement a single improvement task.

**Context injected:**
- The architecture summary.
- The specific improvement (title, description, files to modify, category).
- Build and test commands from recon.

**Configuration:** High effort, 100 max turns, budget divided across remaining tasks (capped at $1.50). Tasks run in parallel git worktrees. On timeout, sessions are resumed with a grace period.

**Tools:** Full access — `Read`, `Glob`, `Grep`, `Bash`, `Edit`, `Write`.

**Structured output:** No. The work is side effects (file edits). Only the response envelope is parsed for `is_error` and `total_cost_usd`.

### Build fix (`src/prompts/fix_build.rs`)

**Purpose:** Fix build failures after a task implementation.

**Context injected:**
- The build command that failed.
- The build error output (stderr/stdout).

**Configuration:** Same tool access as implement. Budget: $0.50 per attempt, max 2 attempts per task.

**Structured output:** No.

## Expected JSON output formats

JSON is requested via prompt instructions (not `--json-schema`). The parser is lenient — it accepts common aliases for enum values.

### Recon output

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

### Analysis output

```json
{
  "improvements": [
    {
      "title": "Short title",
      "description": "What to change and why",
      "severity": "minor|moderate|major",
      "category": "bug|performance|security|quality|testing|docs|error-handling",
      "files_to_modify": ["src/foo.rs"],
      "estimated_lines_changed": 50,
      "risk": "low|medium|high"
    }
  ]
}
```

Severity accepts: minor/low, moderate/medium, major/high/critical. Category accepts common aliases (bug_fix, refactor, etc.).

### PR body output

```json
{
  "title": "PR title, max 72 chars",
  "body": "Markdown PR body"
}
```

## Customizing prompts

The prompt strings live in `src/prompts/` as Rust `const` values:

```
src/prompts/
  mod.rs
  system.rs         # Per-phase system prompts (replaces Claude Code default)
  recon.rs          # const RECON_PROMPT
  analysis.rs       # const ANALYSIS_PROMPT
  plan.rs           # const PR_BODY_PROMPT
  implement.rs      # const IMPLEMENT_PROMPT
  fix_build.rs      # const FIX_BUILD_PROMPT
```

`system.rs` contains compact system prompts that replace Claude Code's default (which is optimized for interactive use). Each phase gets a system prompt with tool-use guidance and phase-specific directives.

To customize:

1. Edit the relevant `const` string in `src/prompts/`.
2. Runtime context is injected via `.replace()` — look for `{placeholder}` patterns in the prompt strings.
3. If you change the expected JSON output format, update the corresponding Rust struct in `src/models.rs`. Deserialization is lenient (custom `Deserialize` impls accept aliases).
4. The analysis prompt is the highest-leverage customization point. Steer it toward specific categories or adjust the do-not-suggest list.
5. Rebuild the binary after changes: `cargo build --release`.

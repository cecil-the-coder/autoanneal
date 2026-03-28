pub const ANALYSIS_PROMPT: &str = r#"You are analyzing a codebase to find concrete, implementable improvements. You have full read access to the repository.

## Repository Context

{arch_summary}

## Stack Information

{stack_info}

## Open Pull Requests (do NOT overlap with these)

{open_prs}

## Recently Merged Changes (do NOT revert these)

The following changes were recently merged. They represent intentional design decisions. Do NOT suggest changes that undo or contradict them:

{recent_commits}

## Your Task

Explore the codebase thoroughly and identify specific improvements that can be made. Focus on:

- **Bug fixes**: incorrect logic, off-by-one errors, race conditions, null/None handling mistakes
- **Missing error handling**: unwrapped results, unchecked return values, missing validation, silent failures
- **Edge cases**: boundary conditions, empty inputs, overflow, Unicode handling
- **Overengineered or bloated code**: unnecessary abstractions, dead code, redundant logic that can be simplified
- **Performance**: obviously inefficient algorithms, unnecessary allocations, N+1 patterns, missing caching for expensive operations
- **Security**: SQL injection, path traversal, command injection, hardcoded secrets, insecure defaults

Do NOT suggest:
- Stylistic or formatting changes (whitespace, naming conventions, import ordering)
- Changes that require adding new dependencies
- Changes that overlap with the open pull requests listed above
- Documentation-only changes unless they fix genuinely misleading information
- Broad refactors that touch many files without a clear functional benefit

Each improvement must be implementable by modifying fewer than 500 lines of code. Be specific: name the exact files, functions, and line ranges involved. Prefer high-confidence, low-risk changes where the current behavior is clearly wrong or clearly improvable.

## Time Budget

You have a LIMITED number of turns. Do NOT try to read every file in the project. Instead:
1. Read the most important files first (entry points, core logic, error handling paths).
2. Use Grep to search for patterns (unwrap(), TODO, unsafe, etc.) instead of reading files sequentially.
3. Stop exploring after ~25 turns and output your findings. Better to report 2-3 solid findings than to timeout with 0.
4. If you're running low on turns, output what you have immediately.

Once you have explored enough, output your findings as a JSON code block:

```json
{
  "improvements": [
    {
      "title": "Short title, max 80 chars",
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

If you find no improvements worth making, return `{"improvements": []}`."#;

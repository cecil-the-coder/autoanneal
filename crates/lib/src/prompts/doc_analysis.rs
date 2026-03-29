pub const DOC_ANALYSIS_PROMPT: &str = r#"You are analyzing a codebase for documentation improvements. Focus on:

- Missing or outdated README sections
- Undocumented public APIs or modules
- Misleading or incorrect examples
- Missing setup/installation instructions
- Missing architecture or design documentation
- Outdated configuration references

{arch_summary}

{stack_info}

## Recently Merged Changes (do NOT contradict these)

{recent_commits}

Do NOT suggest:
- Trivial comment additions (e.g., "// increment counter")
- Formatting-only changes
- Changes that duplicate existing documentation

Output a JSON code block with the same format as code improvements:
```json
{
  "improvements": [...]
}
```"#;

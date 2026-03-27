pub const CRITIC_PROMPT: &str = r#"Review the following code changes from a pull request.

## Diff

```
{diff}
```

## Review Criteria

Rate the changes 1-10 on:
- **Correctness**: Are the changes logically correct? Do they do what they claim?
- **Quality**: Is the code clean, well-structured, and consistent with the existing codebase?
- **Risk**: Could these changes break anything? Are edge cases handled?
- **Value**: Do the changes meaningfully improve the codebase?
- **Documentation**: Are the changes documented where needed?

## Output

Output a JSON code block:

```json
{
  "score": 7,
  "verdict": "approve|needs_work|reject",
  "summary": "Brief review summary"
}
```

- Score 8-10 + "approve": Changes are good, ready for human review
- Score 5-7 + "needs_work": Changes have issues but are salvageable
- Score 1-4 + "reject": Changes should be discarded"#;

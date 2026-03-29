pub const PR_BODY_PROMPT: &str = r#"Write a pull request title and markdown body for the following set of planned improvements.

## Planned Improvements

{improvements}

## Requirements

- **Title**: A clear, concise PR title (maximum 72 characters). Summarize the overall theme of the changes (e.g., "Fix error handling and edge cases in request parsing").
- **Body**: A well-structured markdown PR body containing:
  1. A **Summary** section (2-4 sentences) explaining what this PR does and why.
  2. A **Changes** section with a checklist (`- [ ]`) of each planned improvement, one per item. Each checklist item should be a single sentence describing the change.
  3. A **Risk Assessment** section briefly noting the overall risk level and any areas that reviewers should pay extra attention to.

Use a professional, concise tone. Do not use filler language or marketing speak. State what changes and why, nothing more.

Output as a JSON code block with two keys: "title" (string, max 72 chars) and "body" (string, full markdown).

Do NOT use placeholder text — write the actual title and body for these specific improvements."#;

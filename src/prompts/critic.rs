pub const CRITIC_PROMPT: &str = r#"Review the following code changes from a pull request.

## Diff

```
{diff}
```

## Primary Question: Is this worth doing?

Your most important job is deciding whether these changes have genuine value. Ask yourself:
- Do they fix a real bug that could actually happen in practice?
- Do they prevent a real security issue?
- Do they meaningfully improve performance in a measurable way?
- Do they reduce complexity in a way that helps maintainability?
- Or are they trivial/cosmetic changes that add noise without real benefit?

## Secondary: If worth doing, are they correct?

Only if the changes pass the "worth doing" bar, also check:
- Are the changes logically correct?
- Could they break anything?
- Are they complete (missing tests, docs updates, edge cases)?

## Output

Return a JSON object with your review:

- "score": integer from 1-10
- "verdict": one of "approve", "needs_work", or "reject"
- "summary": brief review summary

Scoring guide:
- Score 8-10 + "approve": Valuable changes, ready for human review
- Score 5-7 + "needs_work": Valuable changes but have fixable issues
- Score 1-4 + "reject": Not worth doing — trivial, cosmetic, or actively harmful"#;

pub const CRITIC_FIX_PROMPT: &str = r#"You previously reviewed a PR and found issues. Fix them now.

## Your Previous Review

Score: {score}/10
Summary: {review_summary}

## Current Diff

```
{diff}
```

## Instructions

Fix the issues you identified in your review. Common fixes include:
- Adding missing tests for new functionality
- Updating documentation to reflect changes
- Fixing incorrect logic or edge cases you spotted
- Reverting unnecessary changes (e.g., log level changes without justification)

Make minimal, focused changes. Only fix what your review identified — do not add new improvements."#;

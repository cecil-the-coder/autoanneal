#[allow(dead_code)]
pub const FIX_BUILD_PROMPT: &str = r#"A build or compilation failed after recent changes. Your only job is to fix the build errors. Do NOT make any other improvements, refactors, or unrelated changes.

## Build Errors

```
{build_errors}
```

## Constraints

- Fix ONLY the errors shown above. Do not make additional changes.
- You may ONLY modify the following files:
{allowed_files}
- Do NOT add new dependencies.
- Do NOT modify CI/CD configuration files.

Read the error messages carefully, identify the root cause in the allowed files, and apply the minimal fix to resolve each error. After fixing, verify with a lightweight check if possible (e.g., cargo check, not cargo build). CI will run the full verification after push."#;

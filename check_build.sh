#!/bin/bash
# Run cargo check to find compilation errors
cd /tmp/autoanneal-1774930207-1/.worktree-ci-fix-79
export CARGO_BUILD_JOBS=1
cargo check 2>&1 | head -200
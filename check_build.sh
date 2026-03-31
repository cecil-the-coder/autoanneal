#!/bin/bash
# Build script to check for errors

cd /tmp/autoanneal-1774921208-1/.worktree-ci-fix-79

echo "=== Running cargo check ==="
CARGO_BUILD_JOBS=1 cargo check 2>&1 | head -100

echo ""
echo "=== Checking if cargo check succeeded ==="
exit ${PIPESTATUS[0]}

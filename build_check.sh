#!/bin/bash
set -e
cd /tmp/autoanneal-1774979404-1/.worktree-ci-fix-79

# Try to build just the library first (faster)
echo "=== Checking library... ==="
cd crates/lib && cargo check 2>&1 | head -50

#!/bin/bash
cd /tmp/autoanneal-1774989604-1/.worktree-ci-fix-79
CARGO_BUILD_JOBS=1 cargo check -p autoanneal 2>&1 | head -100

#!/bin/bash
cd /tmp/autoanneal-1774979404-1/.worktree-ci-fix-79
cargo check -p autoanneal-lib 2>&1 | head -100

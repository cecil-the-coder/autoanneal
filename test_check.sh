#!/bin/bash
cd /tmp/autoanneal-1775014804-1/.worktree-ci-fix-79
cd crates/lib
cargo check --lib 2>&1 | head -100

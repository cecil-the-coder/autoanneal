#!/bin/bash
# Run cargo check to find compilation errors
cd "$(dirname "$0")"
export CARGO_BUILD_JOBS=1
cargo check 2>&1 | head -200

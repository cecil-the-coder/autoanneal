#!/bin/bash
# Build script with limited parallelism to avoid memory issues
cd "$(dirname "$0")"
export CARGO_BUILD_JOBS=1
cargo check -p autoanneal 2>&1 | head -300

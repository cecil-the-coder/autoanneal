#!/bin/bash
cd "$(dirname "$0")"
# Use limited parallelism to prevent OOM
CARGO_BUILD_JOBS=1 cargo check --package autoanneal-lib 2>&1 | head -50

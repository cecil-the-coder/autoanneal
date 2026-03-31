#!/bin/bash
cd "$(dirname "$0")"
# Use limited parallelism to prevent OOM
CARGO_BUILD_JOBS=1 cargo check 2>&1

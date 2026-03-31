#!/bin/bash
cd "$(dirname "$0")"
cargo check 2>&1 | head -100

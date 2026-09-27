#!/bin/sh
# Builds a headless SyncMe binary for Linux (no tray, so no GTK needed).
# Run from the project root. Output: dist/syncme
set -e
cargo build --release --no-default-features
mkdir -p dist
cp target/release/syncme dist/syncme
strip dist/syncme 2>/dev/null || true
echo "Built dist/syncme"

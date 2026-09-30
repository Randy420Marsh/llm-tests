#!/usr/bin/env sh
# Builds the release binary and copies it to dist/latency-tester
set -e
cd "$(dirname "$0")"
command -v cargo >/dev/null || { echo "Rust is not installed. Get it from https://rustup.rs"; exit 1; }
cargo build --release
mkdir -p dist
cp target/release/latency-tester dist/latency-tester
chmod +x dist/latency-tester
echo
echo "Done: $(pwd)/dist/latency-tester"
echo "Run the GUI with ./run.sh, or ./dist/latency-tester --cli for the headless mode."

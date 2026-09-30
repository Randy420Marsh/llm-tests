#!/usr/bin/env sh
# Builds if needed, then starts the GUI
cd "$(dirname "$0")"
[ -x dist/latency-tester ] || ./build.sh
exec ./dist/latency-tester "$@"

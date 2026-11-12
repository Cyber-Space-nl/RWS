#!/usr/bin/env bash
set -euo pipefail

target_dir=${CARGO_TARGET_DIR:-target}
binary=${1:-"$target_dir/release/rws-simple"}
config=${2:-./config.simple.example.toml}
[ -x "$binary" ] || { echo "Binary not executable: $binary" >&2; exit 1; }

echo "binary_bytes=$(stat -c %s "$binary")"
"$binary" --config "$config" >/tmp/rws-measure.log 2>&1 &
pid=$!
cleanup() { kill -TERM "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; }
trap cleanup EXIT
sleep 1
rss_kib=$(awk '/VmRSS:/ {print $2}' "/proc/$pid/status")
threads=$(awk '/Threads:/ {print $2}' "/proc/$pid/status")
echo "idle_rss_kib=$rss_kib"
echo "threads=$threads"
echo "pid=$pid"

#!/usr/bin/env bash
set -euo pipefail

workspace="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
target_dir="${RWS_BENCH_TARGET_DIR:-/tmp/rws-bench-smoke-target}"
output_dir="${RWS_BENCH_SMOKE_OUTPUT_DIR:-/tmp}"
cd "$workspace"

export CARGO_TARGET_DIR="$target_dir"
cargo build -p rws -p rws-bench

"$target_dir/debug/rws" --config config.dev.wsl.toml >"$output_dir/rws-smoke-server.log" 2>&1 &
server_pid=$!
trap 'kill "$server_pid" 2>/dev/null || true; wait "$server_pid" 2>/dev/null || true' EXIT

for _ in $(seq 1 50); do
    if curl -fsS -H 'Host: localhost' http://127.0.0.1:18080/index.html >/dev/null; then
        break
    fi
    sleep 0.1
done

"$target_dir/debug/rws-bench" run \
    --target http://127.0.0.1:18080/index.html \
    --host localhost \
    --label linux-smoke \
    --connections 100 \
    --active-percent 10 \
    --idle-duration 2 \
    --active-duration 2 \
    --rps 100 \
    --connect-rate 1000 \
    --request-timeout-ms 2000 \
    --output "$output_dir/rws-bench-linux-smoke.json"

"$target_dir/debug/rws-bench" report \
    "$output_dir/rws-bench-linux-smoke.json" \
    --output "$output_dir/RWS_BENCHMARK_SMOKE_REPORT.md"

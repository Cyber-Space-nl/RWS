#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."
config=${1:-config.dev.wsl.toml}
target_dir=${CARGO_TARGET_DIR:-target}
"$target_dir/release/rws-simple" --config "$config" >/tmp/rws-smoke.log 2>&1 &
pid=$!
cleanup() {
  kill -TERM "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
}
trap cleanup EXIT

ready=false
for _ in $(seq 1 30); do
  if curl -fsS http://127.0.0.1:19091/healthz >/dev/null 2>&1; then ready=true; break; fi
  sleep 0.1
done
if [ "$ready" != true ]; then cat /tmp/rws-smoke.log >&2; exit 1; fi

static_status=$(curl -sS -o /tmp/rws-body -w '%{http_code}' -H 'Host: localhost' http://127.0.0.1:18080/)
grep -q 'Rust WebServer works' /tmp/rws-body
unknown_status=$(curl -sS -o /dev/null -w '%{http_code}' -H 'Host: invalid.test' http://127.0.0.1:18080/)
health=$(curl -fsS http://127.0.0.1:19091/healthz | tr -d '\n')
"$target_dir/release/rws-ws-load" --url ws://localhost:18080/_rws/ws --connections 20 --messages 5 --payload-bytes 128
open_ws=$(curl -fsS http://127.0.0.1:19091/metrics | awk '/^rws_open_websockets / {print $2}')

kill -TERM "$pid"
wait "$pid"
trap - EXIT

echo "static_status=$static_status"
echo "unknown_host_status=$unknown_status"
echo "health=$health"
echo "open_websockets_after_test=$open_ws"
echo 'shutdown_ok=1'

test "$static_status" = 200
test "$unknown_status" = 421
test "$health" = ok
test "$open_ws" = 0

#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."
if [ "$#" -ne 0 ]; then echo "Usage: $0" >&2; exit 2; fi

if [ "${RWS_NATIVE_CPU:-0}" = 1 ]; then
  export RUSTFLAGS="${RUSTFLAGS:-} -C target-cpu=native"
fi

cargo build --locked --release -p rws
cargo build --locked --release -p rws-ws-load
cargo build --locked --release -p rws-bench

echo "Release build completed for RWS and its test tools."


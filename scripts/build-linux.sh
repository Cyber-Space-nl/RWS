#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."
variant=${1:-all}

if [ "${RSW_NATIVE_CPU:-0}" = 1 ]; then
  export RUSTFLAGS="${RUSTFLAGS:-} -C target-cpu=native"
fi

case "$variant" in
  simple) cargo build --locked --release -p rsw-simple ;;
  comfy) cargo build --locked --release -p rsw-comfy ;;
  all)
    cargo build --locked --release -p rsw-simple
    cargo build --locked --release -p rsw-comfy
    cargo build --locked --release -p rsw-ws-load
    ;;
  *) echo "Usage: $0 [simple|comfy|all]" >&2; exit 2 ;;
esac

echo "Release build completed for: $variant"


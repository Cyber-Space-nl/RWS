#!/usr/bin/env bash
set -euo pipefail

workspace="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$workspace"

cargo build --locked --release -p rws-bench
install -m 0755 target/release/rws-bench /usr/local/bin/rws-bench
echo "installed /usr/local/bin/rws-bench"

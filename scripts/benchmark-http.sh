#!/usr/bin/env bash
set -euo pipefail

url=${1:-http://127.0.0.1/robots.txt}
host=${2:-bench.local}
duration=${DURATION:-20s}
label=${LABEL:-unnamed}
available_threads=$(nproc)
max_threads=${THREADS:-4}
if (( available_threads < max_threads )); then max_threads=$available_threads; fi

command -v wrk >/dev/null || { echo "Install wrk first: apt install -y wrk" >&2; exit 1; }
curl -fsS -H "Host: $host" "$url" >/dev/null

pid=$(systemctl show rws --property MainPID --value 2>/dev/null || true)
echo "url=$url"
echo "host=$host"
echo "label=$label"
echo "duration=$duration"
echo "logical_cpus=$available_threads"
if [[ -n "$pid" && "$pid" != 0 && -r "/proc/$pid/status" ]]; then
    awk '/VmRSS:|Threads:/ {print "server_" tolower($1) "=" $2}' "/proc/$pid/status"
fi

warmup_threads=$max_threads
if (( warmup_threads > 32 )); then warmup_threads=32; fi
wrk -t"$warmup_threads" -c32 -d5s -H "Host: $host" "$url" >/dev/null

for connections in 1 10 50 100 250; do
    run_threads=$max_threads
    if (( run_threads > connections )); then run_threads=$connections; fi
    echo "connections=$connections threads=$run_threads"
    wrk --latency -t"$run_threads" -c"$connections" -d"$duration" \
        -H "Host: $host" -H "Accept: */*" "$url"
    sleep "${COOLDOWN_SECONDS:-2}"
done

if [[ -n "$pid" && "$pid" != 0 && -r "/proc/$pid/status" ]]; then
    awk '/VmRSS:|Threads:/ {print "server_after_" tolower($1) "=" $2}' "/proc/$pid/status"
fi

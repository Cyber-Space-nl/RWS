#!/usr/bin/env bash
set -euo pipefail

workspace="${RWS_WORKSPACE:-$(cd "$(dirname "$0")/../.." && pwd)}"
bench="$workspace/tools/rws-bench/dist/rws-bench-x86_64-linux"
results="$workspace/benchmark-results-20261027"
report="$workspace/RWS_NGINX_APACHE_BENCHMARK.html"
source_ip="$(curl -4fsS --max-time 10 https://api.ipify.org)"

mkdir -p "$results"
ulimit -n 10000

"$bench" check --target http://94.237.44.223/robots.txt --connections 1000 >"$results/generator-check.txt"

clear_redirect() {
    local attempt
    for attempt in 1 2 3 4 5; do
        if ssh -o BatchMode=yes -o ConnectTimeout=5 root@94.237.44.223 \
            "while iptables -t nat -C PREROUTING -p tcp --dport 80 -m comment --comment rws-bench-temp -j REDIRECT --to-ports 18081 2>/dev/null; do iptables -t nat -D PREROUTING -p tcp --dport 80 -m comment --comment rws-bench-temp -j REDIRECT --to-ports 18081; done; while iptables -t nat -C PREROUTING -p tcp --dport 80 -m comment --comment rws-bench-temp -j REDIRECT --to-ports 18082 2>/dev/null; do iptables -t nat -D PREROUTING -p tcp --dport 80 -m comment --comment rws-bench-temp -j REDIRECT --to-ports 18082; done; while iptables -C INPUT -p tcp -s $source_ip --dport 18081 -m comment --comment rws-bench-temp -j ACCEPT 2>/dev/null; do iptables -D INPUT -p tcp -s $source_ip --dport 18081 -m comment --comment rws-bench-temp -j ACCEPT; done; while iptables -C INPUT -p tcp -s $source_ip --dport 18082 -m comment --comment rws-bench-temp -j ACCEPT 2>/dev/null; do iptables -D INPUT -p tcp -s $source_ip --dport 18082 -m comment --comment rws-bench-temp -j ACCEPT; done"
        then
            return 0
        fi
        sleep 2
    done
    return 1
}

route_to() {
    local port="$1"
    clear_redirect
    if [[ "$port" != 80 ]]; then
        local attempt routed=false
        for attempt in 1 2 3 4 5; do
            if ssh -o BatchMode=yes -o ConnectTimeout=5 root@94.237.44.223 \
                "iptables -I INPUT 1 -p tcp -s $source_ip --dport $port -m comment --comment rws-bench-temp -j ACCEPT; iptables -t nat -I PREROUTING 1 -p tcp --dport 80 -m comment --comment rws-bench-temp -j REDIRECT --to-ports $port"
            then
                routed=true
                break
            fi
            sleep 2
        done
        [[ "$routed" == true ]] || return 1
    fi
    sleep 1
}

trap 'clear_redirect || true' EXIT

run_one() {
    local server="$1"
    local repetition="$2"
    local port process
    case "$server" in
        rws)
            port=80
            process=rsw-simple
            ;;
        nginx)
            port=18081
            process=nginx
            ;;
        apache)
            port=18082
            process=apache2
            ;;
        *)
            echo "unknown server: $server" >&2
            return 2
            ;;
    esac

    if [[ -s "$results/$server-$repetition.json" ]]; then
        echo "skipping completed $server repetition $repetition"
        return 0
    fi

    echo "starting $server repetition $repetition"
    route_to "$port"
    "$bench" run \
        --target http://94.237.44.223/robots.txt \
        --host bench.local \
        --label "$server" \
        --connections 1000 \
        --active-percent 10 \
        --idle-duration 15 \
        --active-duration 15 \
        --rps 10000 \
        --connect-rate 1000 \
        --request-timeout-ms 5000 \
        --monitor-ssh root@94.237.44.223 \
        --monitor-process "$process" \
        --monitor-port "$port" \
        --output "$results/$server-$repetition.json"
    sleep 2
}

run_one rws 1
run_one nginx 1
run_one apache 1

run_one apache 2
run_one nginx 2
run_one rws 2

run_one nginx 3
run_one rws 3
run_one apache 3

"$bench" report \
    "$results/rws-1.json" "$results/rws-2.json" "$results/rws-3.json" \
    "$results/nginx-1.json" "$results/nginx-2.json" "$results/nginx-3.json" \
    "$results/apache-1.json" "$results/apache-2.json" "$results/apache-3.json" \
    --output "$report"

clear_redirect
echo "report=$report"

#!/usr/bin/env bash
set -euo pipefail

rsw_url=${1:?Usage: verify-static-equivalence.sh RSW_URL NGINX_URL [HOST]}
nginx_url=${2:?Usage: verify-static-equivalence.sh RSW_URL NGINX_URL [HOST]}
host=${3:-bench.local}
work_dir=$(mktemp -d)
trap 'rm -rf -- "$work_dir"' EXIT

fetch() {
    local name=$1
    local url=$2
    curl --http1.1 --fail --silent --show-error \
        -H "Host: $host" -H "Accept: */*" \
        --dump-header "$work_dir/$name.headers" \
        --output "$work_dir/$name.body" "$url"
}

header_value() {
    local file=$1
    local name=$2
    awk -v wanted="$name" '
        BEGIN { IGNORECASE=1 }
        $0 ~ "^" wanted ":[[:space:]]*" {
            sub("^[^:]+:[[:space:]]*", "")
            sub("\\r$", "")
            print
            exit
        }
    ' "$file"
}

fetch rsw "$rsw_url"
fetch nginx "$nginx_url"
cmp --silent "$work_dir/rsw.body" "$work_dir/nginx.body" || {
    echo "ERROR: response bodies differ" >&2
    exit 1
}

for header in Content-Type Content-Length X-Content-Type-Options; do
    rsw_value=$(header_value "$work_dir/rsw.headers" "$header")
    nginx_value=$(header_value "$work_dir/nginx.headers" "$header")
    if [[ "$rsw_value" != "$nginx_value" ]]; then
        echo "ERROR: $header differs: RSW='$rsw_value' Nginx='$nginx_value'" >&2
        exit 1
    fi
done

echo "Bodies and semantic static headers match."
sha256sum "$work_dir/rsw.body"
echo "RSW headers:"
sed -n '1,/^\r$/p' "$work_dir/rsw.headers"
echo "Nginx headers:"
sed -n '1,/^\r$/p' "$work_dir/nginx.headers"

#!/usr/bin/env bash
set -euo pipefail

if [ "$(uname -s)" != Linux ]; then echo "This installer supports Linux only." >&2; exit 1; fi
if [ "${EUID:-$(id -u)}" -ne 0 ]; then echo "Run with sudo." >&2; exit 1; fi

project_root=$(cd "$(dirname "$0")/.." && pwd)
binary="$project_root/target/release/rws-simple"
config_source="$project_root/config.simple.example.toml"
service_source="$project_root/packaging/rws-simple.service"
logrotate_source="$project_root/packaging/rws-simple.logrotate"

[ -x "$binary" ] || { echo "Missing release binary. Run: scripts/build-linux.sh simple" >&2; exit 1; }

backup_existing() {
  local path=$1
  if [ -e "$path" ]; then
    cp -a "$path" "$path.backup.$(date +%Y%m%d%H%M%S)"
  fi
}

getent group rws >/dev/null || groupadd --system rws
id rws >/dev/null 2>&1 || useradd --system --gid rws --home-dir /var/lib/rws --create-home --shell /usr/sbin/nologin rws
getent group www-data >/dev/null && usermod -a -G www-data rws
install -d -m 0755 /etc/rws-simple /etc/rws-simple/tls /var/www/example.test/public /usr/share/doc/rws-simple
backup_existing /usr/local/bin/rws-simple
backup_existing /etc/systemd/system/rws-simple.service
install -m 0755 "$binary" /usr/local/bin/rws-simple
install -m 0644 "$service_source" /etc/systemd/system/rws-simple.service
install -m 0644 "$logrotate_source" /etc/logrotate.d/rws-simple
if [ ! -e /etc/rws-simple/config.toml ]; then install -m 0640 -o root -g rws "$config_source" /etc/rws-simple/config.toml; fi
if [ ! -e /var/www/example.test/public/index.html ]; then install -m 0644 "$project_root/examples/site/index.html" /var/www/example.test/public/index.html; fi
install -m 0644 "$project_root/README.md" /usr/share/doc/rws-simple/README.md
systemctl daemon-reload

echo "rws-simple installed but not started."
echo "Edit /etc/rws-simple/config.toml, then run:"
echo "  /usr/local/bin/rws-simple --config /etc/rws-simple/config.toml --check"
echo "  systemctl enable --now rws-simple"


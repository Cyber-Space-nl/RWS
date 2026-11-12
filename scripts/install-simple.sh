#!/usr/bin/env bash
set -euo pipefail

if [ "$(uname -s)" != Linux ]; then echo "This installer supports Linux only." >&2; exit 1; fi
if [ "${EUID:-$(id -u)}" -ne 0 ]; then echo "Run with sudo." >&2; exit 1; fi

project_root=$(cd "$(dirname "$0")/.." && pwd)
binary="$project_root/target/release/rsw-simple"
config_source="$project_root/config.simple.example.toml"
service_source="$project_root/packaging/rsw-simple.service"
logrotate_source="$project_root/packaging/rsw-simple.logrotate"

[ -x "$binary" ] || { echo "Missing release binary. Run: scripts/build-linux.sh simple" >&2; exit 1; }

backup_existing() {
  local path=$1
  if [ -e "$path" ]; then
    cp -a "$path" "$path.backup.$(date +%Y%m%d%H%M%S)"
  fi
}

getent group rsw >/dev/null || groupadd --system rsw
id rsw >/dev/null 2>&1 || useradd --system --gid rsw --home-dir /var/lib/rsw --create-home --shell /usr/sbin/nologin rsw
getent group www-data >/dev/null && usermod -a -G www-data rsw
install -d -m 0755 /etc/rsw-simple /etc/rsw-simple/tls /var/www/example.test/public /usr/share/doc/rsw-simple
backup_existing /usr/local/bin/rsw-simple
backup_existing /etc/systemd/system/rsw-simple.service
install -m 0755 "$binary" /usr/local/bin/rsw-simple
install -m 0644 "$service_source" /etc/systemd/system/rsw-simple.service
install -m 0644 "$logrotate_source" /etc/logrotate.d/rsw-simple
if [ ! -e /etc/rsw-simple/config.toml ]; then install -m 0640 -o root -g rsw "$config_source" /etc/rsw-simple/config.toml; fi
if [ ! -e /var/www/example.test/public/index.html ]; then install -m 0644 "$project_root/examples/site/index.html" /var/www/example.test/public/index.html; fi
install -m 0644 "$project_root/README.md" /usr/share/doc/rsw-simple/README.md
systemctl daemon-reload

echo "rsw-simple installed but not started."
echo "Edit /etc/rsw-simple/config.toml, then run:"
echo "  /usr/local/bin/rsw-simple --config /etc/rsw-simple/config.toml --check"
echo "  systemctl enable --now rsw-simple"


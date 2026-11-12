#!/usr/bin/env bash
set -euo pipefail

if [ "$(uname -s)" != Linux ]; then echo "This installer supports Linux only." >&2; exit 1; fi
if [ "${EUID:-$(id -u)}" -ne 0 ]; then echo "Run with sudo." >&2; exit 1; fi

project_root=$(cd "$(dirname "$0")/.." && pwd)
binary="$project_root/target/release/rsw-comfy"
config_source="$project_root/config.comfy.example.toml"
service_source="$project_root/packaging/rsw-comfy.service"
logrotate_source="$project_root/packaging/rsw-comfy.logrotate"

[ -x "$binary" ] || { echo "Missing release binary. Run: scripts/build-linux.sh comfy" >&2; exit 1; }

backup_existing() {
  local path=$1
  if [ -e "$path" ]; then cp -a "$path" "$path.backup.$(date +%Y%m%d%H%M%S)"; fi
}

getent group rsw >/dev/null || groupadd --system rsw
id rsw >/dev/null 2>&1 || useradd --system --gid rsw --home-dir /var/lib/rsw --create-home --shell /usr/sbin/nologin rsw
getent group www-data >/dev/null && usermod -a -G www-data rsw
install -d -m 0755 /etc/rsw-comfy /etc/rsw-comfy/tls /var/www/example.test/public /usr/share/doc/rsw-comfy
backup_existing /usr/local/bin/rsw-comfy
backup_existing /etc/systemd/system/rsw-comfy.service
install -m 0755 "$binary" /usr/local/bin/rsw-comfy
install -m 0644 "$service_source" /etc/systemd/system/rsw-comfy.service
install -m 0644 "$logrotate_source" /etc/logrotate.d/rsw-comfy
if [ ! -e /etc/rsw-comfy/config.toml ]; then install -m 0640 -o root -g rsw "$config_source" /etc/rsw-comfy/config.toml; fi
if [ ! -e /var/www/example.test/public/index.html ]; then install -m 0644 "$project_root/examples/site/index.html" /var/www/example.test/public/index.html; fi
install -m 0644 "$project_root/README.md" /usr/share/doc/rsw-comfy/README.md
systemctl daemon-reload

echo "rsw-comfy installed but not started."
echo "Edit /etc/rsw-comfy/config.toml, then run:"
echo "  /usr/local/bin/rsw-comfy --config /etc/rsw-comfy/config.toml --check"
echo "  systemctl enable --now rsw-comfy"


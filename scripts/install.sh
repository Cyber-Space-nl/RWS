#!/usr/bin/env bash
set -euo pipefail

if [ "$(uname -s)" != Linux ]; then echo "This installer supports Linux only." >&2; exit 1; fi
if [ "${EUID:-$(id -u)}" -ne 0 ]; then echo "Run with sudo." >&2; exit 1; fi

project_root=$(cd "$(dirname "$0")/.." && pwd)
binary="$project_root/target/release/rws"
config_source="$project_root/config.example.toml"
service_source="$project_root/packaging/rws.service"
logrotate_source="$project_root/packaging/rws.logrotate"

[ -x "$binary" ] || { echo "Missing release binary. Run: scripts/build-linux.sh" >&2; exit 1; }

if systemctl is-active --quiet rws-simple; then
  echo "Service rws-simple is active. Stop it before installing RWS." >&2
  exit 1
fi

backup_existing() {
  local path=$1
  if [ -e "$path" ]; then
    cp -a "$path" "$path.backup.$(date +%Y%m%d%H%M%S)"
  fi
}

getent group rws >/dev/null || groupadd --system rws
id rws >/dev/null 2>&1 || useradd --system --gid rws --home-dir /var/lib/rws --create-home --shell /usr/sbin/nologin rws
getent group www-data >/dev/null && usermod -a -G www-data rws
install -d -m 0755 /etc/rws /etc/rws/tls /var/www/example.test/public /usr/share/doc/rws
backup_existing /usr/local/bin/rws
backup_existing /etc/systemd/system/rws.service
backup_existing /etc/logrotate.d/rws
install -m 0755 "$binary" /usr/local/bin/rws
install -m 0644 "$service_source" /etc/systemd/system/rws.service
install -m 0644 "$logrotate_source" /etc/logrotate.d/rws
if [ ! -e /etc/rws/config.toml ]; then install -m 0640 -o root -g rws "$config_source" /etc/rws/config.toml; fi
if [ ! -e /var/www/example.test/public/index.html ]; then install -m 0644 "$project_root/examples/site/index.html" /var/www/example.test/public/index.html; fi
install -m 0644 "$project_root/README.md" /usr/share/doc/rws/README.md
systemctl daemon-reload

echo "rws installed but not started."
echo "Edit /etc/rws/config.toml, then run:"
echo "  /usr/local/bin/rws --config /etc/rws/config.toml --check"
echo "  systemctl enable --now rws"

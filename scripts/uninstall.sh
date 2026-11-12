#!/usr/bin/env bash
set -euo pipefail

if [ "${EUID:-$(id -u)}" -ne 0 ]; then echo "Run with sudo." >&2; exit 1; fi
variant=${1:-}
case "$variant" in simple|comfy) ;; *) echo "Usage: $0 simple|comfy" >&2; exit 2 ;; esac

service="rsw-$variant"
systemctl disable --now "$service" 2>/dev/null || true
rm -f "/etc/systemd/system/$service.service" "/usr/local/bin/$service" "/etc/logrotate.d/$service"
systemctl daemon-reload

echo "$service software removed."
echo "Configuration in /etc/$service and website files in /var/www were preserved."
echo "Logs in /var/log/$service were preserved."


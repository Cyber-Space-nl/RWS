#!/usr/bin/env bash
set -euo pipefail

if [ "${EUID:-$(id -u)}" -ne 0 ]; then echo "Run with sudo." >&2; exit 1; fi
if [ "$#" -ne 0 ]; then echo "Usage: $0" >&2; exit 2; fi

service="rws"
systemctl disable --now "$service" 2>/dev/null || true
rm -f "/etc/systemd/system/$service.service" "/usr/local/bin/$service" "/etc/logrotate.d/$service"
systemctl daemon-reload

echo "$service software removed."
echo "Configuration in /etc/$service and website files in /var/www were preserved."
echo "Logs in /var/log/$service were preserved."


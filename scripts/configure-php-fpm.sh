#!/usr/bin/env bash
set -euo pipefail

if [ "$(uname -s)" != Linux ]; then echo "This helper supports Linux only." >&2; exit 1; fi
if [ "${EUID:-$(id -u)}" -ne 0 ]; then echo "Run with sudo." >&2; exit 1; fi
if [ "$#" -eq 0 ]; then
  echo "Usage: sudo bash scripts/configure-php-fpm.sh php8.3-fpm.service [other-local-fpm.service ...]" >&2
  exit 1
fi

if [ "$(systemctl show --property=LoadState --value rws.service)" != loaded ]; then
  echo "Install the RWS service first." >&2
  exit 1
fi

units=()
for unit in "$@"; do
  if [[ ! "$unit" =~ ^[A-Za-z0-9][A-Za-z0-9_.@:-]*\.service$ ]] || [ "$unit" = rws.service ]; then
    echo "Invalid PHP-FPM service name: $unit" >&2
    exit 1
  fi
  if [ "$(systemctl show --property=LoadState --value "$unit")" != loaded ]; then
    echo "Service not found: $unit" >&2
    exit 1
  fi
  for existing in "${units[@]}"; do
    if [ "$existing" = "$unit" ]; then continue 2; fi
  done
  units+=("$unit")
done

directory=/etc/systemd/system/rws.service.d
dropin="$directory/20-rws-php-fpm.conf"
install -d -m 0755 "$directory"
temporary=$(mktemp "$directory/.rws-php-fpm.XXXXXXXX")
trap 'rm -f -- "$temporary"' EXIT
printf '# Created by RWS configure-php-fpm.sh.\n[Unit]\nWants=%s\nAfter=%s\n' "${units[*]}" "${units[*]}" > "$temporary"
chmod 0644 "$temporary"
if [ -L "$dropin" ]; then
  echo "Refusing to replace symlink: $dropin" >&2
  exit 1
elif [ -e "$dropin" ]; then
  if ! cmp -s "$temporary" "$dropin"; then
    echo "Existing $dropin differs. Review and edit it manually; it was not changed." >&2
    exit 1
  fi
else
  ln "$temporary" "$dropin"
fi
systemctl daemon-reload
echo "Configured local PHP-FPM ordering in $dropin."
echo "Ensure each php_fpm socket in /etc/rws/config.toml matches its service, then restart RWS when ready."

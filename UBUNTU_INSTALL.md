# RWS — Ubuntu 24.04 installation

RWS provides one web-server binary, `rws`. Static files, PHP-FPM and WebSocket echo are available;
HTTP reverse proxying and adaptive WebSocket scheduling are configured only when needed. The
`rws-ws-load` and `rws-bench` binaries are development tools, not additional servers.

The locked workspace requires Rust 1.88 or newer and Linux 5.6 or newer for contained static-file
opening. The commands below install a system service. For a non-root local test, use the
[README quickstart](README.md#local-quickstart). Review [security limitations](docs/SECURITY_TESTING.md)
before exposing a service.

## Fresh installation

```bash
sudo apt update
sudo apt install -y build-essential curl pkg-config php8.3-fpm
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustup toolchain install stable
cd '/path/to/Rust WebServer'
chmod +x scripts/*.sh
cargo fmt --check
cargo test --locked --workspace
./scripts/build-linux.sh
sudo ./scripts/install.sh
sudoedit /etc/rws/config.toml
sudo /usr/local/bin/rws --config /etc/rws/config.toml --check
sudo systemctl enable --now rws
curl -i -H 'Host: example.test' http://127.0.0.1:8080/
```

The installer leaves the service stopped, preserves an existing `/etc/rws/config.toml`, and backs
up existing `rws` binary, unit and logrotate files. It refuses to run while `rws-simple` or
`rws-comfy` is active. The example enables PHP-FPM; set `php = false` when it is not needed.
`--check` does not verify runtime access as user `rws`, port availability, PHP-FPM connectivity or
certificate trust. Inspect `systemctl status rws` and the journal after starting.

## Migrating from Simple or Comfy

The unified build does **not** automatically modify or remove an older installation. Its binary,
unit, config and logs use `/usr/local/bin/rws`, `rws.service`, `/etc/rws/` and `/var/log/rws/`;
the previous variants use separate paths. The default ports overlap. Plan the migration and
rollback during an appropriate maintenance window, with verified backups of binaries, units,
configuration, TLS keys, logrotate settings and site data.

1. Choose the existing service to migrate (`rws-simple` or `rws-comfy`). Record its service state,
   paths and configuration. If both are in use, plan each site and port explicitly; the installer
   will not stop either service for you.
2. Build the unified binary, stop the legacy service, and run `sudo ./scripts/install.sh`. The
   installer will still refuse installation if the other legacy service is active.
3. Copy or merge the old configuration into `/etc/rws/config.toml`, then review it. In particular,
   update per-site log paths to `/var/log/rws/` or ensure their existing paths remain accessible,
   and verify TLS key paths and permissions. Proxy and adaptive settings from Comfy remain
   supported. Simple's static configuration is also accepted.
4. Run `sudo /usr/local/bin/rws --config /etc/rws/config.toml --check`, start the new service with
   `sudo systemctl start rws`, and verify HTTP, HTTPS, PHP-FPM, WebSockets, proxying and logs as
   applicable. `--check` alone does not prove runtime success.
5. Once the new service is confirmed, disable the legacy service and enable `rws` at boot. Preserve
   the old installation and backups until rollback is no longer needed.

To roll back, stop and disable `rws`, restore any separately changed configuration and site data,
then enable and start the original service. Verify its status, routing, TLS, PHP and logs. Do not
remove the legacy unit or binary until a tested rollback is no longer required.

## Operations

```bash
sudo systemctl status rws
sudo journalctl -u rws -f
curl -s http://127.0.0.1:9091/metrics
sudo systemctl restart rws
sudo systemctl stop rws
```

See [README.md](README.md) for TLS, configuration, PHP-FPM, WebSockets, benchmarks, upgrade
backups and the separate development tools.

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
up existing `rws` binary, unit and logrotate files. It refuses to run while `rws-simple` is active.
The example enables PHP-FPM; set `php = false` when it is not needed.
`--check` does not verify runtime access as user `rws`, port availability, PHP-FPM connectivity or
certificate trust. Inspect `systemctl status rws` and the journal after starting.

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

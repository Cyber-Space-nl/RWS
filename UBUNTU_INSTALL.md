# Rust WebServer (RWS) — Ubuntu 24.04 installation

## Components

- **`rws-core`** — shared server library; it is linked into the binaries and is not installed separately.
- **`rws-simple`** — recommended lightweight server for the standard installation.
- **`rws-comfy`** — extended server with additional configuration, reverse proxy support and optional adaptive tuning.
- **`rws-ws-load`** — WebSocket load-testing tool for development and performance testing; it is not installed as a systemd service.

Install either `rws-simple` or `rws-comfy` as the active server. Their default configurations use the same ports, so do not run both simultaneously unless you change those ports.

The locked workspace requires Rust 1.88 or newer; static-file containment requires Linux 5.6 or
newer. These instructions install a system service; for a non-root local test use the README
quickstart. Review [security limitations](docs/SECURITY_TESTING.md) before exposing a service.

> **Naming change:** current packages, binaries, services, configuration directories, metrics and
> the WebSocket endpoint use the `rws` prefix. Existing pre-rename installations are not migrated
> automatically. Stop the previous service and carry over only reviewed configuration before
> enabling a current `rws-*` service on the same ports.

## Simple (recommended default)

```bash
sudo apt update
sudo apt install -y build-essential curl pkg-config php8.3-fpm
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustup toolchain install stable
cd '/path/to/Rust WebServer'
chmod +x scripts/*.sh
cargo fmt --check
cargo test --workspace
./scripts/build-linux.sh simple
sudo ./scripts/install-simple.sh
sudoedit /etc/rws-simple/config.toml
sudo /usr/local/bin/rws-simple --config /etc/rws-simple/config.toml --check
sudo systemctl enable --now rws-simple
curl -i -H 'Host: example.test' http://127.0.0.1:8080/
```

The installer leaves the service stopped and preserves an existing config. `--check` does not
verify runtime access as user `rws`, port availability, PHP-FPM connectivity or certificate trust;
inspect `systemctl status` and the journal after starting. The default example enables PHP-FPM;
if it is not needed, set `php = false` in the site configuration.

## Comfy

Stop Simple before enabling Comfy when both configurations use the default ports:

```bash
cd '/path/to/Rust WebServer'
./scripts/build-linux.sh comfy
sudo ./scripts/install-comfy.sh
sudoedit /etc/rws-comfy/config.toml
sudo /usr/local/bin/rws-comfy --config /etc/rws-comfy/config.toml --check
sudo systemctl disable --now rws-simple 2>/dev/null || true
sudo systemctl enable --now rws-comfy
```

## Operations

```bash
sudo systemctl status rws-simple
sudo journalctl -u rws-simple -f
curl -s http://127.0.0.1:9091/metrics
sudo systemctl restart rws-simple
sudo systemctl stop rws-simple
```

Replace `rws-simple` with `rws-comfy` when operating the Comfy installation.

## WebSocket load test

Build and run the test tool from the project directory:

```bash
cargo run -p rws-ws-load -- \
  --url ws://example.test:8080/_rws/ws \
  --connections 1000 \
  --messages 10 \
  --payload-bytes 128 \
  --idle-seconds 5
```

Tests above 10,000 connections require `--allow-large-test`. Prepare file-descriptor, memory, port-range and load-generator capacity before running large tests.

See `README.md` for TLS, PHP-FPM, WebSockets, measurement, rollback and limitations.


# Rust WebServer (RSW) — Ubuntu 24.04 installation

## Components

- **`rsw-core`** — shared server library; it is linked into the binaries and is not installed separately.
- **`rsw-simple`** — recommended lightweight server for the standard installation.
- **`rsw-comfy`** — extended server with additional configuration, reverse proxy support and optional adaptive tuning.
- **`rsw-ws-load`** — WebSocket load-testing tool for development and performance testing; it is not installed as a systemd service.

Install either `rsw-simple` or `rsw-comfy` as the active server. Their default configurations use the same ports, so do not run both simultaneously unless you change those ports.

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
sudoedit /etc/rsw-simple/config.toml
sudo /usr/local/bin/rsw-simple --config /etc/rsw-simple/config.toml --check
sudo systemctl enable --now rsw-simple
curl -i -H 'Host: example.test' http://127.0.0.1:8080/
```

## Comfy

Stop Simple before enabling Comfy when both configurations use the default ports:

```bash
cd '/path/to/Rust WebServer'
./scripts/build-linux.sh comfy
sudo ./scripts/install-comfy.sh
sudoedit /etc/rsw-comfy/config.toml
sudo /usr/local/bin/rsw-comfy --config /etc/rsw-comfy/config.toml --check
sudo systemctl disable --now rsw-simple 2>/dev/null || true
sudo systemctl enable --now rsw-comfy
```

## Operations

```bash
sudo systemctl status rsw-simple
sudo journalctl -u rsw-simple -f
curl -s http://127.0.0.1:9091/metrics
sudo systemctl restart rsw-simple
sudo systemctl stop rsw-simple
```

Replace `rsw-simple` with `rsw-comfy` when operating the Comfy installation.

## WebSocket load test

Build and run the test tool from the project directory:

```bash
cargo run -p rsw-ws-load -- \
  --url ws://example.test:8080/_rsw/ws \
  --connections 1000 \
  --messages 10 \
  --payload-bytes 128 \
  --idle-seconds 5
```

Tests above 10,000 connections require `--allow-large-test`. Prepare file-descriptor, memory, port-range and load-generator capacity before running large tests.

See `README.md` for TLS, PHP-FPM, WebSockets, measurement, rollback and limitations.


# rws-bench

`rws-bench` is a native Rust/Linux HTTP/1.1 keep-alive load generator and deterministic reporter
for repeatable RWS-versus-Nginx socket scalability tests. It uses Tokio tasks on a bounded set of
runtime worker threads, not one operating-system thread per socket.

## Build and install

Ubuntu 24.04 with Rust 1.85 or newer:

```bash
cargo build --locked --release -p rws-bench
sudo ./tools/rws-bench/install.sh
rws-bench --help
```

The binary is self-contained apart from normal Linux system libraries. Optional remote monitoring
uses the system `ssh` client and read-only Linux commands (`ps`, `/proc`, `ss`, and `nstat` when
available). Authentication remains in the user's SSH agent/config; passwords are never stored.

A validated stripped x86-64 Linux release is included at
`dist/rws-bench-x86_64-linux`; verify it with `sha256sum -c dist/SHA256SUMS`.

## Safety model

- More than 10,000 requested connections requires `--allow-large-test`.
- The hard limit is 1,000,000 connections.
- `check` only reads system limits and resolves the target; it never changes kernel settings.
- A run writes requested and actually opened connection counts separately.
- A result is warned as potentially generator-limited when achieved responses are below 90% of
  the configured offered rate.
- Start with a maximum of 100 connections. Do not run a large test against a public address until
  the target owner has explicitly approved that exact run and both machines have been prepared.

## Commands

Inspect the load generator before testing:

```bash
rws-bench check \
  --target http://94.237.44.223/robots.txt \
  --connections 100000 \
  --source-ip 192.0.2.10 \
  --source-ip 192.0.2.11
```

Small smoke test:

```bash
rws-bench run \
  --target http://127.0.0.1:8080/robots.txt \
  --host bench.local \
  --label smoke \
  --connections 100 \
  --active-percent 10 \
  --idle-duration 10 \
  --active-duration 10 \
  --rps 100 \
  --output smoke.json
```

Approved remote run with read-only server monitoring:

```bash
rws-bench run \
  --config tools/rws-bench/example.toml \
  --monitor-ssh root@94.237.44.223 \
  --monitor-process rsw-comfy \
  --monitor-port 80 \
  --output rws-adaptive-on-run1.json
```

Generate the required Markdown report from three repetitions per variant:

```bash
rws-bench report \
  rws-on-1.json rws-on-2.json rws-on-3.json \
  rws-off-1.json rws-off-2.json rws-off-3.json \
  nginx-1.json nginx-2.json nginx-3.json \
  --output RWS_BENCHMARK_REPORT.md
```

Use an identical `label` on result files produced simultaneously by separate load generators,
then add `--merge-load-generators` to sum throughput/connections and merge histogram buckets. Do
not use that switch for repetitions; normal mode reports their median.

Mixed-idle WebSocket echo workload (use the same URL path, payload, rate and Host header for all
variants):

```bash
rws-bench websocket \
  --url ws://94.237.44.223/_rsw/ws \
  --host bench.local \
  --label rws-adaptive-on \
  --connections 100 \
  --active-percent 1 \
  --active-duration 30 \
  --idle-duration 30 \
  --rps 100 \
  --payload-bytes 128 \
  --output rws-adaptive-on-run1.json
```

## Scenario mapping

- **Idle sockets:** set `active_percent = 0`, `rps = 0`, and `idle_duration_secs = 600`.
- **Mixed activity:** use 100,000 connections and labels for 0.1%, 1%, 5%, 10%, and 50% active.
- **Overload/recovery:** run an explicitly reviewed series of increasing/decreasing `rps` values;
  preserve every JSON file. Automated public-IP ramping is intentionally not hidden behind one
  command.
- **Adaptive scheduler:** use the `websocket` command against `rsw-comfy` and change only
  `[adaptive].enabled` between otherwise identical workloads. The current adaptive controller
  affects native WebSocket output, not static HTTP, so an HTTP comparison cannot establish
  scheduler benefit.
- **WebSockets:** the `websocket` command supports the same mixed-idle ratios, rate staggering,
  safety gates, SSH monitoring, raw JSON and report pipeline as HTTP. For Nginx, use an identical
  echo backend and proxy configuration.

## Fairness requirements

Use the same 77-byte `robots.txt`, headers, HTTP version, server hardware, logging policy and
keep-alive timeout. Verify response body and relevant headers first with
`scripts/verify-static-equivalence.sh`. Alternate server order, repeat at least three times and
retain raw JSON. Never infer 100,000 successful sockets from a 100,000-socket request.

Current transport scope is plain `http://`. Terminate TLS separately only if both products use the
same TLS topology; otherwise report HTTPS as unmeasured.

# Technical design

## Measurement boundary

`rws-bench` measures the load generator and target server as separate systems. A run is invalid for
capacity claims when the requested connection count was not reached, generator resources were
insufficient, response framing differed, or the offered rate could not be sustained.

The current RSW scheduler is a ready-only deficit-round-robin queue for outbound native WebSocket
messages. The adaptive controller changes its byte quantum from queue depth. It does not schedule
ordinary HTTP connections. Consequently:

- HTTP idle/mixed/overload tests compare the general server stacks.
- Scheduler-on/off claims require an identical WebSocket workload on `rsw-comfy`.
- `rsw-simple` cannot be described as “adaptive off”; it is a different build/feature set.

## Runtime architecture

- Tokio multi-thread runtime backed by epoll on Linux.
- One asynchronous task per connection, multiplexed across runtime workers; never one OS thread per
  connection.
- Raw TCP and HTTP/1.1 framing so connection lifetime, exact headers and source-IP binding remain
  under benchmark control.
- Bounded 1,024-item status channel and fixed-capacity per-connection read buffers, with a hard
  16 MiB response safety limit.
- Connections are opened in 10 ms batches capped by `connect_rate`.
- A barrier starts the measured phase only after every attempted connection is either ready or
  failed.
- Active requests are staggered across a deterministic per-connection period. Missed deadlines are
  not queued into an unbounded catch-up burst.
- HDR histograms use microseconds, three significant digits and a one-hour maximum. Sparse recorded
  buckets are retained in JSON so multiple generators can be merged without storing every sample.

## HTTP state machine

Each socket performs a warm-up request before becoming ready. Responses must use `Content-Length`
or chunked transfer encoding; close-delimited responses are rejected because persistent reuse would
be ambiguous. HTTP 2xx/3xx counts as success. A read/write timeout or malformed response records a
socket error and unexpected disconnect.

No automatic reconnect is currently attempted. `reconnects` therefore remains zero, preserving the
distinction between real connection survival and a client that silently replaced failed sockets.

## Monitoring

Optional local/SSH monitoring runs once per configured interval and records process CPU/RSS/PSS,
file descriptors, established sockets on the configured port, TCP memory pages, NIC bytes and TCP
established resets. Commands are read-only. SSH uses batch mode and the caller's agent/config; the
tool has no credential fields.

## Fair comparison procedure

1. Verify identical 77-byte body, `Content-Type`, `Content-Length` and security headers.
2. Set both keep-alive timeouts longer than connection ramp plus measured duration.
3. Disable access logging for both, or enable an equivalent bounded policy for both.
4. Run `check` on every generator and verify its NIC/CPU/RAM outside the target metrics.
5. Use the same source IPs, connection rate, request rate and ordering.
6. Repeat at least three times and alternate variant order.
7. Use median repetitions. Use `--merge-load-generators` only for files produced simultaneously.
8. Preserve raw JSON and report missing or unreliable measurements.

## Guardrails and known gaps

- More than 10,000 connections requires explicit `--allow-large-test`.
- No kernel setting is changed automatically.
- HTTPS is deliberately unsupported until a fair TLS topology is specified.
- Mixed-idle WebSocket echo runs are integrated. Multiple source-IP binding is not yet implemented
  for WebSockets, so scale beyond one source-port range requires multiple generators.
- Automated stepped overload/recovery phases are not yet implemented; preserve separately labelled
  increasing/decreasing runs until phase-aware raw results are available.
- Generator CPU/RAM/NIC time-series are not yet embedded; Linux preflight checks and achieved-rate
  warnings prevent silent capacity claims, but publishable runs should collect generator telemetry.

## Test-VPS preflight observed 27 October 2026

Read-only inspection of `94.237.44.223` found 1 vCPU, approximately 1.8 GiB RAM, shell fd soft
limit 1,024, RSW process fd limit 262,144, ephemeral ports 32768-60999, `somaxconn=4096`, and
`tcp_max_syn_backlog=128`. RSW listens on 80/443 and Nginx on 18081. Both return the same 77-byte
benchmark body and matching content type/length/security header. Nginx has no explicit benchmark
keep-alive timeout, so the current configuration is not valid for a ten-minute idle run.

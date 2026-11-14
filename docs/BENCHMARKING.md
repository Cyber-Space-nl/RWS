# Reproducible static HTTP benchmarks

Use this procedure on machines and addresses you own or are explicitly authorised to test. The
examples are small functional runs, not performance claims. Use a separate load generator for
comparisons; same-host results compete for CPU and are suitable only for smoke checks.

## Prepare equivalent targets

1. Build both `rws` and `rws-bench` with `cargo build --locked --release -p rws -p rws-bench`.
   Use the freshly built binary, not a historical binary in `dist`.
2. Deploy the identical `tools/rws-bench/fixtures/robots.txt` to each test server's document root.
   Record its SHA-256 with `sha256sum`; configure identical host routing, HTTP/1.1, keep-alive,
   logging, response headers, file and hardware. Document cache settings for each server. The
   existing `deploy/` configurations are historical examples with host-specific paths, not ready
   to install. Review and adapt them rather than running the historical firewall-changing script.
3. Before a cold-start run, check the targets with
   `./scripts/verify-static-equivalence.sh http://SERVER_A/robots.txt http://SERVER_B/robots.txt bench.local`.
   This makes requests and warms caches: reset the intended application-cache state afterwards.
   It checks the body and selected headers, not full server equivalence.
4. On each machine, record environment JSON in a **new** result directory. Include CPU model,
   architecture, RAM, OS/kernel, Rust version, Git revision and optional binary/config hashes.
   Include server software versions, CPU affinity, hardware/VM, network and cache-reset method in
   non-secret notes. Server config contents and file paths are not copied.

```bash
mkdir results-run-001  # choose a new directory; do not overwrite historical results
python3 tools/rws-bench/environment.py --role server --binary target/release/rws \
  --config config.example.toml --notes 'Ubuntu; server version; CPU affinity; cache method' \
  --output results-run-001/server.json
python3 tools/rws-bench/environment.py --role generator --binary target/release/rws-bench \
  --notes 'generator host and network details' --output results-run-001/generator.json
```

Run the first command on the server and the second on the generator; copy the resulting JSON into
the same results directory. When the server is not RWS, hash its actual binary and record its
version and relevant configuration. Never put credentials, private URLs or configuration secrets
in notes or result files.

## Cold start and warm runs

The tool's default sends one unmeasured warm-up request per connection. `--skip-warmup` omits
those requests so the first measured request can encounter a cold **application** cache. Restart
or reset the target's application cache before each cold-start repetition. For example, after a
reset, run a single connection and a single offered request:

```bash
target/release/rws-bench run --target http://SERVER/robots.txt --host bench.local \
  --label rws-cold-start --connections 1 --active-percent 100 --idle-duration 0 \
  --active-duration 1 --rps 1 --skip-warmup --output results-run-001/cold-1.json
```

This captures at most one first-request sample, not a reliable p95/p99 or a cold-cache throughput
measurement. Repeat from a reset state to study cold starts. A longer single-file `--skip-warmup`
run mixes cold and warm requests. Restarting RWS does not clear the OS page cache; document the
actual OS cache state. Do not drop global OS caches on shared or production systems.

For a sustained **cold application-cache** workload, prepare many byte-identical files at distinct
paths. This script refuses to overwrite an existing fixture directory or path list:

```bash
mkdir results-run-001/fixture-root
python3 tools/rws-bench/prepare-fixtures.py --fixture tools/rws-bench/fixtures/robots.txt \
  --directory results-run-001/fixture-root/cold --paths results-run-001/paths.txt \
  --manifest results-run-001/manifest.sha256 --prefix /cold --count 20000
sha256sum tools/rws-bench/fixtures/robots.txt results-run-001/paths.txt
```

Copy `fixture-root/cold` to the same `/cold` location below each server's document root and copy
the manifest to each server. From each document root run `sha256sum -c /path/to/manifest.sha256`
and check that every file passes. Keep `paths.txt` on the generator. Before each cold
repetition, reset the application cache; verifying files by requesting them warms that cache, so
perform verification first. Ensure every URL maps to a distinct file and no server rewrites or
deduplicates the cache keys. Then run:

```bash
target/release/rws-bench run --target http://SERVER/robots.txt --host bench.local \
  --label rws-cold-files --connections 100 --active-percent 100 --idle-duration 0 \
  --active-duration 10 --rps 1000 --skip-warmup --unique-paths results-run-001/paths.txt \
  --monitor-ssh USER@SERVER --monitor-process rws --monitor-port 80 \
  --output results-run-001/cold-files-1.json
```

Each listed path is issued at most once, including failed attempts; the JSON records paths
available and attempted. Provide more paths than the offered rate times duration. If the list is
exhausted, workers can stop early and throughput still uses the configured duration; the report
warns about this. Compare identical lists and settings across servers. Distinct URLs do not prove
the OS page cache is cold: copying files can itself warm it. Record whether that layer was warm,
cold or unknown. Per-request path construction can also make the generator a bottleneck.

For a comparable **warm distinct-file** run, first request all listed files on each target to
warm its caches, then repeat the exact same unique-path command with a different label and output
file. Keep cache capacities large enough for the fixture set and verify the effective cache state;
otherwise label the run accordingly. Do not compare this workload directly to a single-file run.

For a steady warm run, use the same workload and omit `--skip-warmup`:

```bash
target/release/rws-bench run --target http://SERVER/robots.txt --host bench.local \
  --label rws-warm --connections 100 --active-percent 100 --idle-duration 0 \
  --active-duration 15 --rps 1000 --monitor-ssh USER@SERVER --monitor-process rws \
  --monitor-port 80 --output results-run-001/warm-1.json
```

Repeat at least three times per variant, alternate order and keep the offered rate, file, host and
configuration consistent. Choose values within the capacity of the generator and target. SSH
monitoring is optional and read-only; without it CPU and RSS are unavailable. The monitor groups
processes by exact name and CPU is sampled over a short interval, so isolate the test and document
sampling limitations. RWS and another server must have equivalent instrumentation and workload.

## Report and interpret

```bash
target/release/rws-bench report results-run-001/cold-1.json results-run-001/warm-1.json \
  --environment results-run-001/server.json --environment results-run-001/generator.json \
  --output results-run-001/report.md
```

Keep each raw JSON and its environment metadata. The report includes throughput, p50/p95/p99,
response failure rate, connection failures and CPU/RSS when samples exist. Connection failures are
separate from the response failure-rate denominator. Small samples and zero baselines are not
evidence of capacity; absent monitoring is reported as unavailable. Request rate is offered, not
guaranteed. Only successful responses contribute to request latency; inspect failures and the
generator separately. The client currently accepts status 200–399, uses plain HTTP and has no TLS
benchmark mode. These measurements do not establish peak throughput, representativeness or
security, and the historic HTML reports remain unchanged.

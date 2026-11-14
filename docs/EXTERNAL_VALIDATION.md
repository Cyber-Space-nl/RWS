# Independent validation checklist — RWS beta

This checklist targets the unified `rws` build. For older Simple/Comfy releases, use the checklist
from the corresponding release tag.

This is a procedure for an independent developer; unchecked items are **not evidence that a test
passed**. Record `PASS`, `FAIL` or `NOT RUN`, the date, commands, results and evidence for each
item. Report failures and limitations, not just successes. This checklist does not authorise
publication or establish production readiness.

`[Command]` means a command or existing script performs the check, but the operator must run it
and inspect its result. `[Manual]` requires operator setup, comparison or judgement. `[CI]` runs
in the repository's GitHub Actions workflow. A passing CI run does not replace this checklist.

## 0. Test environment and evidence

- [ ] **[Manual]** Use a fresh, disposable Ubuntu 24.04 x86-64 VM with systemd and Linux 5.6+.
  These steps install packages, create `/etc/rws` and `/var/www/example.test`, change services
  and exercise load. Do not run them on a production or shared machine. Use a separate authorised
  load generator for comparative benchmarks. Verify ports 8080, 8443 and 9091 are free.
- [ ] **[Manual]** Obtain the approved source revision and, if supplied, the candidate archive and
  checksum from the maintainer or the relevant public GitHub release. Record the source revision, artifact checksum, OS,
  kernel, CPU, RAM, Rust version, server configuration and generator location. Do not include
  credentials or private keys in evidence. Record the artifact's `BUILD_INFO.json` revision
  separately from the checkout used for this checklist: a later documentation-only commit can
  contain the checklist without being the revision used to build the candidate binaries.
- [ ] **[Command]** Start in the source checkout and create a new evidence directory:

  ```bash
  git rev-parse HEAD
  git status --short
  uname -a
  rustc --version
  EVIDENCE=$(mktemp -d)
  printf 'Evidence directory: %s\n' "$EVIDENCE"
  ```

  Record whether the checkout is clean. Keep `EVIDENCE` set in the same shell for later steps.

## 1. Artifact and clean installation

- [ ] **[Command]** If given the standalone archive, obtain its expected checksum through a trusted
  channel, compare it with the supplied `SHA256SUMS`, and run `sha256sum -c SHA256SUMS` in the
  artifact directory. Extract into a new directory, run `sha256sum -c BINARY_SHA256SUMS` inside it,
  then run each binary's `--version` explicitly. All three must report the version of the selected
  release. SHA-256
  detects changes but is not a signature: an attacker who can replace both files can replace both
  hashes. The `v0.1.0-beta.1` package built on Ubuntu 24.04 requires GLIBC symbols up to 2.39;
  check the specific artifact's requirements and do not assume support for older runtimes. In the
  extracted directory:

  ```bash
  sha256sum -c BINARY_SHA256SUMS
  for name in rws rws-ws-load rws-bench; do "./bin/$name" --version; done
  cat BUILD_INFO.json
  ```

  Return to the source checkout before continuing.
- [ ] **[Command + Manual]** In the source checkout, install the prerequisites and follow
  [Ubuntu installation](../UBUNTU_INSTALL.md) for RWS. Before starting the service, edit the
  test site host in `/etc/rws/config.toml` to `localhost`; keep its root at
  `/var/www/example.test/public` and PHP enabled for the PHP test below. Validate with
  `sudo /usr/local/bin/rws --config /etc/rws/config.toml --check`, start the service,
  and inspect `sudo systemctl status rws` and `sudo journalctl -u rws -b`. Expect an
  active service without startup errors. Install PHP-FPM as described in the installation guide;
  confirm its Unix socket matches the configuration. The archive does not contain a systemd
  installer; the installers run from a source checkout with its own `target/release/` binaries.

## 2. Build and automated checks

- [ ] **[Command]** From the candidate source checkout, run:

  ```bash
  cargo fmt --all -- --check
  cargo clippy --locked --workspace --all-targets -- -D warnings
  cargo test --locked --workspace
  cargo build --locked --release --workspace
  cargo audit
  cargo +1.88.0 check --locked --workspace
  ```

  Install `rustfmt`, `clippy`, `cargo-audit` and the 1.88.0 toolchain if needed; see
  [CONTRIBUTING.md](../CONTRIBUTING.md). Each command must exit successfully. Record warnings and
  tool versions; a missing tool or unavailable advisory database means `NOT RUN`, not a pass.
- [ ] **[Command]** After stopping the systemd service or changing ports to avoid conflicts, run
  `./scripts/build-linux.sh` followed by `./scripts/smoke-test.sh` from the checkout root.
  The smoke script uses loopback ports 18080/19091 and fixed files in `/tmp`; use only the isolated
  VM. Expect HTTP 200, unknown host 421, health `ok`, 20 WebSocket connections, 100 echoed
  messages, zero connection errors, zero remaining WebSockets and clean shutdown. It does not test
  TLS or PHP-FPM. Restart the installed service for the remaining checks.

## 3. HTTP and HTTPS

- [ ] **[Command]** With the installed test service active, check host routing and health:

  ```bash
  curl --fail --noproxy '*' -H 'Host: localhost' http://127.0.0.1:8080/
  curl -sS --noproxy '*' -o /dev/null -w '%{http_code}\n' -H 'Host: invalid.test' http://127.0.0.1:8080/
  curl --fail --noproxy '*' http://127.0.0.1:9091/healthz
  ```

  Expect the example page, `421`, and `ok` respectively. Record the response and verify no
  unexpected service errors. The metrics listener is intended to remain accessible only locally.
- [ ] **[Manual + Command]** In the disposable VM only, generate a temporary self-signed test
  certificate, configure `[tls]` with its certificate and private key paths, then restart the
  service. For example:

  ```bash
  sudo apt install -y openssl
  sudo openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
    -subj '/CN=localhost' -addext 'subjectAltName=DNS:localhost' \
    -keyout /etc/rws/tls/test-key.pem -out /etc/rws/tls/test-cert.pem
  sudo chown root:rws /etc/rws/tls/test-key.pem
  sudo chmod 0640 /etc/rws/tls/test-key.pem
  sudo chmod 0644 /etc/rws/tls/test-cert.pem
  ```

  Replace the existing commented TLS example with the following section, validate the config and
  restart:

  ```toml
  [tls]
  cert_file = "/etc/rws/tls/test-cert.pem"
  key_file = "/etc/rws/tls/test-key.pem"
  ```

  Then run:

  ```bash
  curl --fail --noproxy '*' --cacert /etc/rws/tls/test-cert.pem \
    --resolve localhost:8443:127.0.0.1 https://localhost:8443/
  ```

  Expect the example page with certificate verification enabled. A self-signed test certificate
  is not a public certificate; this does not test ACME, renewal or external trust.

## 4. PHP and WebSockets

- [ ] **[Manual + Command]** Confirm PHP-FPM is running and the configured socket is accessible to
  the `rws` service user. Install only a harmless temporary test script:

  ```bash
  printf '%s\n' '<?php echo "RWS PHP validation";' | sudo tee /var/www/example.test/public/validation.php >/dev/null
  curl --fail --noproxy '*' -H 'Host: localhost' http://127.0.0.1:8080/validation.php
  ```

  Expect `RWS PHP validation`, not PHP source or an error. Inspect both RWS and PHP-FPM logs. Delete
  the test file afterwards. This test does not establish isolation or safety of untrusted scripts.
- [ ] **[Command]** Test the native WebSocket endpoint against the installed server:

  ```bash
  target/release/rws-ws-load --url ws://localhost:8080/_rws/ws \
    --connections 20 --messages 5 --payload-bytes 128
  curl -sS http://127.0.0.1:9091/metrics | grep '^rws_open_websockets '
  ```

  Expect 100 echoed messages, zero connection errors and eventually zero open WebSockets. The
  tool uses the URL hostname as the Host header, hence `localhost` must match the test site.

## 5. Load, shutdown and benchmarks

- [ ] **[Manual + Command]** Run a small, bounded same-host functional load against this isolated
  VM, saving raw JSON. This is a smoke check, not a publishable performance comparison:

  ```bash
  target/release/rws-bench run --target http://127.0.0.1:8080/ --host localhost \
    --label local-smoke --connections 5 --active-percent 100 --idle-duration 0 \
    --active-duration 3 --rps 10 --monitor-process rws --monitor-port 8080 \
    --output "$EVIDENCE/load.json"
  target/release/rws-bench report "$EVIDENCE/load.json" --output "$EVIDENCE/load.md"
  ```

  Review opened versus requested connections, response/socket errors and warnings. A successful
  command alone is not proof that every request succeeded. CPU/RSS may be unavailable due to
  permissions or missing tools; record missing samples as `NOT RUN`. Do not present this as a
  maximum-throughput result.
- [ ] **[Manual + Command]** Start a bounded WebSocket workload with an idle period, confirm
  `rws_open_websockets` is greater than zero, then stop the service while clients are connected:

  ```bash
  target/release/rws-ws-load --url ws://localhost:8080/_rws/ws \
    --connections 20 --messages 5 --idle-seconds 20 >"$EVIDENCE/ws-shutdown.log" 2>&1 &
  LOAD_PID=$!
  ```

  Poll the metrics endpoint until active connections are observed, then run
  `sudo systemctl stop rws` and confirm it stops within the configured grace period plus a
  small scheduling margin. Wait for the load process and record its exit status and log: clients
  may report disconnects because shutdown was intentional. Inspect the service journal for a
  clean shutdown and restart the service. This bounded test is not sustained overload validation.
- [ ] **[Manual + Command]** For a comparative benchmark use a separate generator, identical
  fixtures/configuration, cold and warm procedures, environment JSON and raw results as described
  in [BENCHMARKING.md](BENCHMARKING.md). Generate a Markdown report, record CPU/RSS and failure
  rates, and review every warning. Record OS-cache state and generator limitations. If comparison
  hosts or monitoring are unavailable, mark those checks `NOT RUN` rather than inventing values.

## 6. Upgrade and rollback

- [ ] **[Manual + Command]** On the disposable VM, choose a known previous build and candidate;
  record revisions, version output and hashes. If no suitable earlier build exists, mark the
  cross-version upgrade `NOT RUN`. A reinstall of the same build tests the installer mechanics only.
- [ ] **[Manual]** Before upgrading, make separate verified backups of the installed binary, unit,
  config, logrotate file and site data. The installer backs up an existing binary, unit and logrotate file, preserves
  config, and leaves existing legacy services untouched. Do not rely solely on its automatic
  backups. In the disposable VM, for example:

  ```bash
  sudo cp -a /usr/local/bin/rws "$EVIDENCE/before-binary"
  sudo cp -a /etc/systemd/system/rws.service "$EVIDENCE/before-unit"
  sudo cp -a /etc/rws/config.toml "$EVIDENCE/before-config"
  sudo cp -a /etc/logrotate.d/rws "$EVIDENCE/before-logrotate"
  sudo cp -a /var/www/example.test "$EVIDENCE/before-site"
  sudo sha256sum "$EVIDENCE/before-binary" "$EVIDENCE/before-unit" \
    "$EVIDENCE/before-config" "$EVIDENCE/before-logrotate" >"$EVIDENCE/before-sha256.txt"
  ```

  Verify the backups exist and record hashes outside the site root. Preserve TLS keys separately
  when they are needed for recovery; do not include them in a shared report.
- [ ] **[Manual + Command]** Stop the service, install the candidate from its source checkout with
  `scripts/install.sh`, confirm the existing config is preserved, run `--check`,
  start it, and repeat HTTP, health, PHP and WebSocket checks. Record hashes and service logs.
- [ ] **[Manual + Command]** Stop the service and restore the matched backups made above:

  ```bash
  sudo systemctl stop rws
  sudo cp -a "$EVIDENCE/before-binary" /usr/local/bin/rws
  sudo cp -a "$EVIDENCE/before-unit" /etc/systemd/system/rws.service
  sudo cp -a "$EVIDENCE/before-config" /etc/rws/config.toml
  sudo cp -a "$EVIDENCE/before-logrotate" /etc/logrotate.d/rws
  sudo systemctl daemon-reload
  sudo systemctl start rws
  sudo sha256sum /usr/local/bin/rws /etc/systemd/system/rws.service \
    /etc/rws/config.toml /etc/logrotate.d/rws
  ```

  Compare the output with `before-sha256.txt`; the paths differ but each corresponding hash should
  match. Restore site data separately if the upgrade modified it. Repeat HTTP, health and
  service-log checks. Follow the detailed
  [rollback guidance](../README.md#upgrade-and-rollback). Do not delete legacy artifacts during
  validation. Record whether this was a real cross-version rollback or a same-build rehearsal.

## 7. Final report

- [ ] **[Manual]** Supply the checklist with individual outcomes and sanitized evidence, including
  exact source revision and artifact hash. Identify which tests were run on which host and which
  were skipped. Flag discrepancies, security concerns, missing monitoring, PHP-FPM or TLS issues,
  and any differing platform or libc version.
- [ ] **[Manual]** Review the packaged `DEPENDENCIES.md` and collected license/notice files for the
  intended distribution. A package inventory is not a legal opinion. Publication, tagging and
  release uploads require separate maintainer approval.

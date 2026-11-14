# Security testing and limitations

RWS is experimental. These checks reduce specific risks; they are not a security certification.

## Automated checks

Run `cargo test --locked --workspace` on Linux. Tests cover encoded path traversal, a symlink
escaping the site root, replacement of a file with a symlink after it was opened, malformed HTTP/1
headers on a socket, an HTTP/1 header deadline, oversized and slow proxy bodies, filtering of
client forwarding headers and connection-nominated headers, FastCGI wire limits, static cache limits and changes,
and scheduler queue limits and slow-writer isolation. A Linux integration test generates a temporary
certificate and checks `rws --check` and certificate-verified HTTPS startup; it requires
`openssl` and `curl`. Linux-specific tests are not run on Windows.

`cargo audit` checks the lockfile against RustSec advisories. Its findings can change independently
of RWS. CI also runs formatting, Clippy and release builds.

## Local checks performed

A local Linux test used a temporary self-signed certificate trusted only by the test client: a
normal HTTPS request succeeded and incomplete HTTP/1 headers were closed after the configured
deadline on HTTP and HTTPS. SIGTERM completed with 20 incomplete connections and in another run
while 55 WebSockets were open during a 100-client workload. The latter intentionally interrupted
clients; it did not verify delivery of all queued messages. The standard smoke test exercises HTTP
host routing, health, 20 WebSocket clients and shutdown. These are small functional tests, not proof
of resilience under sustained load.

The supplied systemd units omit `RestrictSUIDSGID`: on Ubuntu 24.04 this setting blocks
`openat2`, causing file requests to fail. They retain `NoNewPrivileges`, which prevents gaining
privileges through execution but does not prevent creation of set-ID files. Keep the service
user unprivileged and limit its write access.

## Trust boundaries and gaps

- Linux static reads use `openat2` beneath the configured root and retain the opened descriptor;
  this requires Linux 5.6 or newer and fails closed when unavailable. Keep the root and its parent
  directories trusted. This does not prevent exposure through malicious hard links or files placed
  inside a root by a writer. Non-Linux static opens do not have the same containment guarantee.
- PHP-FPM opens script paths independently, so RWS cannot guarantee race-free PHP path access.
  Restrict write access and isolate the PHP-FPM service. `max_fastcgi_response_bytes` limits
  FastCGI wire bytes per request, including record framing, headers, stderr and response body.
  PHP-FPM memory behaviour needs further end-to-end testing.
- Directory listings are disabled by default. On Linux an enabled listing enumerates an opened,
  contained directory, excludes dotfiles and symlinks, and limits examined entries, result count
  and name bytes.
  It intentionally reveals other visible names; enable it only for public directories. It depends
  on `/proc/self/fd` and is unavailable on non-Linux platforms. Static gzip is opt-in and its
  worker concurrency and input size are bounded; it does not compress PHP or proxy responses.
- Proxy limits apply per request, not to aggregate process memory. The proxy strips untrusted
  forwarding headers and does not construct replacement client-identity headers. Deployments that
  need trusted proxy chains need explicit support before relying on such headers.
- `--check` parses TLS certificate and key files; a successful check does not validate certificate
  trust, expiration, hostname coverage, renewal or the external TLS deployment.
- The configured HTTP/1 header deadline is not a general timeout for TLS handshakes, HTTP/2,
  WebSocket sessions, static response streaming or all idle connections. TLS uses the underlying
  server's handshake timeout. Connection count limits, slow readers, load-related graceful
  shutdown, certificate renewal and invalid certificate contents require further verification.
- Fuzzing was not run. The added socket and boundary regressions cover concrete behaviours, but
  they do not replace parser fuzzing or independent security review.

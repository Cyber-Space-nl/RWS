# Changelog

Notable changes are recorded here. Dates are added when releases are published.

## Unreleased

## 0.1.0-beta.9 — 2026-11-17

### Fixed

- Share pending Basic Auth checks between concurrent requests with identical credentials,
  including the first page load and requests arriving after a cached result expires.
- Reuse successful Basic Auth checks for 60 seconds in a bounded credential cache, so protected
  assets can load without repeating bcrypt for every request.
- Let short bursts of protected requests wait for a Basic Auth check in a bounded queue. This
  avoids immediate 503 responses when a browser loads several protected assets in parallel.

## 0.1.0-beta.8 — 2026-11-16

### Added

- Include the request Referer header in site access logs and HTTP error logs.

### Fixed

- Combine split Cookie fields with `; ` before FastCGI conversion so HTTP/2 requests retain
  all cookies, including PHP session identifiers, in their original order.

## 0.1.0-beta.7 — 2026-11-16

### Added

- Static/PHP applications mounted under URL prefixes, with independent roots, PHP-FPM pools
  and cache namespaces, optional bcrypt htpasswd authentication, path/file deny rules and noindex.
- Correct mounted PHP script URLs and listener-derived HTTPS metadata.
- Automatic HTTP-to-HTTPS 308 redirects when TLS is configured, with a configurable opt-out
  and a static-only ACME HTTP-01 exception for certificate renewal.

## 0.1.0-beta.6 — 2026-11-15

### Changed

- PHP-FPM systemd startup ordering is now configured explicitly per local service through a
  non-clobbering helper instead of hard-coding PHP 8.3 in the RWS unit.

## 0.1.0-beta.5 — 2026-11-14

### Added

- Weak, metadata-based ETags for streamed static files on Linux, with conditional GET and HEAD.

### Changed

- Linux x86-64 release builds target an Ubuntu 22.04 glibc baseline and reject binaries that
  require newer glibc symbols.

## 0.1.0-beta.4 — 2026-11-14

### Added

- Bounded multipart byte ranges and SHA-256 strong ETags for buffered identity and gzip
  representations, including conditional requests and matching ETag- or date-based `If-Range`.

## 0.1.0-beta.3 — 2026-11-14

### Changed

- The server is built as a single `rws` binary and service. HTTP proxying and adaptive scheduling
  are available as optional configuration.

## 0.1.0-beta.2 — 2026-11-14

### Added

- Static GET single-byte-range responses and date-based conditional requests.
- Opt-in bounded gzip for eligible static files and opt-in, contained Linux directory listings.

## 0.1.0-beta.1 — 2026-11-13

### Added

- GitHub Actions checks for formatting, Clippy, workspace tests, release builds, dependency audit
  and the minimum Rust version.
- Linux contained static-file opening and regression tests for traversal, symlinks and request limits.
- HTTP/1 header deadlines, bounded proxy and FastCGI response buffering, and TLS file validation
  during `--check`.
- Reproducible benchmark fixtures, unique-path workloads, environment metadata and reports.
- Contributor, security, architecture and benchmarking documentation.

### Fixed

- The systemd units no longer block the `openat2` calls used to serve files on Ubuntu 24.04.

### Changed

- Project components, binaries, services, configuration paths, metrics and WebSocket endpoint use
  the `rws` prefix. This breaks compatibility with pre-rename installations; migration is manual.
- Client-provided forwarding headers are removed by the HTTP proxy. PHP and proxy request and
  response limits apply per request and may require configuration changes for large workloads.
- The workspace minimum Rust version is 1.88 for the current lockfile.
- The server explicitly selects its Rustls provider when none is installed, avoiding a TLS
  startup panic when both AWS-LC and ring features are enabled.

Historical benchmark reports retain their original measurements and methods.

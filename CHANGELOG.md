# Changelog

Notable changes are recorded here. Dates are added when releases are published.

## Unreleased

### Added

- Weak, metadata-based ETags for streamed static files on Linux, with conditional GET and HEAD.

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

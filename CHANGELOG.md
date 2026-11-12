# Changelog

Notable changes are recorded here. Dates are added when releases are published.

## 0.1.0-beta.1 — prepared, not published

### Added

- GitHub Actions checks for formatting, Clippy, workspace tests, release builds, dependency audit
  and the minimum Rust version.
- Linux contained static-file opening and regression tests for traversal, symlinks and request limits.
- HTTP/1 header deadlines, bounded proxy and FastCGI response buffering, and TLS file validation
  during `--check`.
- Reproducible benchmark fixtures, unique-path workloads, environment metadata and reports.
- Contributor, security, architecture and benchmarking documentation.

### Changed

- Project components, binaries, services, configuration paths, metrics and WebSocket endpoint use
  the `rws` prefix. This breaks compatibility with pre-rename installations; migration is manual.
- Client-provided forwarding headers are removed by the Comfy proxy. PHP and proxy request and
  response limits apply per request and may require configuration changes for large workloads.
- The workspace minimum Rust version is 1.88 for the current lockfile.

Historical benchmark reports retain their original measurements and methods.

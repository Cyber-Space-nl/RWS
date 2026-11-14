# Ubuntu 22.04 clean beta.5 installation

On 14 November 2026, a fresh Ubuntu 22.04.5 x86-64 QEMU VM was used to test the published
beta.5 binary with real systemd and PHP 8.1 FPM. The Ubuntu cloud image's signature and checksum
were verified before use. The VM used software emulation; no production server was changed.
This is a local test, not an independent security or distribution review.

## References

- Release: [v0.1.0-beta.5](https://github.com/Cyber-Space-nl/RWS/releases/tag/v0.1.0-beta.5).
- Release source revision: `1ecc29ab9f7f1a3ec5bb76d102e57f5e6f0de19a`.
- Published x86-64 archive SHA-256: `a7cacb53a84df5f8391c0e7fbc0825fec96ece594bfef865fcc4b2277f9d6402`.
- Tested `rws` binary SHA-256: `7e0a38318bda61e61d22a1a37904bc70e211f3f32232913d3de1283352810dd9`.
- Ubuntu cloud image SHA-256: `77d19e53656f87c95f1f6c6be59c789083b3f77fa08f9d88011ee04085c61f87`.
- Guest: Ubuntu 22.04.5, kernel `5.15.0-203-generic`, glibc 2.35.

## Procedure and results

The published archive's checksums were verified. The matching release source tree supplied the
installer and unit, with the published binary placed at the build path expected by the installer.
The installer was run on a clean VM. Its installed binary, initial configuration and systemd unit
matched their source files, and the installer left the service stopped as designed.

Ubuntu 22.04 provides PHP 8.1 rather than the PHP 8.3 socket in the example configuration. The
installed configuration's socket was changed to `/run/php/php8.1-fpm.sock`; the shipped systemd
unit was left unchanged. With PHP 8.1 FPM already active, the following checks passed:

- Configuration validation and enabling and starting the systemd service as user `rws`.
- Static HTTP, a PHP script through the real Unix socket, and the health endpoint.
- WebSocket echo: four connections, eight messages echoed, no connection errors.
- A systemd service restart followed by static HTTP, PHP and health checks.

## Limits and follow-up

The shipped unit specifies ordering after `php8.3-fpm.service`, not `php8.1-fpm.service`. PHP 8.1
was already running in this test, so ordering during a full system boot was not established. Users
should adjust their systemd ordering for the PHP-FPM version they use and verify it on their system.
This test did not cover an upgrade, rollback, full VM reboot, HTTPS, production workloads, PHP
application isolation, or native ARM64. Preserve backups and test your own deployment procedure.

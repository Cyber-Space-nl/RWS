# Ubuntu 24.04 unified-service validation

On 14 November 2026, a disposable Ubuntu 24.04.5 x86-64 QEMU VM with kernel 6.8.0-147 was used
to check the unified RWS service. The official cloud image signature and checksum were verified.
The VM used software emulation and real systemd and PHP 8.3 FPM; no production server was changed.
This is a local test, not independent security or distribution review.

The exercise used the published beta.4 and beta.5 server binaries and their corresponding source
trees for the installer and systemd unit. The default configuration was adjusted to listen on
loopback and serve `localhost`. It covered:

1. Installing beta.4 on the clean VM, validating its configuration and starting its systemd unit.
   Static HTTP, PHP-FPM over its Unix socket and the health endpoint returned expected responses.
2. Stopping the service, installing beta.5 over it and confirming that the existing configuration
   was preserved and binary/unit backups were created. Static HTTP, PHP and health checks passed
   after restart. The beta.5 WebSocket tool echoed eight messages over four connections with no
   connection errors.
3. Stopping the service and manually restoring the saved beta.4 binary, unit, configuration and
   logrotate file. The restored files matched the saved copies; configuration, systemd service,
   static HTTP, PHP and health checks passed after restart.

The beta.5 server binary came from the published x86-64 archive with SHA-256
`a7cacb53a84df5f8391c0e7fbc0825fec96ece594bfef865fcc4b2277f9d6402`. The tested source revision
was `1ecc29ab9f7f1a3ec5bb76d102e57f5e6f0de19a`.

This does not validate a fresh beta.5 install, automated rollback, other distributions or
architectures, HTTPS, PHP application isolation, sustained production load or security against
untrusted writers. Ubuntu 22.04 was separately checked in containers and in a later
[clean-install VM test](ubuntu-22.04-beta5.md). Preserve backups and test your own upgrade and
rollback procedure before use.

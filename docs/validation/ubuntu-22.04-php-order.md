# Ubuntu 22.04 PHP-FPM startup-order check

On 15 November 2026, a disposable Ubuntu 22.04.5 x86-64 QEMU VM with kernel 5.15.0-203 and real
systemd and PHP 8.1 FPM was used to check the new local PHP-FPM ordering helper. The Ubuntu cloud
image signature and checksum were verified. This was a local test using software emulation, not
an independent security review or production deployment.

The test used the published beta.5 server binary and installer/unit/helper changes from the
working tree based on commit `478682f`. The changed unit and helper had SHA-256 values
`6a3a1165ffc27907e9db6f9a8ab155a299f84e0641753733847bcd7d190c3823` and
`71d316906688975b7a8ec046dd75e06d935b448361e7536d858c193ded0fea41`, respectively.

The test first disabled PHP-FPM and confirmed that RWS could start and serve a static file with
PHP disabled. It then configured `php8.1-fpm.service` and the matching PHP socket. It checked
that the helper rejects missing and malformed service names, supports multiple distinct service
names, removes duplicates, is idempotent, refuses to overwrite a conflicting drop-in, preserves
another existing drop-in, and does not start PHP itself. Starting RWS then started the disabled
PHP-FPM service and a PHP request succeeded.

After enabling RWS and stopping both services, the VM was fully rebooted. The boot ID changed;
PHP-FPM remained disabled as an independent boot service but became active through the RWS
dependency. Systemd's recorded active timestamps placed PHP-FPM before RWS. Static HTTP, PHP
and the health endpoint worked. Re-running the installer preserved the generated and existing
drop-ins, and PHP continued to work after restarting RWS.

This covers one local PHP 8.1 systemd service on Ubuntu 22.04. It does not validate every PHP-FPM
version, remote FastCGI, all PHP failures, PHP application isolation, production load or a native
ARM64 boot. `Wants=` does not make PHP-FPM a hard dependency: RWS can start even if PHP-FPM
fails, in which case PHP requests may fail. Verify the chosen service, socket and boot behaviour
on the target system. The beta.5 release itself predates this helper and unit change.

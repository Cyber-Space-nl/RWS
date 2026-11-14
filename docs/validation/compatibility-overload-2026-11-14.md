# Compatibility and overload checks — 14 November 2026

These local checks cover specific scenarios. They are not independent security research, a claim
of production readiness, a maximum-throughput benchmark, or evidence that all workloads are
free of memory leaks. The tests did not use a production server.

## Linux x86-64 compatibility

The published beta.4 x86-64 server binary failed to start in an Ubuntu 22.04 container because
it required GLIBC 2.38; Ubuntu 22.04 provides GLIBC 2.35. A separate source build on Ubuntu 22.04
started and passed basic static HTTP and proxy checks.

The [beta.5 archive](../releases/v0.1.0-beta.5.md) was subsequently built on Ubuntu 22.04
with Rust 1.88. Its three executables were checked against a GLIBC 2.35 symbol ceiling. In Ubuntu
22.04 and 24.04 containers, the actual archive passed checksum, version, configuration, static
HTTP, streamed weak ETag, multipart range, gzip ETag, HTTP proxy and WebSocket smoke checks.
These containers used a WSL2 Linux 6.6 kernel; they do not test a separate Ubuntu 22.04 kernel,
systemd or PHP-FPM on Ubuntu 22.04. A later [Ubuntu 22.04 VM test](ubuntu-22.04-beta5.md) covered
a clean beta.5 installation with PHP 8.1. See the [Ubuntu 24.04 VM test](ubuntu-24.04-beta5.md)
for the systemd/PHP-FPM upgrade and manual rollback exercise.

## ARM64 under emulation

An ARM64 build of commit `e3dec2f542e2dad4e0d91898c388da1cd18c8ff7` compiled with Rust 1.88 and a
Debian cross-compiler. In an Ubuntu 24.04 ARM64 container under QEMU user-mode emulation 8.2.2,
the version and configuration checks and a proxy HTTP smoke test passed. A static-file request
returned 403. QEMU's trace reported unknown syscall 437 immediately after opening the site root;
that syscall is `openat2`, which RWS uses for contained static-file opening. This test cannot
establish whether static files work on native ARM64 hardware. No native ARM64 hardware was tested
in this exercise.

## Proxy overload and recovery

The five-minute run used a release build of commit `e3dec2f542e2dad4e0d91898c388da1cd18c8ff7`
in an Ubuntu 24.04 container, limited to one CPU and 256 MiB. A separate Python container was
limited to half a CPU and 128 MiB. The containers shared an isolated network namespace. The
client used 20 concurrent workers and paused 100 ms after each request; a simulated upstream
responded after 250 ms. RWS was deliberately configured to allow only four concurrent proxy
requests, so excess requests were expected to receive HTTP 503. A few short compatibility checks
ran on the same host during the measurement.

| Observation | Five-minute run |
|---|---:|
| HTTP 200 responses during load | 4,565 |
| Expected HTTP 503 responses during load | 43,378 |
| Recorded client errors | 0 |
| Maximum concurrent upstream requests | 4 |
| Recovery requests after load | 5 of 5 returned HTTP 200 |
| Sampled RWS RSS | 6,720–7,104 KiB |

RSS was sampled approximately every ten seconds (31 samples, including recovery); it is not a
peak measurement. The workload lasted 300 seconds, and the total including recovery was 302.57
seconds. These numbers describe one deliberately constrained scenario, not server capacity or
general behaviour under other workloads. The beta.5 archive was tested separately as described
above; this overload run used the earlier source revision identified above.

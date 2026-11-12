# Security policy

RWS is experimental and is not yet a drop-in replacement for a mature production web server.
Security fixes are intended for the current default branch; there is no published support schedule
or maintained release branch yet.

## Reporting a vulnerability

Please do not publish exploit details or credentials in an issue. If GitHub private vulnerability
reporting is enabled, use the repository's [private advisory form](https://github.com/Cyber-Space-nl/RWS/security/advisories/new).
If it is unavailable, open an issue requesting a private contact method **without disclosing the
vulnerability**, and wait for a maintainer to provide one. Include affected revision, impact,
reproduction steps and any relevant sanitized configuration in the private report. Do not test
systems without their owner's permission. No response or remediation deadline is promised.

## Deployment boundaries

Keep site roots and their parent directories unwritable by untrusted users. Linux static reads use
contained file opening, but PHP-FPM opens scripts independently; isolate PHP-FPM and restrict
write access. Limit network access to the metrics listener, which defaults to loopback. Review
TLS, filesystem permissions and reverse-proxy trust before exposing a service. The proxy strips
client-provided forwarding headers and does not create trusted replacements. Configure per-request
body limits with awareness that they are not aggregate process memory limits.

See [security testing and limitations](docs/SECURITY_TESTING.md) for tested scenarios, platform
limitations and remaining gaps. `cargo audit` and CI cannot prove the absence of vulnerabilities.

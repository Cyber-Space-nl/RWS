# Contributing to RWS

Thank you for helping improve RWS. The project is experimental; correctness, security and
reproducibility take precedence over performance claims.

## Discuss and report

For bugs, include the RWS revision, operating system and kernel, Rust version, sanitized
configuration, steps to reproduce, expected and actual behaviour, and relevant logs. Remove
credentials, private paths and personal data. For security issues, use [SECURITY.md](SECURITY.md)
instead of a public issue. For significant changes, opening an issue first can help establish scope.

## Develop

Use Linux and Rust 1.88 or later. Clone the repository and run from its root:

```bash
git clone https://github.com/Cyber-Space-nl/RWS.git
cd RWS
rustup component add rustfmt clippy
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo build --locked --release --workspace
cargo install --locked --version 0.23.0 cargo-audit
cargo audit
```

The Linux-specific static-file containment tests run only on Linux. The [README](README.md)
contains a local non-root quickstart; [security testing](docs/SECURITY_TESTING.md) describes test
coverage and gaps. Add targeted regression tests when behaviour changes, and report any checks that
could not be run. Do not run installers, large load tests or deployment scripts on systems without
permission. For performance changes, follow [benchmarking](docs/BENCHMARKING.md), retain raw
measurements and state limitations; do not infer performance from a functional smoke test.

## Submit a change

Keep a pull request focused, explain the problem and the change, and list verification performed.
Update documentation and examples when behaviour or configuration changes. CI runs formatting,
Clippy, workspace tests, release build, a dependency audit and a minimum-Rust-version check on
Linux. It does not replace end-to-end PHP-FPM, TLS or sustained-load testing. Contributions are
provided under the repository's [MIT license](LICENSE).

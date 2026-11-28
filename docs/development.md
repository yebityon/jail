# Development and contributing

English · [日本語](development.ja.md) · [README](../README.md)

Bug reports, documentation improvements, and pull requests are welcome. Include the macOS, container, jail, and agent CLI versions, a minimal reproduction, and a redacted configuration. Never attach credentials, tokens, or unredacted process arguments.

From the repository root:

```sh
cargo fmt --all --check
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked --release --bin jail
./target/release/jail --dry-run claude --resume
```

Integration tests need Python 3. macOS 27 with Apple `container` 1.5.0 has been used for real-container verification.

Integration tests use a Python Apple-container fake and temporary loopback listeners. They do not contact the real container service or import host credentials.

CI runs these checks on macOS. It also runs Clippy and the tests of the library and the `jail-guest` supervisor on arm64 Linux, the only place where code under `cfg(target_os = "linux")` compiles. [rust-toolchain.toml](../rust-toolchain.toml) pins the Rust release that `container/Dockerfile` builds `jail-guest` with.

Fuzz targets for the parsers that read agent input are in [fuzz](../fuzz). They need nightly Rust and [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz), and do not run in CI:

```sh
cargo +nightly fuzz run gateway_request   # or proxy_request, base64
```

Optional real-container checks, after building the release binary:

```sh
./scripts/smoke-test.sh
./scripts/auth-smoke-test.sh
# Native CLI HTTP/SSE against a local fake only (macOS sandbox-exec required):
python3 scripts/protocol-smoke-test.py
```

The container scripts create and clean up dedicated temporary containers and home volumes. The smoke test checks restrictions, protected-path replacement, denial logging/live reload, forwarding and Codex sandbox behavior without host auth. The auth test imports host subscription login into root-only tmpfs and checks CLI recognition, credential visibility and a read-only model-catalog request. Neither performs inference or changes existing project containers. The protocol test uses only synthetic credentials and a local fake SSE server, with external network access blocked by macOS sandbox.

The host entry point is [src/host/main.rs](../src/host/main.rs); runtime management is in [src/host/runtime.rs](../src/host/runtime.rs). Every call to Apple's `container` command goes through [src/host/apple_container](../src/host/apple_container). The Linux supervisor is [src/container/main.rs](../src/container/main.rs), and code both sides use is in [src/shared](../src/shared).

`JAIL_HOME` overrides the host state/build directory, normally `~/.local/share/jail`. `JAIL_CONTAINER_BIN` selects a backend executable for testing. For contributions, add tests for changed behavior and keep the English and Japanese README and docs aligned. Planned work is tracked in [TODO.md](../TODO.md).

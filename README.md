# jail

Run Claude Code and Codex CLI in project-scoped [Apple containers](https://github.com/apple/container), from your terminal.

English · [日本語](README.ja.md)

> **Toy project / experimental prototype.** Built for personal experimentation and learning, not production use. It has not undergone an independent security audit and must not be relied on as a production security boundary. Use disposable projects and accounts; avoid sensitive data or production credentials. Behavior and CLI compatibility may change without notice.

This is an independent community project, not affiliated with or endorsed by Apple, Anthropic, or OpenAI.

```sh
cd /path/to/your/project
jail claude
jail codex
```

`jail` is a Rust CLI that manages a reusable Linux container for each project. It shares the project at the same path, keeps your working directory, and forwards your CLI arguments.

> Default settings allow writes to the shared project and unrestricted outbound networking. Review your configuration before running an agent on sensitive code or importing host credentials.

## Features

- **Project-scoped environments** — reusable containers with separate persistent home volumes.
- **Private host login reuse** — subscription OAuth stays in a root-only gateway; agents see placeholders. No API keys.
- **Write protection** — Git configuration/hooks and agent configuration are read-only; optionally allow writes only to listed paths.
- **Adjustable network policy** — open, allowlist or none; approve allowlist changes without a restart.
- **Local development** — TCP forwarding to your Mac's `127.0.0.1`.
- **Ephemeral sessions** — a throwaway container with no host mounts, deleted on exit.

## Installation

Requires an Apple silicon Mac with macOS 26 or later, Apple `container` 1.5.x, and Rust 1.85 or later. From the root of this checkout:

```sh
brew install container
cargo install --path . --bin jail --locked
jail config init   # writes ~/.config/jail/config.toml
```

The first launch builds a Linux image with the configured Claude Code and Codex versions, plus Git, Node.js, curl, and ripgrep.

## Usage

Put **jail options before the tool name**; arguments after `claude` or `codex` go to that CLI:

```sh
jail claude --resume
jail codex exec "Run the tests"
jail --auto-stop claude        # stop the container when the last session exits
jail --ephemeral claude        # throwaway container, nothing mounted
jail --dry-run claude          # print the launch plan only
```

| Command | Purpose |
| --- | --- |
| `jail claude` / `jail codex [args…]` | Launch the CLI in the project's container |
| `jail stop [--all]` | Stop containers with no active sessions; `--force` stops the current one anyway |
| `jail recreate` | Remove the current container; the next launch creates a new one |
| `jail auth claude` / `jail auth codex` | Re-import the host login |
| `jail policy show` / `log` / `allow HOST` / `reload` | Inspect and adjust the network allowlist |
| `jail config show` | Print the resolved configuration |
| `jail cleanup [volume NAME… \| image TAG… \| --all-delete]` | List or delete jail's home volumes and images |

Containers are reused between launches. When configuration or the image changes, close sessions and run `jail recreate`; the home volume, CLI history and host files are kept. Sign in with `claude auth login` or `codex login` **on the Mac**; running sessions pick up the login within about 15 seconds.

See [docs/usage.md](docs/usage.md) for the container lifecycle, cleanup, ephemeral sessions, development servers and troubleshooting.

## Configuration

Configuration is trusted input, read from `~/.config/jail/config.toml` (or `--config` / `JAIL_CONFIG`). Project config files are never loaded. All settings are in [config.example.toml](config.example.toml). For example:

```toml
[workspace]
allow_write = ["src/", "tests/"]   # everything else in the workspace is read-only
hidden = [".env", "secrets"]

[network]
mode = "allowlist"
allowed_hosts = ["api.anthropic.com", "claude.ai", "platform.claude.com",
                 "chatgpt.com", "auth.openai.com", "api.openai.com"]
```

See [docs/configuration.md](docs/configuration.md) for write protection, network policy and CLI tool restrictions.

## Security

jail isolates a Linux execution environment, but **does not make untrusted agents or projects safe by default**. Agents run as non-root with no capabilities. The host home, SSH agent and container socket are never shared, and real login tokens are hidden from agents. Shared files and authorized inference remain available within the configured permissions.

See [docs/security.md](docs/security.md) for authentication, isolation boundaries and limitations.

## Development

```sh
cargo fmt --all --check
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
```

Bug reports and pull requests are welcome; never attach credentials or tokens. See [docs/development.md](docs/development.md) for tests, fuzzing, smoke tests and the code layout.

## License

[MIT](LICENSE) © 2026 jail contributors. The license covers jail's own source code; third-party tools, including CLIs downloaded when building the image, keep their own licenses.

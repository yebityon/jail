# Security

English · [日本語](security.ja.md) · [README](../README.md)

## Authentication

**Subscription login only; no API keys.** jail reuses Claude / ChatGPT host login from files or macOS Keychain. Credentials travel over standard input into a root-only authentication gateway in guest tmpfs (`/run/jail-private`, mode `0700`; files `0600`). The agent receives synthetic OAuth placeholders, never real access/refresh/ID tokens. Existing host credentials are not overwritten.

- Host settings, skills, MCP configuration, and conversation history are not copied.
- Each project has a separate persistent guest home for settings, history and placeholder auth; real credentials do not persist there.
- Interactive sessions can start without host credentials; sign in on the Mac and the session receives the login. Import failures such as denied Keychain access or unsupported formats still report an error. Direct container CLI login/logout and API-key environment forwarding are rejected.
- During an authenticated session, jail checks every 15 seconds whether the host login changed. The check uses file metadata or Keychain attributes and does not read the password. When the official CLI on the Mac refreshes or replaces its login, jail reads it once and sends it to the running gateway. The gateway accepts it only if its access token expires later than the current one, so an older copy read by another session never replaces a newer login. One session per project and tool performs the check. Failures are reported after the session ends.
- jail does not refresh OAuth tokens or write host credentials. If the host CLI does not refresh its login, the gateway is not refreshed either. If Keychain access was allowed only once, macOS may ask again when the item changes; a denied read is retried after the next change.
- To force a re-import, close active sessions and run `jail auth claude` or `jail auth codex`.
- Set `auth.import_host = false` to disable automatic import and host-login sync; explicit `jail auth` is still available. Credentials vanish on stop and are imported again on the next authenticated launch.

The gateway injects OAuth only for fixed HTTPS subscription endpoints (`api.anthropic.com` and `chatgpt.com`), verifies TLS, pins resolved public IPs, and never follows redirects. It obeys network policy, including `none`. Claude uses its subscription-preserving base-URL setting; Codex uses an OAuth-backed custom provider with HTTP/SSE rather than WebSocket transport. Provider-specific Codex resume lists may differ from older sessions. Endpoint compatibility is limited to the pinned CLI versions; cloud, account-management and arbitrary gateway routes are not supported. The gateway does not refresh tokens itself. If login expires and the host login has not been refreshed, run `claude auth login` or `codex login` on the Mac, then retry the failed request.

**Sign in only on the Mac.** jail never runs a login itself and has no command for signing in from a session. Run `claude auth login` or `codex login` on the Mac; with `auth.import_host = true`, running sessions receive the new login within about 15 seconds. In an interactive terminal, a `/login` typed at the start of a line (including Tab completion from `/log`) is answered by jail: it is not passed to the agent CLI, and jail shows the command to run on the Mac. Arrow-key menu selections and pasted commands are not interpreted and reach the agent CLI. Do not complete a login started that way inside the container: the CLI there could store real tokens in the agent-readable home volume.

Known legacy guest auth files are replaced with placeholders on startup. Existing credential copies elsewhere are not discovered or deleted; a previously exposed token cannot be made secret retroactively. The gateway is **inside the VM**, not an external host-side credential service: it and the root supervisor remain trusted, and an agent can use authorized inference without learning the token.

References: [Codex authentication](https://learn.chatgpt.com/docs/auth), [Codex provider configuration](https://learn.chatgpt.com/docs/config-file/config-reference), [Claude subscription-preserving gateways](https://code.claude.com/docs/en/llm-gateway).

Custom `CLAUDE_CONFIG_DIR` Keychain names are not guessed; only file-based credentials are considered for that override. Account and organization policies may require reauthentication.

## Security and limitations

jail isolates a Linux execution environment, but **does not make untrusted agents or projects safe by default**. Shared files and authorized inference remain available within the configured permissions; real host login tokens are hidden from non-root agents.

| Boundary | Behavior |
| --- | --- |
| Workspace | Shares the entire Git root, or the launch directory outside Git, at the same host path; preserves the current subdirectory. `--ephemeral` shares nothing |
| Host access | Does not share the host home, SSH agent, or container-management socket; rejects launching from the home directory or its ancestors |
| Writes | `workspace.read_only` or `tools.edit = false` makes all shared host mounts read-only; `workspace.allow_write` limits workspace writes to listed paths; guest home and `/tmp` remain writable |
| Sensitive configuration | Default read-only bind mounts plus parent mount boundaries; custom `workspace.deny_write` |
| Credentials | Root-only tmpfs gateway injects subscription tokens for fixed upstreams; agents get placeholders |
| Hidden paths | Uses Apple's experimental path masking; startup does not retry with the restriction removed |
| Networking | `open` allows outbound traffic; `none` blocks agent IPv4/IPv6 traffic; `allowlist` restricts destinations through the proxy and firewall |
| Resources | Limits the container's CPU allocation and memory |
| Agent execution | Runs agents as non-root with dropped capabilities and `no_new_privs` |
| CLI policy | Applies Claude managed settings/tool denials and Codex configuration/managed requirements; rejects policy-override arguments when restrictions apply |

Native tool policies are defense in depth, **not syscall-level restrictions on arbitrary programs**. Enforcement depends on the configured CLI versions. If policy setup fails, jail does not launch the CLI.

The trusted initializer has `SYS_ADMIN` only to set Linux bind-mount boundaries; the supervisor drops that capability after setup. Agents drop every capability and cannot remount protected paths.

Host port-forwarding listeners bind only to IPv4 loopback. However, the container's own IP can be reached by the Mac and other containers on the same container network. Forwarding is not inbound container-to-container isolation; protect sensitive services at the application layer.

Other current limitations:

- The guest runs Linux. Host Homebrew tools and macOS-only commands are not available there.
- Git worktrees whose metadata lives outside the shared workspace are not automatically supported.
- Custom images for system packages and operation history are on the [roadmap](../TODO.md).
- Codex uses its bundled bubblewrap sandbox. Injected configuration can select embedded mode instead of a shared background server and produce a corresponding warning.

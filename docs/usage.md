# Usage

English · [日本語](usage.ja.md) · [README](../README.md)

## Launch plan

`--dry-run` prints a launch plan without contacting the container service, importing credentials, or reserving ports. A new container name in the plan is only a preview.

## Container lifecycle

Containers are reused by default. `--auto-stop` stops the container after the CLI exits unless another jail session is using it. The container and its home volume remain available for the next launch.

Names use the project directory and local creation time:

```text
jail-my-app-20261001-153000-123456
```

The final component is microseconds. In a Git repository, the directory name comes from the repository root. Non-ASCII-alphanumeric characters are replaced with `-`; an empty result becomes `project`. Names remain unchanged when containers are reused or restarted. A launch after `recreate` generates a new name. Legacy containers are not automatically renamed or deleted.

Home-volume identifiers and lifecycle locks use a stable key derived from the project's path, independent of the visible container name.

When resource, policy, port, image, or runtime changes require replacement, jail asks you to recreate explicitly:

```sh
# Close active sessions first; run from the affected project.
jail recreate
jail claude  # or jail codex
```

> `recreate` deletes the container's root filesystem, including packages and files added outside its persistent home or shared mounts. It preserves the project's home volume, CLI settings/history and host files. Host login is re-imported on the next authenticated launch.

Home volumes and images are never deleted automatically. `jail cleanup` lists jail's home volumes and images, each with its project path where known, creation date and the containers using it. Deletion runs only for the names you pass, or for all of them with `--all-delete`:

```sh
jail cleanup
jail cleanup volume jail-0123456789abcdef-home
jail cleanup image jail-runtime:0123456789abcdef0123
jail cleanup --all-delete
```

`--all-delete` asks for no confirmation. It first stops and deletes every container jail has a record of, then deletes jail's home volumes and images one by one by name. It refuses to start while any jail session is active. It never touches containers jail has no record of, or volumes and images in other formats; if one of those containers uses a jail volume or image, the deletion stops with an error.

Only names in jail's own formats (`jail-<16 hex>-home`, `jail-runtime:<20 hex>`) are accepted. A volume or image used by any container is refused; to free a project's home volume, close its sessions and run `jail recreate` in that project first. Deleting a home volume permanently removes that project's CLI settings and history; the next launch creates an empty one. A deleted image is rebuilt when a configuration needs it again. New home volumes carry their project path as a label, so the path remains visible after jail state is removed.

## Ephemeral sessions

`--ephemeral` starts a new container that mounts no host directory: not the project, not `workspace.mounts`, and no home volume. The agent starts in `/home/jail` inside the container, and everything it writes, including CLI settings and history, is deleted with the container when the session ends. The host login is still imported as usual, and network policy, resources and port forwarding follow the configuration. It works from any directory.

```sh
jail --ephemeral claude
```

If jail is killed before it can delete the container, `jail cleanup --all-delete` removes it.

## Development servers

By default, jail forwards container TCP ports **3000, 5173, 8000, and 8080** to your Mac's `127.0.0.1`. Port forwarding does **not** start a server.

The server must listen on `0.0.0.0`, not only on the container's `127.0.0.1`. For example, an agent can start one with:

```sh
node -e 'require("http").createServer((req, res) => res.end("hello from jail\n")).listen(8080, "0.0.0.0")'
```

Check the assigned host port in jail's launch output before connecting:

```sh
# If launch output shows "jail: http://127.0.0.1:8080 -> container:8080 (TCP)":
curl --noproxy '*' http://127.0.0.1:8080
```

If the same host port is occupied during creation, jail allocates a different available port. Launch output shows the mapping. Displayed HTTP URLs are conveniences for web development; the transport itself is TCP.

```toml
[network]
published_ports = [3000, 5173, 8000, 8080] # [] disables forwarding
auto_port = true                        # false makes conflicts an error
```

Mappings are fixed at creation. If a saved host port is occupied when restarting a stopped container, jail returns an error instead of silently changing it. Free the port or recreate the container to allocate new mappings. Port configuration changes also require recreation.

`network.mode = "none"` disables forwarding. In `allowlist` mode, replies to inbound connections on configured service ports are allowed without permitting direct outbound connections. UDP and automatic detection of all listening ports are not supported.

## Troubleshooting

### A forwarded port resets or refuses the connection

A published port is not proof that an application is listening. Check the following:

1. The container is running and the server has started successfully.
2. The server listens on `0.0.0.0` and the expected container port.
3. You are using the **host** port shown in the mapping, which may differ from the container port.
4. You are connecting to `127.0.0.1`, not an IPv6 `localhost` resolution, and bypassing any local proxy when testing.
5. The network mode is not `none`, and the container was recreated after port configuration changed.

A reset alone does not identify the cause; inspect the server's output before changing settings.

### Configuration or image changed

Close active sessions, then run `jail recreate` from the affected project. Review the [deletion scope](#container-lifecycle) first. Existing containers are not silently replaced.

### Authentication is missing or expired

Run `claude auth login` or `codex login` on the Mac. Running sessions receive the new login, or a login the host CLI refreshed, within about 15 seconds; retry the failed request. With `auth.import_host = false`, close active sessions and run `jail auth claude` / `jail auth codex`. There is no API-key fallback.

### A saved host port is already occupied

Free that port, or recreate the stopped container to allocate new ports. `network.auto_port` only resolves conflicts when creating a container, not when restarting one.

#!/bin/bash
# Actual Apple container smoke checks. No host credentials or AI requests.
set -euo pipefail
task_repo="$(cd "$(dirname "$0")/.." && pwd)"
task_cli="$task_repo/target/release/jail"
task_tmp="$(mktemp -d /private/tmp/jail-smoke.XXXXXXXX)"
mkdir "$task_tmp/project"
export JAIL_HOME="$task_tmp/state"
cd "$task_tmp/project"
task_config="$task_repo/tests/config/open.toml"
task_key="$("$task_cli" --config "$task_config" --dry-run codex --version | python3 -c 'import json,sys; print(json.load(sys.stdin)["project_key"])')"
if [[ ! "$task_key" =~ ^jail-[0-9a-f]{16}$ ]]; then exit 1; fi
run_jail() { "$task_cli" --config "$task_config" "$@"; }
cleanup() {
    # Only the uniquely created test container; no global stop/prune operation.
    run_jail recreate >/dev/null 2>&1 || true
    container volume delete "$task_key-home" >/dev/null 2>&1 || true
    printf 'Test files preserved at %s\n' "$task_tmp"
}
trap cleanup EXIT

run_jail codex --version
task_id="$(run_jail status --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["containers"][0]["id"])')"
if [[ ! "$task_id" =~ ^jail-project-[0-9]{8}-[0-9]{6}-[0-9]{6}$ ]]; then exit 1; fi
run_jail claude --version
run_jail status
run_jail ps
# Exercise the real native sandbox without a model request. This diagnostic
# deliberately uses the trusted supervisor, not the restricted public command.
container exec --user root "$task_id" /usr/local/bin/jail-guest launch codex \
    sandbox -P :read-only -- /bin/sh -c \
    'test -r /etc/os-release; if touch native-readonly-probe 2>/dev/null; then exit 9; fi'
test ! -f native-readonly-probe
printf 'PASS: native Codex sandbox executes commands and denies writes\n'
printf 'set -e; test "$(id -u)" != 0; touch writable-probe; test -f writable-probe; test "$(grep NoNewPrivs /proc/self/status | awk '\''{print $2}'\'')" = 1; exit\n' | run_jail --auto-stop shell
test -f writable-probe
run_jail status --json
printf 'PASS: CLI startup, monitoring, non-root, no_new_privs, workspace write, auto-stop\n'

# Protected path replacement and aliases must fail, not just ordinary writes.
printf 'set -e; for p in .git/config .mcp.json .bashrc; do if echo bad >> "$p" 2>/dev/null; then exit 12; fi; if rm "$p" 2>/dev/null; then exit 13; fi; done; if mv .git .git-moved 2>/dev/null; then exit 14; fi; if touch .git/hooks/bad .claude/bad .codex/bad 2>/dev/null; then exit 15; fi; if ln .git/config alias 2>/dev/null; then exit 16; fi; test -r /home/jail/.codex/auth.json; if cat /run/jail-private/codex.json 2>/dev/null; then exit 17; fi; exit 0\n' | run_jail --auto-stop shell
printf 'PASS: sensitive writes, replacement, parent rename and hard-link aliases denied\n'
printf 'if unshare -Ur -m /bin/sh -c '\''set -e; mkdir -p /tmp/jail-alias; mount --bind "$PWD" /tmp/jail-alias; echo escape >> /tmp/jail-alias/.mcp.json'\'' 2>/dev/null; then exit 18; fi; exit 0\n' | run_jail --auto-stop shell
printf 'PASS: unprivileged mount-namespace alias cannot bypass write protection\n'

check_port_forward() {
    # Non-root dev server launched through the normal shell path, no auth/model.
    printf 'node -e '\''require("http").createServer((q,s)=>s.end("jail-port-"+process.getuid())).listen(3000,"0.0.0.0")'\'' >/tmp/jail-port.log 2>&1 &\nexit\n' | run_jail shell
    task_host_port="$(run_jail status --json | python3 -c 'import json,sys; c=json.load(sys.stdin)["containers"][0]; print(next(p["host"] for p in c["ports"] if p["container"]==3000))')"
    task_response="$(curl --noproxy '*' -fsS --max-time 5 "http://127.0.0.1:$task_host_port")"
    test "$task_response" = "jail-port-$(id -u)"
    printf 'PASS: host localhost reaches non-root development server\n'
}
check_port_forward

run_jail recreate
task_config="$task_repo/tests/config/readonly.toml"
printf 'if touch readonly-probe 2>/dev/null; then exit 9; fi; exit 0\n' | run_jail --auto-stop shell
test ! -f readonly-probe
printf 'PASS: workspace write denied\n'

run_jail recreate
task_config="$task_repo/tests/config/hidden.toml"
cp "$task_repo/tests/fixtures/example.env" .env
printf 'set -e; test ! -s .env; exit\n' | run_jail --auto-stop shell
test -s .env
printf 'PASS: hidden path is masked; host file is preserved\n'

run_jail recreate
task_config="$task_repo/tests/config/offline.toml"
printf 'if curl -fsS --max-time 5 https://example.com >/dev/null 2>&1; then exit 9; fi; exit 0\n' | run_jail --auto-stop shell
printf 'PASS: direct network denied\n'

run_jail recreate
task_config="$task_repo/tests/config/allowlist.toml"
# Do not modify the repository's fixture during policy allow.
cp "$task_config" "$task_tmp/allowlist.toml"
task_config="$task_tmp/allowlist.toml"
check_port_forward
printf 'set -e; curl -fsS --max-time 20 https://example.com >/dev/null; if curl -fsS --max-time 5 https://example.org >/dev/null 2>&1; then exit 9; fi; if HTTPS_PROXY= https_proxy= HTTP_PROXY= http_proxy= ALL_PROXY= all_proxy= curl -fsS --max-time 5 https://example.com >/dev/null 2>&1; then exit 10; fi; exit 0\n' | run_jail shell
run_jail policy log --json | python3 -c 'import json,sys; assert any(e["host"] == "example.org" for e in json.load(sys.stdin))'
run_jail policy allow example.org
printf 'set -e; curl -fsS --max-time 20 https://example.org >/dev/null; exit 0\n' | run_jail --auto-stop shell
printf 'PASS: allowed HTTPS works; forbidden hostname and direct bypass denied\n'
printf 'PASS: metadata-only denial log and live allowlist adjustment\n'

run_jail recreate
task_config="$task_repo/tests/config/noshell.toml"
if run_jail shell; then exit 11; fi
run_jail --auto-stop codex --version
run_jail --auto-stop claude --version
task_features="$(run_jail --auto-stop codex features list)"
for task_feature in shell_tool unified_exec hooks; do
    printf '%s\n' "$task_features" | grep -E "^${task_feature}[[:space:]]+.*false$" >/dev/null
done
printf 'PASS: shell disabled; CLI startup with tool restrictions\n'

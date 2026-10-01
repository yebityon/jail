#!/bin/bash
# Test explicitly requested host login reuse. Never print credential contents.
# Dedicated temporary project; root-only tmpfs credentials; no model request.
set -euo pipefail
task_repo="$(cd "$(dirname "$0")/.." && pwd)"
task_cli="$task_repo/target/release/jail"
task_tmp="$(mktemp -d /private/tmp/jail-auth-smoke.XXXXXXXX)"
mkdir "$task_tmp/project"
export JAIL_HOME="$task_tmp/state"
cd "$task_tmp/project"
task_key="$("$task_cli" --dry-run codex --version | python3 -c 'import json,sys; print(json.load(sys.stdin)["project_key"])')"
cleanup() {
    "$task_cli" recreate >/dev/null 2>&1 || true
    container volume delete "$task_key-home" >/dev/null 2>&1 || true
    printf 'Test files preserved at %s\n' "$task_tmp"
}
trap cleanup EXIT
"$task_cli" auth codex
"$task_cli" codex login status >/dev/null 2>&1
printf 'PASS: Codex accepts imported host login\n'
"$task_cli" auth claude
"$task_cli" claude auth status >/dev/null 2>&1
printf 'PASS: Claude accepts imported host login\n'
task_id="$("$task_cli" status --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["containers"][0]["id"])')"
# Trusted root compares files and emits only a boolean, never credential values.
container exec --user root "$task_id" node -e '
const fs=require("fs");
for(const tool of ["claude","codex"]) {
 const privatePath="/run/jail-private/"+tool+".json";
 const publicPath=tool==="claude"?"/home/jail/.claude/.credentials.json":"/home/jail/.codex/auth.json";
 const real=JSON.parse(fs.readFileSync(privatePath)); const publicText=fs.readFileSync(publicPath,"utf8");
 const secrets=tool==="claude"?[real.claudeAiOauth.accessToken,real.claudeAiOauth.refreshToken]:[real.tokens.access_token,real.tokens.refresh_token,real.tokens.id_token];
 if(secrets.some(s=>s&&publicText.includes(s))) process.exit(19);
 if((fs.statSync(privatePath).mode&0o777)!==0o600 || fs.statSync(privatePath).uid!==0) process.exit(20);
}
console.log("PASS: real tokens absent from agent files and stored root-only");'
printf 'set -e; if cat /run/jail-private/claude.json 2>/dev/null; then exit 21; fi; if cat /run/jail-private/codex.json 2>/dev/null; then exit 22; fi; exit 0\n' | "$task_cli" shell
# Read-only model catalog, not an inference request. Emit only HTTP status.
printf 'set -e; code=$(curl -sS --max-time 30 -o /tmp/jail-model-catalog.json -w "%%{http_code}" "http://127.0.0.1:3131/backend-api/codex/models?client_version=0.159.2"); test "$code" = 200; printf "PASS: OAuth gateway model-catalog HTTP %%s (no inference)\\n" "$code"; exit 0\n' | "$task_cli" shell
"$task_cli" stop

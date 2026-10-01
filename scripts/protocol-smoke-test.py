#!/usr/bin/env python3
"""Exercise pinned native CLI HTTP/SSE using fake credentials and loopback only.
No real login, no upstream requests, no inference/billing. macOS sandbox enforced.
"""
import base64
import http.server
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading

captures = []
class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass
    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(b'{"models":[],"data":[]}')
    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        self.rfile.read(length)
        captures.append((self.path, dict(self.headers)))
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        if self.path.startswith("/v1/messages"):
            events = [
                ("message_start", {"type":"message_start", "message":{"id":"mock", "type":"message", "role":"assistant", "model":"claude-sonnet-4-6", "content":[], "stop_reason":None, "stop_sequence":None, "usage":{"input_tokens":1,"output_tokens":0}}}),
                ("content_block_start", {"type":"content_block_start", "index":0, "content_block":{"type":"text", "text":""}}),
                ("content_block_delta", {"type":"content_block_delta", "index":0, "delta":{"type":"text_delta","text":"mock-only"}}),
                ("content_block_stop", {"type":"content_block_stop","index":0}),
                ("message_delta", {"type":"message_delta", "delta":{"stop_reason":"end_turn","stop_sequence":None}, "usage":{"output_tokens":1}}),
                ("message_stop", {"type":"message_stop"}),
            ]
        else:
            response = {"id":"mock", "object":"response", "created_at":1, "status":"completed", "model":"gpt-6.1-sol", "output":[{"id":"msg_mock", "type":"message", "role":"assistant", "status":"completed", "content":[{"type":"output_text","text":"mock-only","annotations":[]}]}], "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}
            events = [("response.completed", {"type":"response.completed", "response":response})]
        for event, payload in events:
            self.wfile.write(("event: " + event + "\ndata: " + json.dumps(payload) + "\n\n").encode())
        self.wfile.flush()

assert Path("/usr/bin/sandbox-exec").exists(), "macOS sandbox-exec is required"
server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
base = f"http://127.0.0.1:{server.server_port}"
tmp = Path(tempfile.mkdtemp(prefix="jail-protocol-", dir="/private/tmp"))
workspace = tmp / "project"
workspace.mkdir()
home = tmp / "home"
home.mkdir()
codex_home = home / ".codex"
codex_home.mkdir()
claude_home = home / ".claude"
claude_home.mkdir()
def b64(value):
    return base64.urlsafe_b64encode(json.dumps(value).encode()).decode().rstrip("=")
jwt = b64({"alg":"none","typ":"JWT"})+"."+b64({"sub":"jail","email":"jail@localhost","exp":4102444800,"https://api.openai.com/auth":{"chatgpt_account_id":"jail-proxy","chatgpt_plan_type":"plus"}})+".jail"
(codex_home / "auth.json").write_text(json.dumps({"auth_mode":"chatgpt", "tokens":{"id_token":jwt,"access_token":"jail-proxy-placeholder","refresh_token":"jail-proxy-placeholder","account_id":"jail-proxy"},"last_refresh":"2099-01-01T00:00:00Z"}))
(claude_home / ".credentials.json").write_text(json.dumps({"claudeAiOauth":{"accessToken":"jail-proxy-placeholder","refreshToken":"jail-proxy-placeholder","expiresAt":4102444800000,"scopes":["user:inference","user:profile"],"subscriptionType":"max"}}))
(home / ".claude.json").write_text('{"hasCompletedOnboarding":true,"theme":"dark"}')
env = {"HOME":str(home),"PATH":os.environ["PATH"],"CODEX_HOME":str(codex_home),"CLAUDE_CONFIG_DIR":str(claude_home),"ANTHROPIC_BASE_URL":base,"CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC":"1","DISABLE_AUTOUPDATER":"1","TERM":"dumb"}
profile = '(version 1) (allow default) (deny network-outbound) (allow network-outbound (remote ip "localhost:*"))'
def run(tool, args):
    cli = shutil.which(tool)
    assert cli, f"{tool} is required"
    result = subprocess.run(["/usr/bin/sandbox-exec", "-p", profile, cli, *args], cwd=workspace, env=env, capture_output=True, timeout=40)
    (tmp / (tool+".stdout")).write_bytes(result.stdout)
    (tmp / (tool+".stderr")).write_bytes(result.stderr)
    assert result.returncode == 0, f"{tool} mock test failed; inspect {tmp}/{tool}.stderr"
run("codex", ["-c",'model_provider="jail_oauth"',"-c",f'model_providers.jail_oauth={{ name="Jail subscription gateway",base_url="{base}/backend-api/codex",wire_api="responses",requires_openai_auth=true,supports_websockets=false }}',"-c",f'chatgpt_base_url="{base}/backend-api"',"-c",'features.apps=false',"-c",'features.plugins=false',"exec","--skip-git-repo-check","mock-only protocol probe"])
run("claude", ["-p","mock-only protocol probe", "--model","claude-sonnet-4-6", "--tools","", "--strict-mcp-config","--mcp-config",'{"mcpServers":{}}'])
for prefix in ["/backend-api/codex/responses", "/v1/messages"]:
    matching = [(p,h) for p,h in captures if p.startswith(prefix)]
    assert matching, f"no native requests for {prefix}"
    for path, headers in matching:
        normalized = {k.lower():v for k,v in headers.items()}
        assert normalized.get("authorization") == "Bearer jail-proxy-placeholder", f"unexpected auth scheme: {prefix}"
        assert "transfer-encoding" not in normalized
        assert "content-length" in normalized
        print("PASS: native OAuth placeholder + HTTP/SSE", prefix, "content-encoding="+normalized.get("content-encoding","none"))
server.shutdown()
print(f"Mock-only test files preserved at {tmp}")

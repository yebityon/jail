#!/usr/bin/env python3
"""A deliberately small Apple container protocol fake; never contacts a service."""
import json
import os
from pathlib import Path
import sys
import time

args = sys.argv[1:]
root = Path(os.environ["JAIL_TEST_BACKEND"])
state_path = root / "containers.json"
state = json.loads(state_path.read_text()) if state_path.exists() else []
with (root / "calls.jsonl").open("a") as log:
    log.write(json.dumps(args) + "\n")

def save():
    state_path.write_text(json.dumps(state))

def record(name):
    return next((r for r in state if r["configuration"]["id"] == name), None)

def tracked(kind):
    """Volumes/images exist implicitly unless a test opts in to tracking them."""
    path = root / f"{kind}s.json"
    return path, (json.loads(path.read_text()) if path.exists() else None)

cmd = args[0]
if cmd == "system":
    pass
elif cmd in ("volume", "image"):
    path, items = tracked(cmd)
    name = args[-1]
    if args[1] == "list":
        print(json.dumps([
            {"id": n, "configuration": {"name": n, "labels": labels,
                                        "creationDate": "2026-10-01T03:03:56Z"},
             "variants": [{"size": 3 * 1024 * 1024}]}
            for n, labels in (items or {}).items()
        ]))
    elif args[1] == "inspect":
        sys.exit(0 if items is None or name in items else 1)
    elif args[1] == "create" and items is not None:
        items[name] = dict(args[i+1].split("=", 1) for i, a in enumerate(args) if a == "--label")
        path.write_text(json.dumps(items))
    elif args[1] == "delete":
        if items is None or name not in items:
            sys.exit(1)
        del items[name]
        path.write_text(json.dumps(items))
elif cmd == "list":
    print(json.dumps(state))
elif cmd == "inspect":
    print(json.dumps([record(args[1])]))
elif cmd == "run":
    name = args[args.index("--name") + 1]
    labels = dict(args[i+1].split("=", 1) for i, a in enumerate(args) if a == "--label")
    mounts = [args[i+1] for i, a in enumerate(args) if a == "--mount"]
    state.append({"configuration": {"id": name, "labels": labels, "mounts": mounts,
                                    "image": {"reference": args[-1]}},
                  "status": {"state": "running"}})
    save()
elif cmd in ("start", "stop"):
    record(args[-1])["status"]["state"] = "running" if cmd == "start" else "stopped"
    save()
elif cmd == "delete":
    state = [r for r in state if r["configuration"]["id"] != args[-1]]
    save()
elif cmd == "exec":
    if "policy-set" in args:
        (root / "live-policy.json").write_text(sys.stdin.read())
    if "policy-log" in args:
        print(json.dumps([{"time": 123, "host": "example.org", "port": 443, "reason": "allowlist"}]))
    if "has-auth" in args:
        sys.exit(0 if os.environ.get("JAIL_TEST_HAS_AUTH") else 3)
    if "import-auth" in args:
        (root / "auth-import.json").write_text(sys.stdin.read())
    if "sync-auth" in args:
        (root / "auth-sync.json").write_text(sys.stdin.read())
    if "launch" in args:
        if os.environ.get("JAIL_TEST_LOGIN_PTY"):
            import tty
            tty.setraw(0)
            print("NATIVE_READY", flush=True)
            line = bytearray()
            while True:
                b = os.read(0, 1)
                if not b or b == b"\x04":
                    break
                if b == b"\x15":
                    line.clear()
                elif b == b"\x0c":
                    print("NATIVE_READY", flush=True)
                elif b in (b"\r", b"\n"):
                    print("NATIVE_LINE:" + line.decode(), flush=True)
                    line.clear()
                else:
                    line.extend(b)
            sys.exit(0)
        time.sleep(float(os.environ.get("JAIL_TEST_SLEEP", "0")))
        sys.exit(int(os.environ.get("JAIL_TEST_EXIT", "0")))
elif cmd == "--version":
    print("container 1.5.0 (test fixture)")
else:
    raise SystemExit(f"unexpected container command: {args}")

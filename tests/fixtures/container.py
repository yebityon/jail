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

cmd = args[0]
if cmd == "system" or cmd == "volume" or cmd == "image":
    pass
elif cmd == "list":
    print(json.dumps(state))
elif cmd == "inspect":
    print(json.dumps([record(args[1])]))
elif cmd == "run":
    name = args[args.index("--name") + 1]
    labels = dict(args[i+1].split("=", 1) for i, a in enumerate(args) if a == "--label")
    state.append({"configuration": {"id": name, "labels": labels}, "status": {"state": "running"}})
    save()
elif cmd in ("start", "stop"):
    record(args[-1])["status"]["state"] = "running" if cmd == "start" else "stopped"
    save()
elif cmd == "delete":
    state = [r for r in state if r["configuration"]["id"] != args[-1]]
    save()
elif cmd == "stats":
    time.sleep(float(os.environ.get("JAIL_TEST_STATS_SLEEP", "0")))
    if "json" in args:
        print(json.dumps([
            {"id": r["configuration"]["id"], "cpuUsageUsec": 1000000,
             "memoryUsageBytes": 128 * 1024 * 1024, "memoryLimitBytes": 256 * 1024 * 1024,
             "numProcesses": 4}
            for r in state if r["status"]["state"] == "running"
        ]))
    else:
        print("CPU 0.0%  MEMORY 64MiB")
elif cmd == "exec":
    if "policy-set" in args:
        (root / "live-policy.json").write_text(sys.stdin.read())
    if "policy-log" in args:
        print(json.dumps([{"time": 123, "host": "example.org", "port": 443, "reason": "allowlist"}]))
    if "has-auth" in args:
        sys.exit(3)
    if "launch" in args:
        time.sleep(float(os.environ.get("JAIL_TEST_SLEEP", "0")))
        sys.exit(int(os.environ.get("JAIL_TEST_EXIT", "0")))
    if "ps" in args:
        print("1 0 0 60 0.0 0.1 jail-guest\n10 0 501 30 1.5 3.0 codex\n11 10 501 2 5.0 0.2 git")
elif cmd == "--version":
    print("container 1.5.0 (test fixture)")
else:
    raise SystemExit(f"unexpected container command: {args}")

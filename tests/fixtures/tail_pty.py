#!/usr/bin/env python3
"""Bounded PTY dashboard check; sends SIGINT only to its own monitor process."""
import errno
import fcntl
import json
import os
import pty
import re
import select
import signal
import struct
import subprocess
import sys
import termios
import time

master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
env = dict(os.environ, TERM="xterm-256color")
child = subprocess.Popen(sys.argv[1:], stdin=slave, stdout=slave, stderr=slave,
                         start_new_session=True, env=env)
# Keep the slave open until draining is complete: on macOS the final close can
# discard unread output, including the monitor's cursor/screen restoration.
chunks = []

def read_available():
    if not select.select([master], [], [], 0.05)[0]:
        return True
    try:
        data = os.read(master, 65536)
    except OSError as error:
        if error.errno == errno.EIO:
            return False
        raise
    if not data:
        return False
    chunks.append(data)
    return True

try:
    deadline = time.monotonic() + 4.5
    while time.monotonic() < deadline and child.poll() is None:
        if not read_available():
            break
    if child.poll() is None:
        child.send_signal(signal.SIGINT)
    deadline = time.monotonic() + 3
    while child.poll() is None and time.monotonic() < deadline:
        if not read_available():
            break
    child.wait(timeout=3)
    while read_available():
        if not select.select([master], [], [], 0.05)[0]:
            break
finally:
    if child.poll() is None:
        child.kill()
        child.wait()
    os.close(master)
    os.close(slave)

text = b"".join(chunks).decode("utf-8", errors="replace")
print(json.dumps({
    "code": child.returncode,
    "frames": re.findall(r"JAIL LIVE \| (\d\d:\d\d:\d\d)", text),
    "restored": text.endswith("\x1b[0m\x1b[?25h\x1b[?1049l"),
    "has_memory": "128.0 MiB / 256.0 MiB (50.0%)" in text,
    "has_process": "codex" in text,
    "has_freshness": "sample " in text,
    "ending": text[-200:],
}))

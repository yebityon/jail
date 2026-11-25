#!/usr/bin/env python3
"""Exercise the real PTY mediation; a typed /login must never reach the agent CLI."""
import os
import select
import subprocess
import sys
import termios
import time

import pty

master, slave = pty.openpty()
original = termios.tcgetattr(slave)
proc = subprocess.Popen(sys.argv[1:], stdin=slave, stdout=slave, stderr=slave,
                        start_new_session=True, env={**os.environ, "JAIL_TEST_LOGIN_PTY": "1"})
pending = bytearray()
transcript = bytearray()
def until(marker):
    global pending
    deadline = time.monotonic() + 15
    while marker not in pending:
        assert time.monotonic() < deadline, (marker, bytes(transcript))
        ready, _, _ = select.select([master], [], [], 0.1)
        if ready:
            data = os.read(master, 16384)
            assert data, bytes(transcript)
            pending.extend(data)
            transcript.extend(data)
        assert proc.poll() is None, bytes(transcript)
    pos = pending.index(marker) + len(marker)
    found = pending[:pos]
    pending = pending[pos:]
    return bytes(found)

try:
    until(b"NATIVE_READY")
    # jail answers the command itself and points to the sign-in on the Mac.
    os.write(master, b"/login\r")
    until(b"login from jail is disabled")
    until(os.environb[b"JAIL_TEST_HOST_LOGIN"])
    until(b"press Enter")
    os.write(master, b"\n")
    until(b"NATIVE_READY")
    os.write(master, b"still-here\r")
    until(b"NATIVE_LINE:still-here")
    assert b"NATIVE_LINE:/login" not in transcript
    os.write(master, b"\x04")
    assert proc.wait(timeout=5) == 0
    restored = termios.tcgetattr(slave)
    # Darwin sets the kernel-maintained PENDIN bit when cooked mode is restored.
    restored[3] &= ~getattr(termios, "PENDIN", 0)
    original[3] &= ~getattr(termios, "PENDIN", 0)
    assert restored == original, (original, restored)
finally:
    if proc.poll() is None:
        proc.kill()
        proc.wait()
    os.close(master)
    os.close(slave)

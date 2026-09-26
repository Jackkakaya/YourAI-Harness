"""Real input-to-presentation regressions; offline PTY, no latency benchmark."""
import fcntl
import os
import signal
import struct
import termios
import time
from smoke_support import Probe, wait_exit


def changed(p, label, action):
    baseline = p.tap.frames
    action()
    deadline = time.monotonic() + 3
    while p.tap.frames == baseline:
        assert time.monotonic() < deadline, f"No presentation after {label}"
        time.sleep(0.01)
    time.sleep(0.15)


def quiet(p):
    time.sleep(0.4)
    baseline = p.tap.frames
    time.sleep(0.4)
    assert p.tap.frames == baseline, "idle frame keeps resetting the terminal cursor"


with Probe(env={"YOURAI_TUI_SYNC": "on"}) as p:
    p.wait(b"New session")
    time.sleep(1.5)
    changed(p, "open menu", lambda: p.send(b"/"))
    quiet(p)
    changed(p, "menu selection", lambda: p.send(b"\x1b[B"))
    changed(p, "dismiss menu", lambda: p.send(b"\x1b"))
    quiet(p)
    def resize():
        fcntl.ioctl(p.master, termios.TIOCSWINSZ, struct.pack("HHHH", 35, 110, 0, 0))
        os.kill(p.child.pid, signal.SIGWINCH)
    changed(p, "idle resize", resize)
    p.send(b"\x15hello\r")
    p.wait(b"Section 499")
    time.sleep(1.5)
    quiet(p)
    changed(p, "latest turn", lambda: p.send(b"\x1b[1;5H"))
    changed(p, "return to bottom", lambda: p.send(b"\x1b[1;5F"))
    p.send(b"\x1b[<0;5;5M")
    changed(p, "first drag", lambda: p.send(b"\x1b[<32;10;5M"))
    changed(p, "extend drag", lambda: p.send(b"\x1b[<32;20;5M"))
    changed(p, "clear selection", lambda: p.send(b"\x1b"))
    quiet(p)
    p.send(b"\x11")
    assert wait_exit(p.child, p.master) == 0
print("PASS: menu, resize, navigation, selection present; idle emits no frames")

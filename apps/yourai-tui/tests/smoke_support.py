"""PTY helpers: continue draining frames while waiting for a terminal to exit."""
import os
import select
import time


def wait_exit(child, master, timeout=10):
    deadline = time.monotonic() + timeout
    while child.poll() is None:
        if time.monotonic() >= deadline:
            raise AssertionError("TUI did not exit after Ctrl-Q")
        if select.select([master], [], [], 0.05)[0]:
            data = os.read(master, 65536)
            if b'\x1b[6n' in data:
                os.write(master, b'\x1b[1;1R')
    return child.returncode

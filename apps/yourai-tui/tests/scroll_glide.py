"""Scroll pacing probe: sustained wheel bursts, frame rate and settle tail.

Runs the app with TERM=tmux-256color so the ScrollBackend takes the cell-diff
path (what real muxes see), injects wheel events at a trackpad-like rate, and
reports emitted frames per second plus the settle tail: how long output keeps
flowing after the last injected event. Unbounded tails mean queued frames are
draining behind the user's finger (jelly scrolling). Frames are counted via
the 2026 sync marker, forced on here (the auto rule keeps it off inside muxes;
the wrapper itself costs only 16 bytes per frame).
"""
import sys
import time
from pathlib import Path

from smoke_support import Probe, wait_exit


def run():
    binary = Path(sys.argv[1]) if len(sys.argv) > 1 else None
    hz = float(sys.argv[2]) if len(sys.argv) > 2 else 120.0
    seconds = float(sys.argv[3]) if len(sys.argv) > 3 else 2.0
    with Probe(binary, term='tmux-256color',
               env={'YOURAI_TUI_SYNC': 'on'}) as p:
        p.wait(b'New session')
        p.send(b'render a long transcript\r')
        p.wait(b'Section 499')
        time.sleep(0.5)
        p.reset()
        sent = p.burst(hz, seconds)
        last_event = time.monotonic()
        tail_ms = p.tap.quiet(idle=0.2, timeout=3, since=last_event)
        t = p.tap
        during = [ts for ts in t.frame_times if ts <= last_event]
        print(f'events={sent} ({hz:.0f}Hz {seconds}s) -> '
              f'frames during={len(during)} ({len(during) / seconds:.0f}fps), '
              f'total={t.frames}, tail={tail_ms:.0f}ms, {t.bytes / seconds / 1024:.0f}KB/s, '
              f'bytes/frame={t.bytes / max(1, t.frames) / 1024:.2f}KB')
        p.send(b'\x11')
        assert wait_exit(p.child, p.master) == 0


if __name__ == '__main__':
    run()

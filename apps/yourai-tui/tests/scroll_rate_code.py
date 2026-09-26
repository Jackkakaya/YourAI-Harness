"""Sustained-scroll frame rate on dense code content (heavy SGR repaints)."""
import sys
import time
from pathlib import Path

from smoke_support import Probe, code_text, wait_exit


def run():
    binary = Path(sys.argv[1]) if len(sys.argv) > 1 else None
    hz = float(sys.argv[2]) if len(sys.argv) > 2 else 90.0
    seconds = float(sys.argv[3]) if len(sys.argv) > 3 else 2.0
    with Probe(binary, text=code_text(), env={'YOURAI_TUI_SYNC': 'on'}) as p:
        p.wait(b'New session')
        p.send(b'render a long transcript\r')
        p.wait(b'results.')
        time.sleep(0.5)
        p.reset()
        sent = p.burst(hz, seconds)
        time.sleep(0.3)
        t = p.tap
        print(f'code events={sent} ({hz:.0f}Hz for {seconds}s) -> '
              f'frames={t.frames} ({t.frames / seconds:.0f}fps), '
              f'{t.bytes / seconds / 1024:.0f}KB/s, '
              f'bytes/frame={t.bytes / max(1, t.frames) / 1024:.1f}KB')
        p.send(b'\x11')
        assert wait_exit(p.child, p.master) == 0


if __name__ == '__main__':
    run()

"""Measure per-frame output bytes during a 1-line scroll (emulator paint excluded)."""
import sys
import time
from pathlib import Path

from smoke_support import DEFAULT_BIN, WHEEL_UP, Probe, wait_exit


def run():
    binary = Path(sys.argv[1]) if len(sys.argv) > 1 else DEFAULT_BIN
    cols, rows = (int(sys.argv[2]), int(sys.argv[3])) if len(sys.argv) > 2 else (160, 48)
    with Probe(binary, cols=cols, rows=rows, env={'YOURAI_TUI_SYNC': 'on'}) as p:
        p.wait(b'New session')
        p.send(b'render a long transcript\r')
        p.wait(b'Section 499')
        time.sleep(0.5)
        # Measure one frame = bytes between one 2026h..2026l pair per scroll.
        sizes = []
        latencies = []
        for _ in range(40):
            start_len = len(p.tap.buf)
            base_frames = p.tap.frames
            started = time.monotonic()
            p.send(WHEEL_UP)
            deadline = started + 5
            while p.tap.frames <= base_frames:
                if time.monotonic() >= deadline:
                    raise AssertionError('no frame emitted after a wheel event')
                time.sleep(0.001)
            latencies.append((time.monotonic() - started) * 1000)
            # bytes strictly inside this frame's sync window
            frame = bytes(p.tap.buf[start_len:])
            s = frame.find(b'\x1b[?2026h')
            e = frame.find(b'\x1b[?2026l')
            sizes.append(max(0, e - s) if s >= 0 else len(frame))
            time.sleep(0.02)
        sizes.sort()
        latencies.sort()
        print(f'{cols}x{rows}: frame bytes p50={sizes[20] / 1024:.1f}KB '
              f'p95={sizes[38] / 1024:.1f}KB; '
              f'latency p50={latencies[20]:.2f}ms p95={latencies[38]:.2f}ms')
        p.send(b'\x11')
        assert wait_exit(p.child, p.master) == 0


if __name__ == '__main__':
    run()

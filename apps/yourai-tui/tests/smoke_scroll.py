"""Manual PTY latency probe; optional binary path. Excludes emulator/GPU paint time."""
import sys
import time
from pathlib import Path

from smoke_support import WHEEL_UP, Probe, wait_exit


def run():
    binary = Path(sys.argv[1]) if len(sys.argv) > 1 else None
    with Probe(binary, env={'YOURAI_TUI_SYNC': 'on'}) as p:
        p.wait(b'New session')
        p.send(b'render a long transcript\r')
        p.wait(b'Section 499')
        time.sleep(0.5)
        # Measure complete synchronized frames (wait for the closing 2026l),
        # not arrival of a first output byte.
        samples = []
        for _ in range(60):
            base = p.tap.frames
            started = time.monotonic()
            p.send(WHEEL_UP)
            deadline = started + 5
            while p.tap.frames <= base:
                if time.monotonic() >= deadline:
                    raise AssertionError('no frame emitted after a wheel event')
                time.sleep(0.001)
            samples.append((time.monotonic() - started) * 1000)
            time.sleep(0.02)
        samples.sort()
        # Simulate queued legacy hover reports followed by a keystroke.
        time.sleep(0.2)
        started = time.monotonic()
        p.send(b'\x1b[<35;10;10M' * 256 + b'INPUT_PROBE')
        p.wait(b'INPUT_PROBE')
        backlog = (time.monotonic() - started) * 1000
        print(f'PTY scroll p50={samples[30]:.2f}ms p95={samples[57]:.2f}ms; '
              f'queued-input={backlog:.2f}ms')
        p.send(b'\x11')
        assert wait_exit(p.child, p.master) == 0


if __name__ == '__main__':
    run()

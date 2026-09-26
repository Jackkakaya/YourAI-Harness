"""Run opencode (same mock model) inside tmux and measure its scroll cost,
for comparison with scroll_tmux.py on the same chain."""
import re
import sys
import time
from collections import Counter

from smoke_support import MuxProbe, WHEEL_UP, code_text, write_opencode_config


def run():
    hz = float(sys.argv[1]) if len(sys.argv) > 1 else 30.0
    seconds = float(sys.argv[2]) if len(sys.argv) > 2 else 2.0

    def pane(tmp, port):
        return ['opencode'], {'OPENCODE_CONFIG': str(write_opencode_config(tmp, port))}

    with MuxProbe(pane, text=code_text(),
                  passthrough=('OPENCODE_CONFIG',)) as p:
        # opencode may show a trust prompt on first run in a new dir.
        p.wait(b'Ask anything', timeout=40)
        time.sleep(3.0)
        p.send(b'render a long transcript\r')
        p.wait(b'results.', timeout=60)
        time.sleep(2.0)

        def first_chunk_latency():
            p.reset()
            started = time.monotonic()
            p.send(WHEEL_UP)
            deadline = started + 1.0
            while not p.tap.chunks:
                if time.monotonic() >= deadline:
                    return None
                time.sleep(0.001)
            return (p.tap.chunks[0][0] - started) * 1000

        latencies = []
        for _ in range(30):
            x = first_chunk_latency()
            if x is not None:
                latencies.append(x)
            time.sleep(0.06)  # spaced events: match scroll_tmux.py's protocol
        latencies.sort()
        p.reset()
        burst_from = len(p.tap.buf)
        sent = p.burst(hz, seconds)
        time.sleep(0.6)
        t = p.tap
        per_event = t.bytes / max(1, sent)
        p50 = latencies[len(latencies) // 2] if latencies else float('nan')
        p95 = latencies[int(len(latencies) * 0.95)] if latencies else float('nan')
        print(f'opencode-in-tmux: events={sent} {t.bytes / seconds / 1024:.0f}KB/s, '
              f'{per_event / 1024:.2f}KB/event, latency p50={p50:.1f}ms p95={p95:.1f}ms n={len(latencies)}')
        # Which sequences dominate?
        data = bytes(t.buf[burst_from:])
        counts = Counter(s for s in re.findall(rb'\x1b\[[0-9;?]*[a-zA-Z]', data))
        top = ' '.join(f'{c}x{s.decode()}' for s, c in counts.most_common(6))
        print(f'  top sequences: {top}')


if __name__ == '__main__':
    run()

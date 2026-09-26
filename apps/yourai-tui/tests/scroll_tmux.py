"""Measure tmux-client-side output when scrolling inside a tmux pane.

Chain: probe PTY -> tmux client -> tmux server -> pane (yourai-tui + mock model).
Wheel events written to the PTY are forwarded by tmux to the pane's app.
We count bytes tmux sends back over the PTY (approximates the SSH link).
Client-side "paints" are chunks separated by >30ms: tmux consumes the app's
2026 wrappers (or the app omits them inside muxes), so there is no per-frame
marker to count at this end.
"""
import sys
import time
from pathlib import Path

from smoke_support import DEFAULT_BIN, MuxProbe, WHEEL_UP, write_yourai_config


def run():
    binary = Path(sys.argv[1]) if len(sys.argv) > 1 else DEFAULT_BIN
    hz = float(sys.argv[2]) if len(sys.argv) > 2 else 30.0
    seconds = float(sys.argv[3]) if len(sys.argv) > 3 else 2.0
    extra_env = dict(arg.split('=', 1) for arg in sys.argv[4:])

    def pane(tmp, port):
        return [str(binary), '--config', str(write_yourai_config(tmp, port))]

    with MuxProbe(pane, env=extra_env) as p:
        p.wait(b'New session')
        p.send(b'render a long transcript\r')
        p.wait(b'results.')
        time.sleep(1.0)

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

        # Per-event latency: wheel event -> first output byte from tmux.
        # The 60ms gap keeps events spaced out; back-to-back events would
        # legitimately hit the 12ms frame-pacing gate (~15ms by design).
        latencies = []
        for _ in range(30):
            x = first_chunk_latency()
            if x is not None:
                latencies.append(x)
            time.sleep(0.06)
        latencies.sort()
        time.sleep(0.3)
        p.reset()
        sent = p.burst(hz, seconds)
        time.sleep(0.5)
        t = p.tap
        paints = 0
        last = None
        for ts, _ in t.chunks:
            if last is None or ts - last > 0.03:
                paints += 1
            last = ts
        p50 = latencies[len(latencies) // 2] if latencies else float('nan')
        p95 = latencies[int(len(latencies) * 0.95)] if latencies else float('nan')
        label = ' '.join(f'{k}={v}' for k, v in extra_env.items()) or 'defaults'
        print(f'tmux [{label}]: events={sent} paints={paints} ({paints / seconds:.0f}/s) '
              f'{t.bytes / seconds / 1024:.0f}KB/s, {t.bytes / max(1, sent) / 1024:.2f}KB/event, '
              f'latency p50={p50:.1f}ms p95={p95:.1f}ms n={len(latencies)}')


if __name__ == '__main__':
    run()

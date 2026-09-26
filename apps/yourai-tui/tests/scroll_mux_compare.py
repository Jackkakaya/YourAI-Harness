"""Compare wheel-scroll cost across yourai-tui, opencode and codex under a
mux, from the CLIENT side (what a remote terminal would receive).

Chain: probe PTY -> tmux client -> tmux server (mouse on, UURemote-style
copy-mode wheel bindings) -> pane app + mock model. Wheel events written to
the PTY are what SwiftTerm would send; we measure what tmux writes back.
"""
import sys
import time
from pathlib import Path

from pty_probe import Screen

from smoke_support import DEFAULT_BIN, MuxProbe, write_opencode_config, write_yourai_config

APPS = ('yourai', 'opencode', 'codex')


def pane_for(app):
    def pane(tmp, port):
        if app == 'yourai':
            return [str(DEFAULT_BIN), '--config', str(write_yourai_config(tmp, port))]
        if app == 'opencode':
            return ['opencode'], {'OPENCODE_CONFIG': str(write_opencode_config(tmp, port))}
        home = Path(tmp) / 'codex-home'
        home.mkdir()
        (home / 'config.toml').write_text(f'''model = "scroll"
model_provider = "mock"

[model_providers.mock]
name = "mock"
base_url = "http://127.0.0.1:{port}/v1"
wire_api = "responses"
''')
        return ['codex'], {'CODEX_HOME': str(home)}

    return pane


def steps_for(app):
    if app == 'yourai':
        return [(b'New session', b'render a long transcript\r'), (b'results.', b'')]
    if app == 'opencode':
        return [(b'Ask anything', b'render a long transcript\r'), (b'results.', b'')]
    # Codex needs the trust dialog and a type-then-wait-then-Enter submit.
    return [(b'Trust and continue', b'\r'),
            (b'Ask Codex to do anything',
             [(0.2, b'render a long transcript'), (1.2, b'\r')]),
            (b'RAW:readable code and results', b'')]


def run(app, hz=120.0, seconds=2.0, cols=160, rows=48):
    screen = Screen(cols, rows)
    with MuxProbe(pane_for(app), mouse=True, cols=cols, rows=rows,
                  passthrough=('YOURAI_TUI_SYNC', 'YOURAI_TUI_SCROLL',
                               'OPENCODE_CONFIG', 'CODEX_HOME'),
                  on_data=screen.feed) as p:
        # NOTE: access tap.chunks by attribute — reset() replaces the list,
        # so a local alias bound before reset would go stale.
        buf = p.tap.buf
        steps = steps_for(app)
        step_i, attempts, sent_at = 0, [0] * (len(steps) + 1), [None] * (len(steps) + 1)

        def present(trigger):
            # b'RAW:...' triggers match the byte history (scrollback).
            return (trigger[4:] in buf) if trigger.startswith(b'RAW:') \
                else (trigger in screen.text().encode())

        deadline = time.monotonic() + 90
        while time.monotonic() < deadline and step_i < len(steps):
            trigger, keys = steps[step_i][0], steps[step_i][1]
            if present(trigger):
                if not keys:
                    step_i += 1  # wait-only step
                    continue
                if sent_at[step_i] is None or time.monotonic() - sent_at[step_i] > 3.0:
                    sent_at[step_i] = time.monotonic()
                    attempts[step_i] += 1
                    if attempts[step_i] > 8:
                        print(f'=== {app} SCREEN AT GIVE-UP (step {step_i}) ===')
                        print(screen.text()[-1500:])
                        raise AssertionError(f'{app}: gave up waiting after {trigger!r}')
                    # Keys may be bytes or a list of (delay, bytes) phases.
                    for delay, payload in keys if isinstance(keys, list) else [(0.0, keys)]:
                        if delay:
                            time.sleep(delay)
                        print(f'--- {app} step {step_i} send {payload!r}')
                        p.send(payload)
                    time.sleep(1.0)
            elif sent_at[step_i] is not None:
                # Advance when this step's trigger is gone OR the next
                # step's trigger arrived (placeholders return after submit).
                nxt = steps[step_i + 1][0] if step_i + 1 < len(steps) else None
                step_i += 2 if (nxt is not None and present(nxt)) else 1
            else:
                time.sleep(0.1)
        if step_i < len(steps):
            print(f'=== {app} SCREEN AT FAILURE ===')
            print(screen.text()[-1500:])
            raise AssertionError(
                f'{app}: transcript never rendered (last trigger {steps[step_i][0]!r})')
        time.sleep(1.0)

        # Reset measurement, then inject a wheel burst like a trackpad flick.
        p.reset()
        sent = p.burst(hz, seconds)
        last_event = time.monotonic()
        # Wait for output to go quiet for 200ms (bounded by 3s).
        quiet = time.monotonic() + 3.0
        while time.monotonic() < quiet:
            recent = [t for t, _ in p.tap.chunks if t > last_event - 0.05]
            if recent and time.monotonic() - max(recent) > 0.2:
                break
            time.sleep(0.02)
        during = [(t, n) for t, n in p.tap.chunks if t <= last_event]
        after = [(t, n) for t, n in p.tap.chunks if t > last_event]
        kbs = sum(n for _, n in during) / seconds / 1024
        tail_ms = (after[-1][0] - last_event) * 1000 if after else 0.0
        total_kb = sum(n for _, n in p.tap.chunks) / 1024
        per_event = sum(n for _, n in p.tap.chunks) / max(1, sent) / 1024
        # Update bursts: chunks separated by >30ms count as separate paints.
        paints = 0
        last = None
        for t, _ in during:
            if last is None or t - last > 0.03:
                paints += 1
            last = t
        print(f'{app}: events={sent} -> client {kbs:.0f}KB/s, {per_event:.2f}KB/event, '
              f'paints={paints} ({paints / seconds:.0f}/s), tail={tail_ms:.0f}ms, total={total_kb:.0f}KB')


if __name__ == '__main__':
    apps = sys.argv[1:] or APPS
    for app in apps:
        if app not in APPS:
            print(f'unknown app {app}; choices: {APPS}')
            sys.exit(1)
        run(app)

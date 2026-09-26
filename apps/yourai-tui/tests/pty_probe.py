"""Interactive PTY driver for probing CLI TUI apps; dumps screen as plain text."""
import fcntl
import os
import pty
import re
import select
import struct
import subprocess
import sys
import termios
import time


def run_pty(cmd, env=None, cwd=None, cols=160, rows=48):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', rows, cols, 0, 0))
    child = subprocess.Popen(cmd, cwd=cwd, stdin=slave, stdout=slave, stderr=slave,
                             env=env, close_fds=True)
    os.close(slave)
    return child, master


class Screen:
    """Very small VT100-ish grid to see what the app paints."""

    def __init__(self, cols, rows):
        self.grid = [[' '] * cols for _ in range(rows)]
        self.x = self.y = 0
        self.cols, self.rows = cols, rows

    def feed(self, data):
        i = 0
        while i < len(data):
            b = data[i]
            if b == 0x1b:
                if i + 1 < len(data) and data[i + 1] == ord('['):
                    j = i + 2
                    while j < len(data) and (chr(data[j]).isdigit() or data[j] in (ord(';'), ord('?'))):
                        j += 1
                    if j < len(data):
                        params = data[i + 2:j].decode(errors='ignore')
                        final = chr(data[j])
                        if final == 'H':
                            p = [int(x) if x else 1 for x in params.split(';')] if params else [1, 1]
                            self.y, self.x = p[0] - 1, p[-1] - 1
                        elif final == 'J' and params in ('2', ''):
                            self.grid = [[' '] * self.cols for _ in range(self.rows)]
                        i = j + 1
                        continue
                i += 2
                continue
            if b == 0x0a:
                self.y = min(self.y + 1, self.rows - 1)
                i += 1
                continue
            if b == 0x0d:
                self.x = 0
                i += 1
                continue
            if 0x20 <= b < 0x7f and self.y < self.rows and self.x < self.cols:
                self.grid[self.y][self.x] = chr(b)
                self.x += 1
            i += 1

    def text(self):
        return '\n'.join(''.join(row).rstrip() for row in self.grid).rstrip('\n')


def probe(cmd, env=None, cwd=None, steps=None, cols=160, rows=48):
    child, master = run_pty(cmd, env, cwd, cols, rows)
    screen = Screen(cols, rows)
    buf = bytearray()
    steps = steps or []
    step_i = 0
    attempts = [0] * (len(steps) + 1)
    sent_at = [None] * (len(steps) + 1)
    deadline = time.monotonic() + 45
    try:
        while time.monotonic() < deadline:
            r, _, _ = select.select([master], [], [], 0.05)
            if r:
                try:
                    data = os.read(master, 65536)
                except OSError:
                    break
                if not data:
                    break
                buf.extend(data)
                screen.feed(data)
                # Answer terminal capability queries like a real emulator.
                if b'\x1b[6n' in data:
                    os.write(master, b'\x1b[1;1R')
                if b'\x1b[c' in data:
                    os.write(master, b'\x1b[?62;1;2;6;9;15;22c')
                if b'\x1b[>c' in data:
                    os.write(master, b'\x1b[>0;280;0c')
                if b'\x1b[?u' in data:
                    os.write(master, b'\x1b[?0u')
            # Step machine: triggers match the CURRENT screen (rendered grid),
            # not the byte history, so consumed prompts release the step.
            if step_i < len(steps):
                trigger, keys = steps[step_i][0], steps[step_i][1]
                if trigger in screen.text().encode():
                    last_sent = sent_at[step_i]
                    if last_sent is None or time.monotonic() - last_sent > 2.0:
                        sent_at[step_i] = time.monotonic()
                        attempts[step_i] += 1
                        if attempts[step_i] > 6:
                            print(f'--- step {step_i} gave up on {trigger!r}')
                            step_i += 1
                            continue
                        print(f'--- step {step_i} sending {keys!r} (try {attempts[step_i]})')
                        os.write(master, keys)
                        time.sleep(1.0)
                    else:
                        time.sleep(0.3)
                else:
                    # Not on screen: either not yet appeared (before first
                    # send) or consumed (after send) — advance on the latter.
                    if sent_at[step_i] is not None:
                        step_i += 1
            if child.poll() is not None:
                break
        print('=== SCREEN ===')
        print(screen.text())
        print('=== EXIT', child.poll(), '===')
        out = os.environ.get('PTY_PROBE_DUMP')
        if out:
            open(out, 'wb').write(bytes(buf))
            print(f'raw bytes -> {out}')
    finally:
        if child.poll() is None:
            child.kill()
            child.wait()
        os.close(master)
    return bytes(buf)


if __name__ == '__main__':
    import tempfile

    from smoke_support import start_model_server, write_opencode_config

    app = sys.argv[1]
    tmp = tempfile.mkdtemp()
    server = start_model_server(
        ''.join(f'## Section {i}\n\nParagraph {i}: readable code and results.\n\n'
                for i in range(500)))
    port = server.server_port
    env = {k: v for k, v in os.environ.items() if k not in ('TMUX', 'TMUX_PANE', 'TERM_PROGRAM')}
    env['TERM'] = 'xterm-256color'
    if app == 'codex':
        home = os.path.join(tmp, 'codex-home')
        os.makedirs(home)
        with open(os.path.join(home, 'config.toml'), 'w') as f:
            f.write(f'''model = "scroll"
model_provider = "mock"

[model_providers.mock]
name = "mock"
base_url = "http://127.0.0.1:{port}/v1"
wire_api = "responses"

[projects."{tmp}"]
trust_level = "trusted"
''')
        env['CODEX_HOME'] = home
        cmd = ['codex']
        steps = [
            (b'Trust and continue', b'\r'),
            (b'Ask Codex to do anything', b'render a long transcript\r'),
            (b'results.', b''),
        ]
    elif app == 'opencode':
        cfg = write_opencode_config(tmp, port)
        env['OPENCODE_CONFIG'] = str(cfg)
        cmd = ['opencode']
        steps = []
    else:
        print('usage: pty_probe.py codex|opencode')
        sys.exit(1)
    print(f'tmp={tmp} port={port}')
    probe(cmd, env=env, cwd=tmp, steps=steps)
    server.shutdown()

"""Interactive PTY driver for probing CLI TUI apps; dumps screen as plain text."""
import codecs
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
import unicodedata


def run_pty(cmd, env=None, cwd=None, cols=160, rows=48):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', rows, cols, 0, 0))
    child = subprocess.Popen(cmd, cwd=cwd, stdin=slave, stdout=slave, stderr=slave,
                             env=env, close_fds=True)
    os.close(slave)
    return child, master


class Screen:
    """Small VT grid for current-screen assertions, including sparse updates."""

    def __init__(self, cols, rows):
        self.grid = [[' '] * cols for _ in range(rows)]
        self.x = self.y = 0
        self.cols, self.rows = cols, rows
        self.top, self.bottom = 0, rows - 1
        self.decoder = codecs.getincrementaldecoder('utf-8')('replace')
        self.pending = ''

    def feed(self, data):
        text = self.pending + self.decoder.decode(data)
        i = 0
        while i < len(text):
            ch = text[i]
            if ch == '\x1b':
                if i + 1 == len(text):
                    break
                if text[i + 1] == '[':
                    j = i + 2
                    while j < len(text) and not ('@' <= text[j] <= '~'):
                        j += 1
                    if j == len(text):
                        break
                    params, final = text[i + 2:j], text[j]
                    values = [int(v) if v.isdigit() else 0 for v in params.lstrip('?').split(';')]
                    amount = values[0] or 1
                    if final in ('H', 'f'):
                        self.y = amount - 1
                        self.x = (values[1] or 1) - 1 if len(values) > 1 else 0
                    elif final == 'G':
                        self.x = amount - 1
                    elif final == 'd':
                        self.y = amount - 1
                    elif final in ('A', 'B', 'C', 'D'):
                        if final == 'A':
                            self.y = max(0, self.y - amount)
                        elif final == 'B':
                            self.y = min(self.rows - 1, self.y + amount)
                        elif final == 'C':
                            self.x = min(self.cols - 1, self.x + amount)
                        else:
                            self.x = max(0, self.x - amount)
                    elif final == 'J':
                        mode = values[0]
                        for y in range(self.rows):
                            for x in range(self.cols):
                                if mode == 2 or (mode == 0 and (y, x) >= (self.y, self.x)) or (mode == 1 and (y, x) <= (self.y, self.x)):
                                    self.grid[y][x] = ' '
                    elif final == 'K' and 0 <= self.y < self.rows:
                        lo = 0 if values[0] in (1, 2) else self.x
                        hi = self.cols if values[0] in (0, 2) else min(self.cols, self.x + 1)
                        self.grid[self.y][max(0, lo):hi] = [' '] * (hi - max(0, lo))
                    elif final == 'r':
                        self.top = max(0, amount - 1)
                        self.bottom = min(self.rows - 1, (values[1] or self.rows) - 1) if len(values) > 1 else self.rows - 1
                        self.x = self.y = 0
                    elif final in ('S', 'T'):
                        for _ in range(min(amount, self.bottom - self.top + 1)):
                            at = self.top if final == 'S' else self.bottom
                            self.grid.pop(at)
                            self.grid.insert(self.bottom if final == 'S' else self.top, [' '] * self.cols)
                    i = j + 1
                    continue
                if text[i + 1] == ']':
                    # OSC title/link sequences can span read chunks too.
                    end = re.search('\x07|\x1b\\\\', text[i + 2:])
                    if end is None:
                        break
                    i += 2 + end.end()
                    continue
                i += 2
                continue
            if ch == '\n':
                self.y = min(self.y + 1, self.rows - 1)
            elif ch == '\r':
                self.x = 0
            elif ch == '\b':
                self.x = max(0, self.x - 1)
            elif ch >= ' ' and ch != '\x7f':
                width = 0 if unicodedata.combining(ch) else 2 if unicodedata.east_asian_width(ch) in ('W', 'F') else 1
                if 0 <= self.y < self.rows and 0 <= self.x < self.cols and width:
                    self.grid[self.y][self.x] = ch
                    if width == 2 and self.x + 1 < self.cols:
                        self.grid[self.y][self.x + 1] = ''
                self.x += width
            i += 1
        self.pending = text[i:]

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

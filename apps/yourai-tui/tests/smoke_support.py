"""Shared harness for the TUI probes: mock model server + PTY driver.

The mock streams one long assistant transcript over both the chat-completions
and the codex-flavored Responses wire format, so the same server drives
yourai-tui, opencode and codex. `Probe` runs the binary on a PTY, answers its
terminal capability queries and counts the frames/bytes it writes back;
`MuxProbe` does the same through a real tmux server (what a remote client
would receive). Keep the default line protocols stable: the performance log
in docs/tui-performance.md quotes these probes' output formats.
"""
import fcntl
import http.server
import json
import os
from pathlib import Path
import pty
import select
import struct
import subprocess
import tempfile
import termios
import threading
import time

DEFAULT_BIN = Path(__file__).resolve().parents[3] / 'target/debug/yourai-tui'

WHEEL_UP = b'\x1b[<64;10;10M'
WHEEL_DOWN = b'\x1b[<65;10;10M'


def long_text(sections=500):
    """Markdown transcript: headings and prose, the standard scroll payload."""
    return ''.join(
        f'## Section {i}\n\nParagraph {i}: readable code and results.\n\n'
        for i in range(sections)
    )


def code_text(sections=70):
    """Markdown transcript with rust code blocks: dense SGR, paints like code."""
    code = '''fn process_%d(input: &str) -> Result<Value> {
    let parsed: Vec<Token> = lexer::tokenize(input)?;
    let mut ctx = Context::new(Cfg::default());
    for (i, tok) in parsed.iter().enumerate() {
        ctx.push(Key::Index(i), tok.render()?);
    }
    Ok(ctx.finalize())
}'''
    return ''.join(
        f'## Section {i}\n\n```rust\n' + (code % i) + '\n```\n\n'
        f'Paragraph %d: readable code and results.\n\n' % i
        for i in range(sections)
    )


class _Model(http.server.BaseHTTPRequestHandler):
    """Streams `server.text` as one assistant message, both wire formats."""

    def log_message(self, *args):
        pass

    def do_POST(self):
        self.rfile.read(int(self.headers['Content-Length']))
        text = self.server.text
        if self.path.endswith('/responses'):
            # Codex Responses SSE: item added -> delta -> item done -> completed.
            # No [DONE] marker — that is chat-completions only.
            events = [
                {'type': 'response.output_item.added', 'output_index': 0,
                 'item': {'type': 'message', 'id': 'm1', 'role': 'assistant', 'content': []}},
                {'type': 'response.output_text.delta', 'item_id': 'm1', 'output_index': 0,
                 'content_index': 0, 'delta': text},
                {'type': 'response.output_item.done', 'output_index': 0,
                 'item': {'type': 'message', 'id': 'm1', 'role': 'assistant',
                          'content': [{'type': 'output_text', 'text': text}]}},
                {'type': 'response.completed',
                 'response': {'id': 'r1', 'end_turn': True, 'usage_metadata': None,
                              'usage': {'input_tokens': 1, 'output_tokens': 1, 'total_tokens': 2,
                                        'input_tokens_details': {'cached_tokens': 0, 'cache_write_tokens': 0},
                                        'output_tokens_details': {'reasoning_tokens': 0}}}},
            ]
            payload = ''.join('data: ' + json.dumps(e) + '\n\n' for e in events).encode()
        else:
            events = [
                {'choices': [{'index': 0, 'delta': {'content': text}, 'finish_reason': None}]},
                {'choices': [{'index': 0, 'delta': {}, 'finish_reason': 'stop'}]},
            ]
            payload = (''.join('data: ' + json.dumps(e) + '\n\n' for e in events)
                       + 'data: [DONE]\n\n').encode()
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Content-Length', str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


def start_model_server(text):
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), _Model)
    server.text = text
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def write_yourai_config(tmp, port):
    """Point a scratch yourai-tui config at the mock server."""
    config = Path(tmp) / 'config.json'
    config.write_text(json.dumps({
        'model': 'mock/scroll', 'extensions': False,
        'provider': {'mock': {'npm': '@ai-sdk/openai-compatible', 'options': {
            'baseURL': f'http://127.0.0.1:{port}/v1', 'apiKey': 'x'}, 'models': {}}}
    }))
    return config


def write_opencode_config(tmp, port):
    cfg = Path(tmp) / 'oc.json'
    cfg.write_text(json.dumps({'model': 'mock/scroll', 'provider': {'mock': {
        'npm': '@ai-sdk/openai-compatible',
        'options': {'baseURL': f'http://127.0.0.1:{port}/v1', 'apiKey': 'x'},
        'models': {'scroll': {'name': 'Scroll'}}}}}))
    return cfg


def _strip_env(extra=None):
    env = {k: v for k, v in os.environ.items()
           if k not in ('TMUX', 'TMUX_PANE', 'TERM_PROGRAM')}
    if extra:
        env.update(extra)
    return env


def _answer_queries(master, data):
    """Reply to terminal capability queries like a real emulator."""
    for needle, reply in ((b'\x1b[6n', b'\x1b[1;1R'),
                          (b'\x1b[c', b'\x1b[?62;1;2;6;9;15;22c'),
                          (b'\x1b[>c', b'\x1b[>0;280;0c'),
                          (b'\x1b[?u', b'\x1b[?0u')):
        if needle in data:
            os.write(master, reply)


class _Tap:
    """Reader thread: feeds `buf`, counts bytes, frames and chunk arrivals."""

    def __init__(self, master, on_data=None):
        self.master = master
        self.on_data = on_data    # optional raw chunk sink (e.g. a VT screen)
        self.buf = bytearray()
        self.bytes = 0
        self.frames = 0
        self.frame_times = []
        self.chunks = []          # (monotonic time, chunk size)
        self.stop = False
        self.thread = threading.Thread(target=self._run, daemon=True)

    def _run(self):
        while not self.stop:
            try:
                if not select.select([self.master], [], [], 0.005)[0]:
                    continue
                data = os.read(self.master, 65536)
            except OSError:
                break
            if not data:
                break
            now = time.monotonic()
            self.buf.extend(data)
            self.chunks.append((now, len(data)))
            self.bytes += len(data)
            frames = data.count(b'\x1b[?2026l')
            self.frames += frames
            self.frame_times.extend([now] * frames)
            if self.on_data:
                self.on_data(data)
            _answer_queries(self.master, data)

    def start(self):
        self.thread.start()
        return self

    def wait_for(self, needle, timeout=15):
        end = time.monotonic() + timeout
        while needle not in self.buf:
            if time.monotonic() >= end:
                raise AssertionError(
                    f'timeout waiting for {needle!r}; tail={bytes(self.buf[-800:])!r}')
            time.sleep(0.01)

    def reset(self):
        """Zero the counters; byte history (`buf`) keeps accumulating."""
        self.bytes = 0
        self.frames = 0
        self.frame_times = []
        self.chunks = []

    def quiet(self, idle=0.2, timeout=3.0, since=None):
        """Wait until output stops flowing; returns the settle time in ms."""
        if since is None:
            since = time.monotonic()
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            last = self.chunks[-1][0] if self.chunks else None
            if last is not None and time.monotonic() - last > idle:
                break
            time.sleep(0.02)
        last = self.chunks[-1][0] if self.chunks else since
        return max(0.0, (last - since) * 1000)

    def kill(self):
        self.stop = True


def wait_exit(child, master, timeout=10):
    """Drain remaining output until the child exits after Ctrl-Q."""
    deadline = time.monotonic() + timeout
    while child.poll() is None:
        if time.monotonic() >= deadline:
            raise AssertionError('TUI did not exit after Ctrl-Q')
        if select.select([master], [], [], 0.05)[0]:
            data = os.read(master, 65536)
            if b'\x1b[6n' in data:
                os.write(master, b'\x1b[1;1R')
    return child.returncode


class Probe:
    """A yourai-tui binary under test on a PTY against the mock model.

    Counters assume the 2026 sync wrapper (frames are delimited by
    `\\x1b[?2026l`), so probes that count frames force
    `YOURAI_TUI_SYNC=on` — the wrapper only costs 16 bytes per frame.
    """

    def __init__(self, binary=None, *, text=None, term='xterm-256color',
                 cols=160, rows=48, cwd=None, env=None):
        self.binary = Path(binary or DEFAULT_BIN).resolve()
        self.text = text if text is not None else long_text()
        self.term = term
        self.cols, self.rows = cols, rows
        self.tmp = None
        self.cwd = cwd
        self.env = env or {}

    def __enter__(self):
        self.tmp = tempfile.TemporaryDirectory()
        tmp = self.tmp.name
        self.server = start_model_server(self.text)
        self.config = write_yourai_config(tmp, self.server.server_port)
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ,
                    struct.pack('HHHH', self.rows, self.cols, 0, 0))
        env = _strip_env(self.env)
        env['TERM'] = self.term
        self.child = subprocess.Popen(
            [str(self.binary), '--config', str(self.config)],
            cwd=self.cwd or tmp, stdin=slave, stdout=slave, stderr=slave, env=env)
        os.close(slave)
        self.master = master
        self.tap = _Tap(master).start()
        return self

    def __exit__(self, *exc):
        self.tap.kill()
        if self.child.poll() is None:
            self.child.kill()
            self.child.wait()
        os.close(self.master)
        self.server.shutdown()
        self.server.server_close()
        self.tmp.cleanup()
        return False

    # -- driving ------------------------------------------------------------
    def send(self, data):
        os.write(self.master, data)

    def wait(self, needle, timeout=15):
        self.tap.wait_for(needle, timeout)

    def reset(self):
        self.tap.reset()

    def burst(self, hz, seconds):
        """Inject wheel-up events at `hz` like a trackpad flick; returns count."""
        period = 1.0 / hz
        sent = 0
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            self.send(WHEEL_UP)
            sent += 1
            time.sleep(period)
        return sent


class MuxProbe:
    """An app under test behind a real tmux server, seen from the client PTY.

    `pane(tmp, port)` builds the pane command once the scratch dir and mock
    server exist. `mouse=True` adds the UURemote-style copy-mode wheel
    bindings. Extra `passthrough` env names are forwarded into the pane via
    `-e` (tmux starts panes with its own scrubbed environment). `on_data`
    receives every client-side chunk (e.g. to feed a VT screen parser).
    """

    def __init__(self, pane, *, text=None, mouse=False, cols=160, rows=48,
                 cwd=None, env=None, passthrough=(), on_data=None):
        self.pane = pane
        self.text = text if text is not None else long_text()
        self.mouse = mouse
        self.cols, self.rows = cols, rows
        self.tmp = None
        self.cwd = cwd
        self.env = env or {}
        self.passthrough = list(passthrough)
        self.on_data = on_data

    def __enter__(self):
        self.tmp = tempfile.TemporaryDirectory()
        tmp = self.tmp.name
        self.server = start_model_server(self.text)
        conf = Path(tmp) / 'mux.conf'
        conf.write_text(
            'set -g status off\n'
            'set -g exit-empty on\n'
            'set -g escape-time 0\n'
            'set -g default-terminal "tmux-256color"\n'
            + ('set -g mouse on\n'
               'bind -Tcopy-mode WheelUpPane { select-pane; send -N1 -X scroll-up }\n'
               'bind -Tcopy-mode WheelDownPane { select-pane; send -N1 -X scroll-down }\n'
               'bind -Tcopy-mode-vi WheelUpPane { select-pane; send -N1 -X scroll-up }\n'
               'bind -Tcopy-mode-vi WheelDownPane { select-pane; send -N1 -X scroll-down }\n'
               if self.mouse else '')
        )
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ,
                    struct.pack('HHHH', self.rows, self.cols, 0, 0))
        env = _strip_env(self.env)
        env['TERM'] = 'xterm-256color'
        # pane(tmp, port) -> command list, or (command list, extra client env)
        # that also reaches the pane via tmux's inherited environment.
        built = self.pane(tmp, self.server.server_port)
        if isinstance(built, tuple):
            pane_cmd, extra_env = built
            env.update(extra_env)
        else:
            pane_cmd = built
        cmd = ['tmux', '-f', str(conf), 'new-session', '-A',
               '-x', str(self.cols), '-y', str(self.rows),
               '-s', f'probe{os.getpid()}']
        for name in self.passthrough:
            cmd += ['-e', f'{name}={env.get(name, "")}']
        cmd += ['--'] + pane_cmd
        self.child = subprocess.Popen(cmd, cwd=self.cwd or tmp,
                                      stdin=slave, stdout=slave, stderr=slave, env=env)
        os.close(slave)
        self.master = master
        self.tap = _Tap(master, self.on_data).start()
        return self

    def __exit__(self, *exc):
        self.tap.kill()
        subprocess.run(['tmux', 'kill-server'], env=_strip_env(), capture_output=True)
        try:
            self.child.wait(timeout=3)
        except subprocess.TimeoutExpired:
            self.child.kill()
        try:
            os.close(self.master)
        except OSError:
            pass
        self.server.shutdown()
        self.server.server_close()
        self.tmp.cleanup()
        return False

    # -- driving ------------------------------------------------------------
    def send(self, data):
        os.write(self.master, data)

    def wait(self, needle, timeout=20):
        self.tap.wait_for(needle, timeout)

    def reset(self):
        self.tap.reset()

    def burst(self, hz, seconds):
        period = 1.0 / hz
        sent = 0
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            self.send(WHEEL_UP)
            sent += 1
            time.sleep(period)
        return sent

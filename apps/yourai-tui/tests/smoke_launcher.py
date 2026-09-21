"""Launcher smoke test: bare `--resume` opens the session picker."""
import fcntl
import http.server
import json
import os
from pathlib import Path
import pty
import re
import select
import struct
import sys
import subprocess
import tempfile
import termios
import threading
import time


class Model(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        if not body.get('stream'):
            payload = json.dumps({'id': 'c', 'object': 'chat.completion', 'model': 'm',
                                  'choices': [{'index': 0, 'message': {'role': 'assistant', 'content': 'OK'}, 'finish_reason': 'stop'}],
                                  'usage': {'prompt_tokens': 10, 'completion_tokens': 2, 'total_tokens': 12}}).encode()
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return
        events = [
            {'choices': [{'index': 0, 'delta': {'content': 'LAUNCHER_OK'}, 'finish_reason': None}]},
            {'choices': [{'index': 0, 'delta': {}, 'finish_reason': 'stop'}],
             'usage': {'prompt_tokens': 10, 'completion_tokens': 2, 'total_tokens': 12}},
        ]
        payload = ''.join('data: ' + json.dumps(e) + '\n\n' for e in events) + 'data: [DONE]\n\n'
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Content-Length', str(len(payload.encode())))
        self.end_headers()
        self.wfile.write(payload.encode())


with tempfile.TemporaryDirectory() as tmp:
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Model)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    config = Path(tmp) / 'config.json'
    config.write_text(json.dumps({
        "model": "mock/m", "extensions": False,
        "provider": {"mock": {
            "npm": "@ai-sdk/openai-compatible",
            "options": {"baseURL": f"http://127.0.0.1:{server.server_port}/v1", "apiKey": "x"},
            "models": {"m": {"id": "m", "limit": {"context": 16000, "output": 4096}}}
        }},
        "context": {"keep_recent_tokens": 0, "summary_min_savings": 1}
    }))
    # Seed an existing session by running the TUI once briefly.
    bin_path = Path(__file__).resolve().parents[3] / 'target/debug/yourai-tui'
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 35, 120, 0, 0))
    child = subprocess.Popen([str(bin_path), '--config', str(config)], stdin=slave, stdout=slave, stderr=slave, env=os.environ)
    captured = bytearray()

    def wait_for(needle, timeout=10):
        end = time.monotonic() + timeout
        while needle not in re.sub(rb'\x1b\[[0-?]*[ -/]*[@-~]', b'', captured):
            if time.monotonic() > end:
                raise AssertionError(f'Missing {needle!r}; output: ' + re.sub(rb'\x1b\[[0-?]*[ -/]*[@-~]', b'', captured).decode(errors='replace')[-2000:])
            if select.select([master], [], [], 0.1)[0]:
                data = os.read(master, 65536)
                captured.extend(data)
                if b'\x1b[6n' in data:
                    os.write(master, b'\x1b[1;1R')

    try:
        wait_for(b'Untitled session')
        os.write(master, b'seed message\r')
        wait_for(b'LAUNCHER_OK')
        os.write(master, b'\x11')  # Ctrl-Q
        child.wait(timeout=10)
    finally:
        if child.poll() is None:
            child.kill()
            child.wait()
    os.close(master)
    os.close(slave)

    # Now run with bare `--resume`; the launcher should show the picker with our session.
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 35, 120, 0, 0))
    original = termios.tcgetattr(slave)
    child = subprocess.Popen([str(bin_path), '--config', str(config), '--resume'], stdin=slave, stdout=slave, stderr=slave, env=os.environ)
    captured = bytearray()
    try:
        wait_for(b'Sessions')
        # The seeded session title should appear in the list.
        wait_for(b'seed message')
        # Resume it with Enter.
        os.write(master, b'\r')
        wait_for(b'LAUNCHER_OK')
        os.write(master, b'\x11')  # Ctrl-Q
        child.wait(timeout=10)
        assert child.returncode == 0
        assert termios.tcgetattr(slave) == original, 'terminal mode was not restored'
        print('PASS: launcher --resume shows picker, Enter resumes, terminal mode restored')
    finally:
        if child.poll() is None:
            child.kill()
            child.wait()
        os.close(master)
        os.close(slave)
        server.shutdown()

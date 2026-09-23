from smoke_support import wait_exit
"""Smoke test for /models switching: verify the second request hits the switched model."""
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

requests = []


class Model(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        requests.append((self.path, body))
        if not body.get('stream'):
            payload = json.dumps({'id': 'c', 'object': 'chat.completion', 'model': body.get('model', '?'),
                                  'choices': [{'index': 0, 'message': {'role': 'assistant', 'content': 'OK'}, 'finish_reason': 'stop'}],
                                  'usage': {'prompt_tokens': 10, 'completion_tokens': 2, 'total_tokens': 12}}).encode()
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return
        events = [
            {'choices': [{'index': 0, 'delta': {'content': 'MODELS_OK'}, 'finish_reason': None}]},
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
        "model": "mock/smoke", "extensions": False,
        "provider": {"mock": {
            "npm": "@ai-sdk/openai-compatible",
            "options": {"baseURL": f"http://127.0.0.1:{server.server_port}/v1", "apiKey": "x"},
            "models": {
                "smoke": {"id": "smoke-model", "limit": {"context": 16000, "output": 4096}},
                "alt": {"id": "alt-model", "limit": {"context": 16000, "output": 4096}}
            }
        }},
        "context": {"keep_recent_tokens": 0, "summary_min_savings": 1}
    }))
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 35, 120, 0, 0))
    original = termios.tcgetattr(slave)
    binary = Path(__file__).resolve().parents[3] / 'target/debug/yourai-tui'
    child = subprocess.Popen([str(binary), '--config', str(config)], stdin=slave, stdout=slave, stderr=slave, env=os.environ)
    captured = bytearray()

    def wait_for(needle, timeout=10):
        end = time.monotonic() + timeout
        while needle not in re.sub(rb'\x1b\[[0-?]*[ -/]*[@-~]', b'', captured):
            if time.monotonic() > end:
                raise AssertionError(f'Missing {needle!r}; requests: {len(requests)}; output: ' + re.sub(rb'\x1b\[[0-?]*[ -/]*[@-~]', b'', captured).decode(errors='replace')[-2000:])
            if select.select([master], [], [], 0.1)[0]:
                data = os.read(master, 65536)
                captured.extend(data)
                if b'\x1b[6n' in data:
                    os.write(master, b'\x1b[1;1R')

    try:
        wait_for(b'New session')
        # Send first message with default model (smoke-model).
        os.write(master, b'first message\r')
        wait_for(b'MODELS_OK')
        assert len(requests) >= 1, 'first request missing'
        assert requests[0][1]['model'] == 'smoke-model', f'expected smoke-model, got {requests[0][1]["model"]}'
        # Switch to alt model via direct command.
        os.write(master, b'/models mock/alt\r')
        time.sleep(0.5)
        # Verify the footer shows the new model label.
        wait_for(b'mock/alt')
        # Clear captured so the second MODELS_OK wait doesn't match the first.
        captured.clear()
        # Send second message; should use alt-model.
        os.write(master, b'second message\r')
        wait_for(b'MODELS_OK')
        # Find the second streaming request (skip any non-streaming ones).
        stream_reqs = [r for r in requests if r[1].get('stream')]
        assert len(stream_reqs) >= 2, f'expected 2 stream requests, got {len(stream_reqs)}'
        assert stream_reqs[1][1]['model'] == 'alt-model', f'expected alt-model, got {stream_reqs[1][1]["model"]}'
        os.write(master, b'\x11')  # Ctrl-Q
        wait_exit(child, master)
        assert child.returncode == 0
        assert termios.tcgetattr(slave) == original, 'terminal mode was not restored'
        print('PASS: /models switch — first request smoke-model, second request alt-model')
    finally:
        if child.poll() is None:
            child.kill()
            child.wait()
        os.close(master)
        os.close(slave)
        server.shutdown()

# Keep modal/session regressions in the existing CI smoke entry point.
subprocess.run([sys.executable, str(Path(__file__).with_name('smoke_ui.py'))], check=True)

from smoke_support import DEFAULT_BIN, wait_completed, wait_exit
from pty_probe import Screen
"""Smoke test for /models switching: verify the second request hits the switched model."""
import fcntl
import http.server
import json
import os
from pathlib import Path
import pty
import re
import select
import sqlite3
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
                "third": {"id": "third-model", "limit": {"context": 16000, "output": 4096}},
                "alt": {"id": "alt-model", "limit": {"context": 262144, "output": 65536},
                        "options": {"reasoningEffort": "low", "maxOutputTokens": 49152}, "variants": {"inherit": {}}}
            }
        }},
        "context": {"keep_recent_tokens": 0, "summary_min_savings": 1}
    }))
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 35, 120, 0, 0))
    original = termios.tcgetattr(slave)
    binary = DEFAULT_BIN
    child = subprocess.Popen([str(binary), '--config', str(config)], stdin=slave, stdout=slave, stderr=slave, env=dict(os.environ, XDG_DATA_HOME=str(Path(tmp) / 'xdg-data')))
    captured = bytearray()
    screen = Screen(120, 35)

    def read_output(timeout):
        if select.select([master], [], [], timeout)[0]:
            data = os.read(master, 65536)
            captured.extend(data)
            screen.feed(data)
            if b'\x1b[6n' in data:
                os.write(master, b'\x1b[1;1R')
            return True
        return False

    def visible(needle):
        text = screen.text()
        if needle.startswith(b'Model switched'):
            notices = [line.strip().lstrip('· ') for line in text.splitlines() if 'Model switched' in line]
            return bool(notices) and notices[-1] == needle.decode()
        return needle in text.encode()

    def wait_for(needle, timeout=10, request_count=0, current=False):
        end = time.monotonic() + timeout
        while len(requests) < request_count or not (visible(needle) if current else needle in re.sub(rb'\x1b\[[0-?]*[ -/]*[@-~]', b'', captured)):
            if time.monotonic() > end:
                with sqlite3.connect(Path(tmp) / 'xdg-data/yourai/sessions/sessions.sqlite3') as db:
                    messages = db.execute('SELECT kind, status, content_json FROM messages ORDER BY rowid DESC LIMIT 3').fetchall()
                raise AssertionError(f'Missing {needle!r}; requests: {len(requests)}; exit: {child.poll()}; messages: {messages!r}; output: ' + re.sub(rb'\x1b\[[0-?]*[ -/]*[@-~]', b'', captured).decode(errors='replace')[-2000:])
            read_output(0.1)

    last_confirmation = None

    def wait_for_switch(needle):
        global last_confirmation
        assert needle != last_confirmation, 'unchanged targets are covered by App contract tests'
        wait_for(needle, current=True)
        last_confirmation = needle

    try:
        wait_for(b'New session')
        # Send first message with default model (smoke-model).
        os.write(master, b'first message\r')
        wait_for(b'MODELS_OK', request_count=1)
        assert len(requests) >= 1, 'first request missing'
        assert requests[0][1]['model'] == 'smoke-model', f'expected smoke-model, got {requests[0][1]["model"]}'
        # Switch to alt model via direct command; it is idle-guarded, so the
        # first turn must be fully settled first.
        wait_completed(Path(tmp) / 'xdg-data/yourai/sessions/sessions.sqlite3', 'main', 1)
        os.write(master, b'/models mock/alt\r')
        wait_for_switch('Model switched to mock/alt · thinking low'.encode())
        # Clear captured so the second MODELS_OK wait doesn't match the first.
        captured.clear()
        # Send second message; should use alt-model.
        os.write(master, b'second message\r')
        wait_for(b'MODELS_OK', request_count=2)
        # Find the second streaming request (skip any non-streaming ones).
        stream_reqs = [r for r in requests if r[1].get('stream')]
        assert len(stream_reqs) >= 2, f'expected 2 stream requests, got {len(stream_reqs)}'
        assert stream_reqs[1][1]['model'] == 'alt-model', f'expected alt-model, got {stream_reqs[1][1]["model"]}'
        assert stream_reqs[0][1].get('max_tokens', stream_reqs[0][1].get('max_completion_tokens')) == 4096
        assert stream_reqs[1][1].get('max_tokens', stream_reqs[1][1].get('max_completion_tokens')) == 49152
        assert stream_reqs[1][1].get('reasoning_effort') == 'low'
        # Set thinking effort via the /models picker's Enter drill-in.
        wait_completed(Path(tmp) / 'xdg-data/yourai/sessions/sessions.sqlite3', 'main', 2)
        os.write(master, b'/models\r')
        wait_for(b'Enter effort')
        os.write(master, b'\r')
        wait_for(b'config default')
        for _ in range(2):  # configured low -> medium -> high
            os.write(master, b'\x1b[B')
            time.sleep(0.05)
        os.write(master, b'\r')
        wait_for_switch('Model switched to mock/alt · thinking high'.encode())
        # The override rides on the next request as reasoning_effort.
        captured.clear()
        os.write(master, b'third message\r')
        wait_for(b'MODELS_OK', request_count=3)
        stream_reqs = [r for r in requests if r[1].get('stream')]
        assert len(stream_reqs) >= 3, f'expected 3 stream requests, got {len(stream_reqs)}'
        assert stream_reqs[2][1]['model'] == 'alt-model', f'expected alt-model, got {stream_reqs[2][1]["model"]}'
        assert stream_reqs[2][1].get('max_tokens', stream_reqs[2][1].get('max_completion_tokens')) == 49152
        assert stream_reqs[2][1].get('reasoning_effort') == 'high', \
            f'expected reasoning_effort high, got {stream_reqs[2][1].get("reasoning_effort")}'
        wait_completed(Path(tmp) / 'xdg-data/yourai/sessions/sessions.sqlite3', 'main', 3)
        captured.clear()
        os.write(master, b'/models\r')
        wait_for(b'Enter effort')
        os.write(master, b'\r')
        wait_for(b'config default')
        for _ in range(5):  # high -> config default
            os.write(master, b'\x1b[A')
            time.sleep(0.05)
        os.write(master, b'\r')
        wait_for_switch('Model switched to mock/alt · thinking low'.encode())
        captured.clear()
        os.write(master, b'fourth message\r')
        wait_for(b'MODELS_OK', request_count=4)
        stream_reqs = [r for r in requests if r[1].get('stream')]
        assert stream_reqs[3][1].get('reasoning_effort') == 'low', \
            'config default must restore the original configured effort'
        # Visiting and confirming an inherited level must not pin it to the
        # variant. Cancelling the sub-picker must not change either entry.
        wait_completed(Path(tmp) / 'xdg-data/yourai/sessions/sessions.sqlite3', 'main', 4)
        captured.clear()
        os.write(master, b'/models\r')
        wait_for(b'Enter effort')
        os.write(master, b'\x1b[B\r')  # alt default -> inherited variant
        wait_for(b'config default')
        os.write(master, b'\x1b[B')  # navigate, then cancel without applying
        captured.clear()
        os.write(master, b'\x1b')
        wait_for(b'Models')
        assert len(requests) == 4, 'opening/cancelling a picker must not call the model'
        os.write(master, b'\r')
        wait_for(b'config default')
        os.write(master, b'\r')
        wait_for_switch('Model switched to mock/alt · inherit · thinking low'.encode())

        # Change only the base model, then confirm the inherited variant again.
        captured.clear()
        os.write(master, b'/models\r')
        wait_for(b'Enter effort')
        os.write(master, b'\r')
        wait_for(b'config default')
        os.write(master, b'\x1b[B\x1b[B\r')  # low -> high
        wait_for_switch('Model switched to mock/alt · thinking high'.encode())
        captured.clear()
        os.write(master, b'/models\r')
        wait_for(b'Enter effort')
        os.write(master, b'\x1b[B\r')
        wait_for(b'config default')
        os.write(master, b'\r')
        wait_for_switch('Model switched to mock/alt · inherit · thinking high'.encode())
        captured.clear()
        os.write(master, b'fifth message\r')
        wait_for(b'MODELS_OK', request_count=5)
        assert requests[4][1].get('reasoning_effort') == 'high', \
            'an unchanged confirmation must preserve variant inheritance'

        # An intentional variant override remains independent of base changes.
        wait_completed(Path(tmp) / 'xdg-data/yourai/sessions/sessions.sqlite3', 'main', 5)
        captured.clear()
        os.write(master, b'/models\r')
        wait_for(b'Enter effort')
        os.write(master, b'\x1b[B\r')
        wait_for(b'config default')
        os.write(master, b'\x1b[A\x1b[A\r')  # high -> low
        wait_for_switch('Model switched to mock/alt · inherit · thinking low'.encode())
        captured.clear()
        os.write(master, b'/models\r')
        wait_for(b'Enter effort')
        os.write(master, b'\r')
        wait_for(b'config default')
        os.write(master, b'\x1b[A\r')  # base high -> medium
        wait_for_switch('Model switched to mock/alt · thinking medium'.encode())
        captured.clear()
        os.write(master, b'/models\r')
        wait_for(b'Enter effort')
        os.write(master, b'\x1b[B\r')
        wait_for(b'config default')
        os.write(master, b'\r')
        wait_for_switch('Model switched to mock/alt · inherit · thinking low'.encode())
        captured.clear()
        os.write(master, b'sixth message\r')
        wait_for(b'MODELS_OK', request_count=6)
        assert requests[5][1].get('reasoning_effort') == 'low', \
            'an unchanged confirmation must preserve an explicit variant override'

        # Config default removes the variant override and resumes inheritance.
        wait_completed(Path(tmp) / 'xdg-data/yourai/sessions/sessions.sqlite3', 'main', 6)
        captured.clear()
        os.write(master, b'/models\r')
        wait_for(b'Enter effort')
        os.write(master, b'\x1b[B\r')
        wait_for(b'config default')
        os.write(master, b'\x1b[A\x1b[A\x1b[A\r')  # low -> config default
        wait_for_switch('Model switched to mock/alt · inherit · thinking medium'.encode())
        captured.clear()
        os.write(master, b'seventh message\r')
        wait_for(b'MODELS_OK', request_count=7)
        assert requests[6][1].get('reasoning_effort') == 'medium', \
            'config default must restore inheritance after a variant override'
        wait_completed(Path(tmp) / 'xdg-data/yourai/sessions/sessions.sqlite3', 'main', 7)
        # F2 follows the complete picker order, including variants, then wraps.
        captured.clear()
        os.write(master, b'/models mock/alt\r')
        wait_for_switch('Model switched to mock/alt · thinking medium'.encode())
        for turn, (label, api_model, effort) in enumerate([
            ('mock/alt · inherit', 'alt-model', 'medium'),
            ('mock/smoke', 'smoke-model', None),
            ('mock/third', 'third-model', None),
            ('mock/alt', 'alt-model', 'medium'),
        ], start=8):
            captured.clear()
            os.write(master, b'\x1bOQ')
            expected = 'Model switched to ' + label
            if effort: expected += ' · thinking ' + effort
            wait_for_switch(expected.encode())
            captured.clear()
            os.write(master, f'cycle {turn}\r'.encode())
            wait_for(b'MODELS_OK', request_count=turn)
            assert requests[-1][1]['model'] == api_model
            assert requests[-1][1].get('reasoning_effort') == effort
            wait_completed(Path(tmp) / 'xdg-data/yourai/sessions/sessions.sqlite3', 'main', turn)
        os.write(master, b'\x11')  # Ctrl-Q
        wait_exit(child, master)
        assert child.returncode == 0
        assert termios.tcgetattr(slave) == original, 'terminal mode was not restored'
        print('PASS: /models two-step confirmation -> cancelled selection -> inherited/explicit effort -> configured defaults + complete F2 cycle on HTTP requests')
    finally:
        if child.poll() is None:
            child.kill()
            child.wait()
        os.close(master)
        os.close(slave)
        server.shutdown()

# Keep modal/session regressions in the existing CI smoke entry point.
subprocess.run([sys.executable, str(Path(__file__).with_name('smoke_ui.py'))], check=True)

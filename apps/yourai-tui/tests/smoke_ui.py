from smoke_support import wait_exit
"""UI regression flows: modal isolation, model defaults, deletion, narrow stats and launcher quit."""
import fcntl
import http.server
import json
import os
from pathlib import Path
import pty
import re
import select
import struct
import sqlite3
import signal
import uuid
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
        time.sleep(0.5)
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
            "models": {}
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
        os.write(master, b'/models\r')
        wait_for(b'Models')
        os.write(master, b'\r')
        wait_for(b'Model switched to mock/smoke')
        captured.clear()
        # Open a dashboard while work is in flight. Text and Esc belong to it.
        os.write(master, b'after stats\r\x02')
        wait_for(b'Session')
        os.write(master, b'LEAK\x1b')
        wait_for(b'MODELS_OK')
        assert b'Cancellation requested' not in captured
        assert len(requests) == 1
        assert 'after stats' in json.dumps(requests[0][1])
        assert 'LEAK' not in json.dumps(requests[0][1])
        db = sqlite3.connect(Path(tmp) / '.yourai/sessions/sessions.sqlite3')
        # Local commands must reset the actual model context, retain old sessions,
        # and preserve the process permission selection across a session switch.
        time.sleep(0.2)
        captured.clear()
        os.write(master, b'/yolo on\r')
        wait_for(b'YOLO enabled')
        before_new = db.execute('SELECT count(*) FROM sessions').fetchone()[0]
        captured.clear()
        os.write(master, b'/new\r')
        wait_for(b'Session ready')
        assert db.execute('SELECT count(*) FROM sessions').fetchone()[0] == before_new + 1
        captured.clear()
        os.write(master, b'\x07')
        wait_for(b'YOLO disabled')
        captured.clear()
        os.write(master, b'fresh context\r')
        wait_for(b'MODELS_OK')
        assert len(requests) == 2
        assert 'after stats' not in json.dumps(requests[-1][1])
        assert 'fresh context' in json.dumps(requests[-1][1])
        assert '/new' not in json.dumps(requests[-1][1])
        time.sleep(0.2)
        captured.clear()
        os.write(master, b'/clear\r')
        wait_for(b'New session')
        assert db.execute('SELECT count(*) FROM sessions').fetchone()[0] == before_new + 2
        captured.clear()
        os.write(master, b'after reset\r')
        wait_for(b'MODELS_OK')
        assert len(requests) == 3
        assert 'fresh context' not in json.dumps(requests[-1][1])
        assert 'after reset' in json.dumps(requests[-1][1])
        time.sleep(0.2)
        old_id = str(uuid.uuid4())
        db.execute("INSERT INTO sessions(session_id,title,created_at,updated_at) VALUES (?, 'Old review', 1, 1)", (old_id,))
        db.commit()
        captured.clear()
        os.write(master, b'/sessions\r')
        wait_for(b'Sessions')
        os.write(master, b'\x1b[200~Old review\x1b[201~\x04')
        wait_for(b'Confirm deletion')
        captured.clear()
        os.write(master, b'\r\x1b')  # Enter cannot delete; Esc returns to list.
        wait_for(b'Sessions')
        assert db.execute('SELECT count(*) FROM sessions WHERE session_id=?', (old_id,)).fetchone()[0] == 1
        captured.clear()
        os.write(master, b'\x04')
        wait_for(b'Confirm deletion')
        os.write(master, b'y')
        wait_for(b'Session deleted')
        assert db.execute('SELECT count(*) FROM sessions WHERE session_id=?', (old_id,)).fetchone()[0] == 0
        os.write(master, b'\x1b')
        # Give the app time to consume the Esc on its own: sent back-to-back,
        # ESC + Ctrl-B coalesce into Alt+Ctrl+B in the input parser and the
        # overlay stays open.
        time.sleep(0.5)
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 20, 35, 0, 0))
        os.kill(child.pid, signal.SIGWINCH)
        captured.clear()
        os.write(master, b'\x02')
        wait_for(b'Session')
        os.write(master, b'\x1b\x11')
        assert wait_exit(child, master) == 0
        assert termios.tcgetattr(slave) == original
        before = db.execute('SELECT count(*) FROM sessions').fetchone()[0]
        captured.clear()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 35, 120, 0, 0))
        child = subprocess.Popen([str(binary), '--config', str(config), '--resume'], stdin=slave, stdout=slave, stderr=slave, env=os.environ)
        wait_for(b'Sessions')
        os.write(master, b'\x11')
        assert wait_exit(child, master) == 0
        assert db.execute('SELECT count(*) FROM sessions').fetchone()[0] == before
        db.close()
        print('PASS: default model picker -> running dashboard Esc isolation -> new/clear context + runtime YOLO -> confirmed deletion -> 35-column stats -> launcher Ctrl-Q')
    finally:
        if child.poll() is None:
            child.kill()
            child.wait()
        os.close(master)
        os.close(slave)
        server.shutdown()

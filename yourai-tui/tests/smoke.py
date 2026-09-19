"""Offline POSIX TUI smoke test. Run cargo build -p yourai-tui first."""
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
        requests.append((self.path, self.headers.get('Authorization'), body))
        if not body.get('stream'):
            payload = json.dumps({'id':'compact-smoke','object':'chat.completion','model':'smoke-model','choices':[{'index':0,'message':{'role':'assistant','content':'Prior task list is complete.'},'finish_reason':'stop'}],'usage':{'prompt_tokens':1000,'completion_tokens':20,'total_tokens':1020}}).encode()
            self.send_response(200)
            self.send_header('Content-Type','application/json')
            self.send_header('Content-Length',str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return
        if len(requests) == 1:
            delta = {'tool_calls': [{'index': 0, 'id': 'call-smoke', 'type': 'function',
                     'function': {'name': 'tasks', 'arguments': '{"action":"list"}'}}]}
            reason = 'tool_calls'
        else:
            delta = {'content': 'SMOKE_STREAM_OK ' + 'x' * 1500 if len(requests) == 2 else 'SMOKE_SECOND_OK' if len(requests) == 3 else 'SMOKE_RESUME_OK'}
            reason = 'stop'
        events = [
            {'choices': [{'index': 0, 'delta': delta, 'finish_reason': None}]},
            {'choices': [{'index': 0, 'delta': {}, 'finish_reason': reason}],
             'usage': {'prompt_tokens': len(json.dumps(body)) // 3, 'completion_tokens': 2, 'total_tokens': len(json.dumps(body)) // 3 + 2}},
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
    config = Path(tmp) / 'config.toml'
    config.write_text('extensions=true\n[model]\nprovider="openai"\nname="smoke-model"\n'
                      f'base_url="http://127.0.0.1:{server.server_port}/v1"\napi_key="smoke-only"\n'
                      '[context]\ncontext_window=16000\nkeep_recent_tokens=0\nsummary_min_savings=1\n')
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 35, 120, 0, 0))
    original = termios.tcgetattr(slave)
    binary = Path(__file__).resolve().parents[2] / 'target/debug/yourai-tui'
    child = subprocess.Popen([str(binary), '--config', str(config)], stdin=slave, stdout=slave, stderr=slave)
    captured = bytearray()

    def wait_for(needle):
        end = time.monotonic() + 10
        while needle not in captured:
            if time.monotonic() > end:
                raise AssertionError(f'Missing {needle!r}; requests: {len(requests)}; output: ' + re.sub(rb'\x1b\[[0-?]*[ -/]*[@-~]', b'', captured).decode(errors='replace')[-4000:])
            if select.select([master], [], [], 0.1)[0]:
                data = os.read(master, 65536)
                captured.extend(data)
                if b'\x1b[6n' in data:
                    os.write(master, b'\x1b[1;1R')

    try:
        wait_for(b'Conversation')
        os.write(master, b'list tasks\r')
        wait_for(b'permission')
        os.write(master, b'y\r')
        wait_for(b'SMOKE_STREAM_OK')
        os.write(master, b'next question\r')
        wait_for(b'SMOKE_SECOND_OK')
        os.write(master, b'/compact\r')
        wait_for(b'Summarized')
        os.write(master, b'\x11')
        child.wait(timeout=10)
        assert child.returncode == 0
        assert termios.tcgetattr(slave) == original, 'terminal mode was not restored'
        assert len(requests) == 4
        assert all(path == '/v1/chat/completions' and auth == 'Bearer smoke-only'
                   and body['model'] == 'smoke-model' for path, auth, body in requests)
        assert any(m['role'] == 'tool' for m in requests[1][2]['messages'])
        db = sqlite3.connect(Path(tmp) / '.yourai/sessions/sessions.sqlite3')
        session = db.execute('SELECT session_id FROM sessions').fetchone()[0]
        assert db.execute("SELECT COUNT(*) FROM messages WHERE kind='summary' AND status='active'").fetchone()[0] == 1
        db.close()
        captured.clear()
        child = subprocess.Popen([str(binary), '--config', str(config), '--resume', session], stdin=slave, stdout=slave, stderr=slave)
        wait_for(b'Conversation')
        os.write(master, b'resume question\r')
        wait_for(b'SMOKE_RESUME_OK')
        assert any('Prior task list is complete.' in str(m.get('content')) for m in requests[-1][2]['messages'])
        os.write(master, b'\x11')
        child.wait(timeout=10)
        assert child.returncode == 0
        assert termios.tcgetattr(slave) == original
        print('PASS: config -> model/tool/approval -> manual compact -> SQLite summary -> resume -> clean terminal exit')
    finally:
        if child.poll() is None:
            child.kill()
            child.wait()
        os.close(master)
        os.close(slave)
        server.shutdown()

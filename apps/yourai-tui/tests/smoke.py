from smoke_support import wait_exit
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
import sys
import subprocess
import tempfile
import termios
import threading
import time

requests = []
request_times = []
rate_limit = "--rate-limit" in sys.argv
quota_limit = "--quota-limit" in sys.argv
single_request = "--single-request" in sys.argv
retry_once = "--retry-once" in sys.argv
config_headers = []
yolo = "--yolo" in sys.argv
mode_args = (["--yolo"] if yolo else []) + ["--variant", "test"]


class Model(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        request_times.append(time.monotonic())
        requests.append((self.path, self.headers.get('Authorization'), body))
        config_headers.append(self.headers.get('x-config-test'))
        if rate_limit or quota_limit or (retry_once and len(requests) == 1):
            error = {"message":"rpm exceeded", "type":"rate_limit_error", "code":"insufficient_quota" if quota_limit else "rate_limit_exceeded", "dimension":"rpm"}
            payload = json.dumps({"error": error}).encode()
            self.send_response(429)
            self.send_header('Content-Type','application/json')
            self.send_header('Content-Length',str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return
        if not body.get('stream'):
            payload = json.dumps({'id':'compact-smoke','object':'chat.completion','model':'smoke-model','choices':[{'index':0,'message':{'role':'assistant','content':'Prior task list is complete.'},'finish_reason':'stop'}],'usage':{'prompt_tokens':1000,'completion_tokens':20,'total_tokens':1020}}).encode()
            self.send_response(200)
            self.send_header('Content-Type','application/json')
            self.send_header('Content-Length',str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return
        if len(requests) == 1 and not single_request:
            delta = {'tool_calls': [{'index': 0, 'id': 'call-smoke', 'type': 'function',
                     'function': {'name': 'tasks', 'arguments': '{"action":"create","subject":"Review parser"}'}}]}
            reason = 'tool_calls'
        else:
            delta = {'content': 'SMOKE_SINGLE_OK' if single_request or retry_once else 'SMOKE_STREAM_OK ' + 'x' * 1500 if len(requests) == 2 else 'SMOKE_SECOND_OK' if len(requests) == 3 else 'SMOKE_RESUME_OK'}
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
    config = Path(tmp) / 'config.json'
    config.write_text(json.dumps({
        "model": "mock/smoke", "extensions": False,
        "provider": {"mock": {
            "npm": "@ai-sdk/openai-compatible",
            "options": {"baseURL": f"http://127.0.0.1:{server.server_port}/v1", "apiKey": "smoke-only",
                        "headers": {"x-config-test": "provider"}, "requests": {"cooldown_seconds": 7}},
            "models": {"smoke": {"id": "smoke-model", "limit": {"context": 16000, "output": 4096},
                        "headers": {"x-config-test": "model"},
                        "options": {"maxOutputTokens": 4096, "temperature": 0.8, "topP": 0.9, "reasoningEffort": "low", "user": "config-smoke"},
                        "variants": {"test": {"temperature": 0.2, "reasoningEffort": "high"}}}}
        }},
        "context": {"keep_recent_tokens": 0, "summary_min_savings": 1}
    }))
    # Mock clipboard commands: exercise mouse-release copying without changing the user's clipboard.
    clipboard_file = Path(tmp) / 'clipboard.txt'
    bin_dir = Path(tmp) / 'bin'
    bin_dir.mkdir()
    for program in ('pbcopy', 'wl-copy', 'xclip', 'xsel'):
        command = bin_dir / program
        command.write_text('#!' + sys.executable + '\nimport os,sys\nfrom pathlib import Path\nPath(os.environ["SMOKE_CLIPBOARD"]).write_text(sys.stdin.read())\n')
        command.chmod(0o755)
    child_env = dict(os.environ, PATH=str(bin_dir) + os.pathsep + os.environ['PATH'], SMOKE_CLIPBOARD=str(clipboard_file))
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 35, 120, 0, 0))
    original = termios.tcgetattr(slave)
    binary = Path(__file__).resolve().parents[3] / 'target/debug/yourai-tui'
    child = subprocess.Popen([str(binary), '--config', str(config)] + mode_args, stdin=slave, stdout=slave, stderr=slave, env=child_env)
    captured = bytearray()

    def wait_for(needle, timeout=10):
        end = time.monotonic() + timeout
        while needle not in re.sub(rb'\x1b\[[0-?]*[ -/]*[@-~]', b'', captured):
            if time.monotonic() > end:
                raise AssertionError(f'Missing {needle!r}; requests: {len(requests)}; output: ' + re.sub(rb'\x1b\[[0-?]*[ -/]*[@-~]', b'', captured).decode(errors='replace')[-4000:])
            if select.select([master], [], [], 0.1)[0]:
                data = os.read(master, 65536)
                captured.extend(data)
                if b'\x1b[6n' in data:
                    os.write(master, b'\x1b[1;1R')

    try:
        wait_for(b'New session')
        if rate_limit or quota_limit or single_request or retry_once:
            os.write(master,b'hello\r')
            if single_request or retry_once:
                wait_for(b'SMOKE_SINGLE_OK')
                expected = 2 if retry_once else 1
                if retry_once:
                    wait_for(b"HTTP 429, code=rate_limit_exceeded, dimension=rpm")
                    assert request_times[1] - request_times[0] >= 7
            else:
                expected = 1 if quota_limit else 3
                wait_for(f'stopped after {expected} attempt(s)'.encode(), timeout=25)
                wait_for(b'Execution failed')
            assert len(requests) == expected, len(requests)
            if rate_limit:
                gaps = [b-a for a,b in zip(request_times,request_times[1:])]
                # Exponential backoff is now 2s, then 4s; the configured
                # shared 7s cooldown dominates both attempts.
                assert all(gap >= 7 for gap in gaps), gaps
            os.write(master,b'\x11')
            wait_exit(child, master)
            assert child.returncode == 0
            assert termios.tcgetattr(slave) == original
            db = sqlite3.connect(Path(tmp) / '.yourai/sessions/sessions.sqlite3')
            logs = db.execute('SELECT source, outcome, http_status, attempt, session_id, turn_id FROM model_requests ORDER BY started_at').fetchall()
            assert len(logs) == expected, logs
            assert all(row[0] == 'main' and row[4] and row[5] for row in logs), logs
            assert [row[3] for row in logs] == list(range(1, expected + 1)), logs
            assert all(row[2] == 429 for row in logs if row[1] == 'failed'), logs
            assert logs[-1][1] == ('completed' if single_request or retry_once else 'failed'), logs
            db.close()
            print(f'PASS: real SDK HTTP path -> {expected} requests; intervals {[round(b-a,2) for a,b in zip(request_times,request_times[1:])]}')
            sys.exit(0)
        os.write(master, b'\x02')  # Ctrl-B opens the stats dashboard.
        wait_for(b'16.0K')
        os.write(master, b'\x02')  # Close the dashboard.
        time.sleep(0.3)
        os.write(master, b'\x1b[<0;1;35M\x1b[<32;11;35M\x1b[<0;11;35m')
        wait_for(b'Copied')
        assert clipboard_file.read_text() == 'New session', repr(clipboard_file.read_text())
        os.write(master, b'/')
        wait_for(b'/continue')
        os.write(master, b'\x1b\x7f/theme nord\r')
        # Esc dismisses completion; Backspace removes the slash before a new command.
        # Theme command is local; wait for the editor to clear (no theme button in footer now).
        time.sleep(0.5)
        assert len(requests) == 0, 'theme command should not reach the model'
        os.write(master, b'\x1b[200~list tasks\nsecond line\x1b[201~\r')
        if yolo:
            wait_for(b'YOLO')
        else:
            wait_for(b'Allow tasks?')
            os.write(master, b'\r')
            wait_for(b'Enter y to allow once')
            assert len(requests) == 1, 'empty reply approved a tool'
        if yolo:
            wait_for(b'SMOKE_STREAM_OK')
        content = next(m['content'] for m in requests[0][2]['messages'] if m['role'] == 'user')
        if isinstance(content, list):
            content = ''.join(p.get('text', '') for p in content)
        assert content == 'list tasks\nsecond line', repr(content)
        if not yolo:
            os.write(master, b'y\r')
            # Clear the historical approval dialog (which contains the tool input JSON)
            # before checking the folded card preview — only the current screen matters.
            captured.clear()
        wait_for(b'SMOKE_STREAM_OK')
        wait_for(b'Todo')
        wait_for(b'Review parser')
        # Card preview shows the tool OUTPUT (Task JSON), but the INPUT JSON
        # (with "action":"create") stays hidden until the block is expanded.
        assert b'"action"' not in captured, 'tool input should start folded'
        os.write(master, b'\x1b[17~\x0f')  # F6 selects the tool; Ctrl-O opens only that block.
        wait_for(b'"action"')
        os.write(master, b'\x0f\x1b[1;5F')  # fold again and follow the latest output
        os.write(master, b'next question\r')
        wait_for(b'SMOKE_SECOND_OK')
        os.write(master, b'/compact\r')
        wait_for(b'Summarized')
        os.write(master, b'\x11')
        wait_exit(child, master)
        assert child.returncode == 0
        assert termios.tcgetattr(slave) == original, 'terminal mode was not restored'
        assert len(requests) == 4
        assert all(value == 'model' for value in config_headers)
        for _, _, body in requests:
            assert body.get('max_tokens', body.get('max_completion_tokens')) == 4096, body
            assert body['temperature'] == 0.2
            assert body['top_p'] == 0.9
            assert body['reasoning_effort'] == 'high'
            assert body['user'] == 'config-smoke'
        assert all(path == '/v1/chat/completions' and auth == 'Bearer smoke-only'
                   and body['model'] == 'smoke-model' for path, auth, body in requests)
        assert any(m['role'] == 'tool' for m in requests[1][2]['messages'])
        db = sqlite3.connect(Path(tmp) / '.yourai/sessions/sessions.sqlite3')
        session = db.execute('SELECT session_id FROM sessions').fetchone()[0]
        assert db.execute("SELECT source, COUNT(*) FROM model_requests GROUP BY source ORDER BY source").fetchall() == [('compact', 1), ('main', 3)]
        assert db.execute("SELECT COUNT(*) FROM model_requests WHERE outcome='completed'").fetchone()[0] == 4
        assert db.execute("SELECT COUNT(*) FROM messages WHERE kind='summary' AND status='active'").fetchone()[0] == 1
        db.close()
        captured.clear()
        child = subprocess.Popen([str(binary), '--config', str(config), '--resume', session] + mode_args, stdin=slave, stdout=slave, stderr=slave, env=child_env)
        wait_for(b'list tasks')  # session title persists in the footer
        wait_for(b'SMOKE_SECOND_OK')  # history must be visible before a new request
        wait_for(b'Todo')
        wait_for(b'Review parser')
        os.write(master, b'resume question\r')
        wait_for(b'SMOKE_RESUME_OK')
        assert any('Prior task list is complete.' in str(m.get('content')) for m in requests[-1][2]['messages'])
        os.write(master, b'\x11')
        wait_exit(child, master)
        assert child.returncode == 0
        assert termios.tcgetattr(slave) == original
        print(('YOLO ' if yolo else '') + 'PASS: mouse auto-copy -> theme/context -> multiline paste -> approval policy -> folded tool toggle -> Todo -> compact -> restored history/tasks -> clean exit')
    finally:
        if child.poll() is None:
            child.kill()
            child.wait()
        os.close(master)
        os.close(slave)
        server.shutdown()

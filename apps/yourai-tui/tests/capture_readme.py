"""Capture real TUI terminal output for the README, using a local demo model.

Requires Pillow + pyte (documentation tooling only). No API key/network model.
Run from the repository root after `cargo build -p yourai-tui`.
"""
import argparse
import codecs
import http.server
import json
import os
from pathlib import Path
import select
import subprocess
import tempfile
import threading
import time

from PIL import Image, ImageDraw, ImageFont, PngImagePlugin
import pyte
from pty_probe import run_pty
from smoke_support import DEFAULT_BIN, REPO_ROOT, _answer_queries, wait_exit


class DemoModel(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        step = self.server.step
        self.server.step += 1
        task_id = None
        for message in body.get('messages', []):
            if message.get('tool_call_id') == 'demo-create':
                task_id = json.loads(message['content'])['id']
            if message.get('tool_call_id') == 'demo-tests':
                result = json.loads(message['content'])
                assert result['exit_code'] == 0 and 'Ran 2 tests' in result['stderr'], result
        plan = [
            ('demo-create', 'tasks', {'action': 'create', 'subject': 'Fix parser boundary check'}),
            ('demo-read', 'read', {'path': 'parser.py'}),
            ('demo-edit', 'edit', {'path': 'parser.py', 'old_text': 'index >= len(tokens) - 1', 'new_text': 'index >= len(tokens)'}),
            ('demo-tests', 'shell', {'command': 'python3 -m unittest -v'}),
            ('demo-complete', 'tasks', {'action': 'complete', 'id': task_id}),
        ]
        if step < len(plan):
            call_id, name, arguments = plan[step]
            delta = {'tool_calls': [{'index': 0, 'id': call_id, 'type': 'function',
                     'function': {'name': name, 'arguments': json.dumps(arguments)}}]}
            reason = 'tool_calls'
        else:
            delta = {'content': '## Parser fixed\n\nThe parser now returns the final token and handles end-of-input.\n\n- Updated `parser.py` with the corrected boundary check.\n- Ran `python3 -m unittest -v`: **2 tests passed**.\n'}
            reason = 'stop'
        events = [
            {'choices': [{'index': 0, 'delta': delta, 'finish_reason': None}]},
            {'choices': [{'index': 0, 'delta': {}, 'finish_reason': reason}],
             'usage': {'prompt_tokens': len(json.dumps(body)) // 3, 'completion_tokens': 80,
                       'total_tokens': len(json.dumps(body)) // 3 + 80}},
        ]
        payload = (''.join('data: ' + json.dumps(e) + '\n\n' for e in events) + 'data: [DONE]\n\n').encode()
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Content-Length', str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


def color(value, default):
    colors = {'black': '#000000', 'red': '#cd3131', 'green': '#0dbc79',
              'brown': '#e5e510', 'blue': '#2472c8', 'magenta': '#bc3fbc',
              'cyan': '#11a8cd', 'white': '#e5e5e5', 'brightblack': '#666666',
              'brightred': '#f14c4c', 'brightgreen': '#23d18b', 'brightbrown': '#f5f543',
              'brightblue': '#3b8eea', 'brightmagenta': '#d670d6',
              'brightcyan': '#29b8db', 'brightwhite': '#ffffff'}
    if value == 'default':
        return default
    return colors.get(value, '#' + value)


def capture(screen, path, font_path):
    font = ImageFont.truetype(str(font_path), 18)
    # Menlo's collection has matching bold/italic faces; plain TTF fonts use one face.
    bold = ImageFont.truetype(str(font_path), 18, index=1) if font_path.suffix == '.ttc' else font
    width, height, padding = round(font.getlength('M')), 25, 16
    image = Image.new('RGB', (screen.columns * width + padding * 2,
                             screen.lines * height + padding * 2), '#000000')
    draw = ImageDraw.Draw(image)
    for y in range(screen.lines):
        for x in range(screen.columns):
            cell = screen.buffer[y][x]
            fg, bg = color(cell.fg, '#dedee2'), color(cell.bg, '#000000')
            if cell.reverse:
                fg, bg = bg, fg
            px, py = padding + x * width, padding + y * height
            draw.rectangle((px, py, px + width - 1, py + height - 1), fill=bg)
            if cell.data.strip():
                draw.text((px, py), cell.data, font=bold if cell.bold else font, fill=fg)
            if cell.underscore:
                draw.line((px, py + 21, px + width - 1, py + 21), fill=fg)
    info = PngImagePlugin.PngInfo()
    info.add_text('Description', 'Actual YourAI TUI output captured from a PTY. Local demo model; real file tools and tests.')
    image.save(path, pnginfo=info, optimize=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--font', type=Path)
    parser.add_argument('--output', type=Path, default=REPO_ROOT / 'docs/screenshots')
    args = parser.parse_args()
    fonts = [args.font] if args.font else [Path('/System/Library/Fonts/Menlo.ttc'), Path('/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf')]
    font = next((p for p in fonts if p.is_file()), None)
    if font is None:
        parser.error('Use --font PATH to a monospace font')
    args.output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='yourai-demo-', dir='/tmp') as tmp:
        tmp = Path(tmp)
        workspace = tmp / 'demo-project'
        workspace.mkdir()
        (workspace / 'parser.py').write_text('def next_token(tokens, index):\n    if index >= len(tokens) - 1:\n        return None\n    return tokens[index]\n')
        (workspace / 'test_parser.py').write_text('import unittest\nfrom parser import next_token\n\nclass ParserTests(unittest.TestCase):\n    def test_last_token(self):\n        self.assertEqual(next_token(["a", "b"], 1), "b")\n    def test_past_end(self):\n        self.assertIsNone(next_token(["a", "b"], 2))\n')
        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), DemoModel)
        server.step = 0
        threading.Thread(target=server.serve_forever, daemon=True).start()
        config = tmp / 'config.json'
        model = {'id': 'demo-code', 'limit': {'context': 128000, 'output': 4096},
                 'options': {'reasoningEffort': 'medium'},
                 'variants': {'fast': {'reasoningEffort': 'low'}, 'deep': {'reasoningEffort': 'high'}}}
        provider = {'api': 'openai-compatible', 'options': {
            'baseURL': f'http://127.0.0.1:{server.server_port}/v1', 'apiKey': 'demo-only'},
            'models': {'code': model}}
        config.write_text(json.dumps({'model': 'demo/code', 'theme': 'dark',
            'session_dir': str(tmp / 'sessions'), 'provider': {'demo': provider}}))
        env = {k: v for k, v in os.environ.items() if k not in ('TMUX', 'TMUX_PANE', 'TERM_PROGRAM')}
        env['TERM'] = 'xterm-256color'
        child, master = run_pty([str(DEFAULT_BIN), '--config', str(config)], env=env, cwd=workspace, cols=120, rows=38)
        screen = pyte.Screen(120, 38)
        stream = pyte.Stream(screen)
        decoder = codecs.getincrementaldecoder('utf-8')('replace')

        def pump(seconds=0.3):
            end = time.monotonic() + seconds
            while time.monotonic() < end:
                if select.select([master], [], [], 0.05)[0]:
                    data = os.read(master, 65536)
                    _answer_queries(master, data)
                    stream.feed(decoder.decode(data))

        def wait(text, timeout=15):
            end = time.monotonic() + timeout
            while text not in '\n'.join(screen.display):
                if child.poll() is not None or time.monotonic() > end:
                    raise AssertionError(f'Missing {text}:\n' + '\n'.join(screen.display))
                pump(0.1)
            pump()

        try:
            wait('New session')
            os.write(master, b'Fix the parser boundary check and run the tests.\r')
            wait('Allow tasks?')
            os.write(master, b'a')
            wait('Allow shell?')
            capture(screen, args.output / 'approval.png', font)
            os.write(master, b'y')
            wait('2 tests passed')
            pump(1)
            result = subprocess.run(['python3', '-m', 'unittest', '-v'], cwd=workspace, capture_output=True)
            assert result.returncode == 0, result.stderr
            os.write(master, b'/models\r')
            wait('Models')
            capture(screen, args.output / 'models.png', font)
            os.write(master, b'\x1b')
            pump(0.6)
            os.write(master, b'\x14')  # Hide the task sidebar for a full-width conversation.
            pump(0.6)
            capture(screen, args.output / 'conversation.png', font)
            os.write(master, b'\x11')
            assert wait_exit(child, master) == 0
        finally:
            if child.poll() is None:
                child.kill()
                child.wait()
            os.close(master)
            server.shutdown()
            server.server_close()
    print('Captured conversation.png, approval.png, models.png from the actual TUI')


if __name__ == '__main__':
    main()

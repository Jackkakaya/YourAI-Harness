"""External editor handoff on a controlling PTY, including signal shutdown."""
import fcntl
import os
from pathlib import Path
import pty
import shlex
import signal
import struct
import subprocess
import tempfile
import termios
import time

from smoke_support import DEFAULT_BIN, _Tap, start_model_server, write_yourai_config


def wait_for(test, message, timeout=5):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if test():
            return
        time.sleep(0.01)
    raise AssertionError(message)


def alive(pid):
    result = subprocess.run(['ps', '-o', 'stat=', '-p', str(pid)], capture_output=True, text=True)
    state = result.stdout.strip()
    return bool(state) and not state.startswith('Z')


with tempfile.TemporaryDirectory() as tmp:
    root = Path(tmp)
    editor = root / 'editor with spaces'
    editor.write_text('''#!/bin/sh
[ "$1" = 'argument with spaces' ] || exit 9
record="$2"
file="$3"
count=0
[ ! -f "$record/count" ] || count=$(cat "$record/count")
count=$((count + 1))
printf '%s' "$count" > "$record/count"
printf '%s' "$file" > "$record/file"
if [ "$count" -le 2 ]; then
    printf 'EDITOR_READY_%s\\n' "$count"
    IFS= read -r answer || exit 8
    printf 'EDITOR_SAVED_%s: %s\\n' "$count" "$answer" > "$file"
    exit 0
fi
if [ "$count" -eq 3 ]; then
    printf 'CANCELLED_CONTENT' > "$file"
    exit 1
fi
sleep 60 &
printf '%s %s' "$$" "$!" > "$record/pids"
wait
''')
    editor.chmod(0o700)
    server = start_model_server('OK')
    config = write_yourai_config(tmp, server.server_port)
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 30, 100, 0, 0))
    original = termios.tcgetattr(slave)

    def own_terminal():
        os.setsid()
        fcntl.ioctl(slave, termios.TIOCSCTTY, 0)

    env = dict(os.environ, TERM='xterm-256color', XDG_DATA_HOME=str(root / 'data'),
               VISUAL=shlex.join([str(editor), 'argument with spaces', str(root)]))
    child = subprocess.Popen([str(DEFAULT_BIN), '--config', str(config)], cwd=tmp,
                             stdin=slave, stdout=slave, stderr=slave, env=env,
                             preexec_fn=own_terminal)
    tap = _Tap(master).start()
    pids = []
    try:
        tap.wait_for(b'New session')
        for count in (1, 2):
            child_marker = f'EDITOR_READY_{count}'.encode()
            # Repeat the handoff immediately after the previous screen returns.
            os.write(master, b'\x18')  # Ctrl-X
            tap.wait_for(child_marker)
            answer = f'typed in editor {count}'.encode()
            os.write(master, answer + b'\r')
            # The child never prints this marker, so seeing it proves the UI
            # admitted the edited file and resumed rendering.
            tap.wait_for(f'EDITOR_SAVED_{count}'.encode())
            temporary = Path((root / 'file').read_text())
            wait_for(lambda: not temporary.exists(), 'editor temp file was not removed')
        before_cancel = len(tap.buf)
        os.write(master, b'\x18')
        tap.wait_for(b'draft unchanged')
        returned = bytes(tap.buf[before_cancel:])
        assert b'EDITOR_SAVED_2' in returned and b'CANCELLED_CONTENT' not in returned
        os.write(master, b'\x18')
        wait_for(lambda: (root / 'pids').exists() and len((root / 'pids').read_text().split()) == 2,
                 'interruptible editor did not start')
        pids = [int(pid) for pid in (root / 'pids').read_text().split()]
        assert all(alive(pid) for pid in pids)
        temporary = Path((root / 'file').read_text())
        # Suspend restored cooked input before giving stdin to the editor.
        restored = termios.tcgetattr(slave)
        assert restored[3] & (termios.ICANON | termios.ECHO) == original[3] & (termios.ICANON | termios.ECHO)
        os.kill(child.pid, signal.SIGTERM)  # Signal the TUI alone, not its editor.
        child.wait(timeout=6)
        assert child.returncode == 128 + signal.SIGTERM, child.returncode
        wait_for(lambda: not any(alive(pid) for pid in pids), 'editor or descendant survived TUI shutdown')
        assert not temporary.exists(), 'interrupted editor file leaked'
        # Session-leader exit revokes this PTY on macOS; no ioctl afterwards.
        print('PASS: quoted editor + args -> repeated terminal handoff -> cancelled draft unchanged -> SIGTERM reaps editor and descendant -> terminal/file cleanup')
    finally:
        if child.poll() is None:
            child.kill()
            child.wait()
        for pid in pids:
            if alive(pid):
                os.kill(pid, signal.SIGKILL)
        tap.kill()
        os.close(master)
        os.close(slave)
        server.shutdown()
        server.server_close()

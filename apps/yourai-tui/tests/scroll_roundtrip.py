"""Round-trip correctness: scroll up N then back down N; the final screen must
match the pre-scroll screen (modulo the cursor row)."""
import re
import sys
import time
from pathlib import Path

from smoke_support import Probe, code_text, wait_exit


class Vt:
    """Decode the stream into a screen grid (SGR ignored)."""

    def __init__(self, width, height):
        self.grid = [[' '] * width for _ in range(height)]
        self.x = 0
        self.y = 0
        self.width = width
        self.height = height
        self.top = 1
        self.bottom = height

    def feed(self, data):
        text = data.decode('utf-8', 'replace')
        i = 0
        while i < len(text):
            ch = text[i]
            if ch == '\x1b' and i + 1 < len(text) and text[i + 1] == '[':
                j = i + 2
                while j < len(text) and (text[j].isdigit() or text[j] in ';?'):
                    j += 1
                if j >= len(text):
                    return  # partial sequence; wait for more
                params = text[i + 2:j]
                final = text[j]
                numbers = [int(p) if p else 0 for p in params.split(';')] if not params.startswith('?') else []
                pick = lambda k, d: (numbers[k] if k < len(numbers) and numbers[k] else d)
                if final == 'H':
                    self.y, self.x = pick(0, 1) - 1, pick(1, 1) - 1
                elif final == 'r':
                    self.top, self.bottom = pick(0, 1), pick(1, self.height)
                elif final == 'S':
                    self.scroll(pick(0, 1), True)
                elif final == 'T':
                    self.scroll(pick(0, 1), False)
                i = j + 1
            else:
                if 0 <= self.y < self.height and 0 <= self.x < self.width:
                    self.grid[self.y][self.x] = ch
                self.x += 1
                i += 1

    def scroll(self, n, up):
        top, bottom = self.top - 1, self.bottom
        region = [row[:] for row in self.grid[top:bottom]]
        blank = [' '] * self.width
        for offset in range(top, bottom):
            rel = offset - top
            src = rel + n if up else rel - n
            self.grid[offset] = region[src][:] if 0 <= src < len(region) else blank[:]

    def lines(self):
        return [''.join(row).rstrip() for row in self.grid]


def run():
    binary = Path(sys.argv[1]).resolve()
    with Probe(binary, text=code_text()) as p:
        vt = Vt(p.cols, p.rows)
        pending = bytearray()
        fed = 0

        def pump(timeout):
            """Feed the VT complete escape sequences from the tap's raw bytes."""
            nonlocal fed
            end = time.monotonic() + timeout
            while time.monotonic() < end:
                pending.extend(p.tap.buf[fed:])
                fed = len(p.tap.buf)
                data = bytes(pending)
                cut = len(data)
                for m in re.finditer(rb'\x1b(?:\[[0-9;?]*)?\Z', data):
                    cut = m.start()
                vt.feed(data[:cut])
                del pending[:cut]
                time.sleep(0.02)

        p.wait(b'New session')
        p.send(b'render a long transcript\r')
        p.wait(b'results.')
        pump(0.8)
        before = vt.lines()
        p.send(b'\x1b[<64;10;10M' * 25)  # scroll up 25 lines
        pump(0.8)
        midway = vt.lines()
        p.send(b'\x1b[<65;10;10M' * 25)  # scroll back down
        pump(0.8)
        after = vt.lines()
        diff_rows = [(i, a, b) for i, (a, b) in enumerate(zip(before, after)) if a != b]
        # The nav label row in the composer is allowed to flip (scroll==0).
        allowed = all('Latest' in a or 'Latest' in b or 'Question' in a or 'Question' in b
                      for _, a, b in diff_rows)
        print(f'round-trip rows-changed={len(diff_rows)} allowed={allowed}')
        for i, a, b in diff_rows[:5]:
            print(f'  row {i}: {a[:60]!r} -> {b[:60]!r}')
        assert midway != before, 'scrolling up must move content'
        assert allowed and len(diff_rows) <= 3, f'screen mismatch after round trip: {diff_rows}'
        print('ROUND TRIP OK')
        p.send(b'\x11')
        assert wait_exit(p.child, p.master) == 0


if __name__ == '__main__':
    run()

"""Regression checks for the terminal grid used by HTTP/PTY assertions."""
import unittest

from pty_probe import Screen


class ScreenTests(unittest.TestCase):
    def test_utf8_csi_and_osc_survive_every_byte_boundary(self):
        frame = '\x1b[2J\x1b]0;title\x1b\\\x1b[2;3H模型 high\x1b[3;1Hdone'.encode()
        whole = Screen(30, 5)
        whole.feed(frame)
        self.assertEqual(whole.text(), '\n  模型 high\ndone')
        for cut in range(len(frame) + 1):
            screen = Screen(30, 5)
            screen.feed(frame[:cut])
            screen.feed(frame[cut:])
            self.assertEqual(screen.text(), whole.text(), cut)
        screen = Screen(30, 5)
        for byte in frame:
            screen.feed(bytes([byte]))
        self.assertEqual(screen.text(), whole.text())

    def test_native_scroll_changes_only_the_declared_region(self):
        screen = Screen(20, 5)
        for row, text in enumerate(['outside top', 'one', 'two', 'three', 'outside bottom'], 1):
            screen.feed(f'\x1b[{row};1H{text}'.encode())
        screen.feed(b'\x1b[2;4r\x1b[S')
        self.assertEqual(screen.text(), 'outside top\ntwo\nthree\n\noutside bottom')
        screen.feed(b'\x1b[T')
        self.assertEqual(screen.text(), 'outside top\n\ntwo\nthree\noutside bottom')


if __name__ == '__main__':
    unittest.main()

//! Shared terminal sizing for the launcher and conversation UI.
use ratatui::{
    backend::{ClearType, CrosstermBackend, WindowSize},
    buffer::Cell,
    prelude::*,
};
use std::io;

/// Backend wrapper whose `size()` reads the window size from our own stdio.
///
/// crossterm 0.28's `terminal::size()` prefers `/dev/tty` — the *controlling*
/// terminal — over the process's own descriptors. When stdin/stdout are a
/// different tty (PTY smoke tests, embedded panes, piped stdio), every frame
/// would be laid out for the wrong screen and PTY resizes would never
/// register. Prefer our own tty descriptors and fall back to stock crossterm
/// behavior when neither is a usable terminal.
pub(crate) struct SizedBackend<W: io::Write>(pub CrosstermBackend<W>);
impl<W: io::Write> Backend for SizedBackend<W> {
    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.0.draw(content)
    }
    fn hide_cursor(&mut self) -> io::Result<()> {
        self.0.hide_cursor()
    }
    fn show_cursor(&mut self) -> io::Result<()> {
        self.0.show_cursor()
    }
    fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.0.get_cursor_position()
    }
    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.0.set_cursor_position(position)
    }
    fn clear(&mut self) -> io::Result<()> {
        self.0.clear()
    }
    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.0.clear_region(clear_type)
    }
    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.0.append_lines(n)
    }
    fn size(&self) -> io::Result<Size> {
        match stdio_window_size() {
            Some(size) => Ok(size.columns_rows),
            None => self.0.size(),
        }
    }
    fn window_size(&mut self) -> io::Result<WindowSize> {
        if let Some(size) = stdio_window_size() {
            return Ok(size);
        }
        self.0.window_size()
    }
    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.0)
    }
}
/// Read the actual output terminal before considering the input terminal.
#[cfg(unix)]
fn stdio_window_size() -> Option<WindowSize> {
    [libc::STDOUT_FILENO, libc::STDIN_FILENO]
        .into_iter()
        .find_map(tty_window_size)
}
#[cfg(unix)]
fn tty_window_size(fd: libc::c_int) -> Option<WindowSize> {
    let mut ws = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: ioctl only writes a winsize into this valid, exclusively borrowed
    // allocation. An invalid or non-terminal fd returns an error.
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } != 0
        || ws.ws_col == 0
        || ws.ws_row == 0
    {
        return None;
    }
    Some(WindowSize {
        columns_rows: Size::new(ws.ws_col, ws.ws_row),
        pixels: Size::new(ws.ws_xpixel, ws.ws_ypixel),
    })
}
#[cfg(not(unix))]
fn stdio_window_size() -> Option<WindowSize> {
    None
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    #[test]
    fn descriptor_size_tracks_pty_resize_and_rejects_invalid_sizes() {
        let (mut master, mut slave) = (-1, -1);
        // SAFETY: output pointers are valid; the optional name/termios/winsize are omitted.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        // SAFETY: openpty returned two distinct owned descriptors.
        let (_master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        for (cols, rows) in [(120, 35), (35, 20), (0, 0)] {
            let ws = libc::winsize {
                ws_row: rows,
                ws_col: cols,
                ws_xpixel: 900,
                ws_ypixel: 600,
            };
            // SAFETY: TIOCSWINSZ reads a valid winsize for the lifetime of the call.
            assert_eq!(
                unsafe { libc::ioctl(slave.as_raw_fd(), libc::TIOCSWINSZ, &ws) },
                0
            );
            let actual = tty_window_size(slave.as_raw_fd());
            if cols == 0 {
                assert!(actual.is_none());
            } else {
                let actual = actual.unwrap();
                assert_eq!(actual.columns_rows, Size::new(cols, rows));
                assert_eq!(actual.pixels, Size::new(900, 600));
            }
        }
        assert!(tty_window_size(-1).is_none());
    }
}

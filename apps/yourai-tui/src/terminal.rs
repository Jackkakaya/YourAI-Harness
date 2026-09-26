//! Shared terminal sizing, screen lifecycle and scroll backend for the
//! launcher and conversation UI.
use crossterm::{
    cursor::Show,
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture},
    execute, queue,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::{Backend, ClearType, WindowSize},
    buffer::{Buffer, Cell},
    prelude::*,
};
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use unicode_width::UnicodeWidthStr;

/// The transcript rectangle the renderer scrolls, published per frame.
pub(crate) type SharedRegion = Arc<Mutex<Option<Rect>>>;

/// Alternate-screen guard shared by the conversation UI and the launcher:
/// raw mode, bracketed paste and mouse capture on open, everything restored
/// on drop. Both front ends must not keep their own copies of this ritual —
/// the launcher once drifted (missing mouse-mode fix) precisely because it did.
pub(crate) struct Screen;
impl Screen {
    pub(crate) fn open() -> Result<Self, io::Error> {
        enable_raw_mode()?;
        let setup = (|| {
            execute!(
                io::stdout(),
                EnterAlternateScreen,
                EnableBracketedPaste,
                EnableMouseCapture
            )?;
            // Keep button/drag/wheel tracking, without flooding the input
            // queue on hover events.
            #[cfg(unix)]
            {
                let mut output = io::stdout();
                output.write_all(b"\x1b[?1003l\x1b[?1002h")?;
                output.flush()?;
            }
            Ok::<(), io::Error>(())
        })();
        match setup {
            Ok(()) => Ok(Self),
            Err(e) => {
                restore();
                Err(e)
            }
        }
    }
}
/// Restore the terminal to its pre-screen state: alternate screen off, cooked
/// mode, mouse/paste off, cursor visible, and synchronized output definitely
/// closed (a crashed frame may have left it open).
pub(crate) fn restore() {
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        LeaveAlternateScreen,
        DisableBracketedPaste,
        DisableMouseCapture,
        Show
    );
    let _ = io::stdout().write_all(b"\x1b[?2026l");
    let _ = io::stdout().flush();
}
impl Drop for Screen {
    fn drop(&mut self) {
        restore();
    }
}

/// Backend wrapper whose `size()` reads the window size from our own stdio.
///
/// crossterm 0.28's `terminal::size()` prefers `/dev/tty` — the *controlling*
/// terminal — over the process's own descriptors. When stdin/stdout are a
/// different tty (PTY smoke tests, embedded panes, piped stdio), every frame
/// would be laid out for the wrong screen and PTY resizes would never
/// register. Prefer our own tty descriptors and fall back to stock crossterm
/// behavior when neither is a usable terminal.
pub(crate) struct SizedBackend<B: Backend>(pub B);
impl<B: Backend> Backend for SizedBackend<B> {
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

/// How the visible transcript moved between two frames.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Shift {
    /// Content moved up: the terminal scrolls up, new rows appear at the bottom.
    Up(u16),
    /// Content moved down: the terminal scrolls down, new rows appear at the top.
    Down(u16),
}

/// Backend that turns pure vertical transcript shifts into terminal-native
/// scrolls (DECSTBM + SU/SD) and repaints only the newly exposed rows.
///
/// Repainting every shifted row costs ~28 bytes of 24-bit SGR per syntax
/// token; scrolling code-heavy text at 60fps reaches ~1MB/s, which slower
/// emulators cannot paint. A native scroll shifts their own backbuffer and
/// leaves only the exposed rows to encode. Non-shift frames fall back to a
/// regular cell diff that, unlike ratatui's crossterm backend, emits
/// foreground and background color changes separately.
pub(crate) struct ScrollBackend<W: io::Write> {
    writer: W,
    region: SharedRegion,
    /// Our model of what the terminal currently shows; kept in sync by
    /// applying every frame's diff.
    prev: Option<Buffer>,
    /// Decided once at construction: native scrolls pay off on a direct
    /// terminal; behind a mux a cell diff feeds its incremental client
    /// protocol instead. `YOURAI_TUI_SCROLL=off` disables it for
    /// measurements.
    native_scroll: bool,
    /// Hermetic screen area for tests: the real stdio may be a tty even
    /// under `cargo test -- --nocapture`, which would override test buffers.
    #[cfg(test)]
    test_area: Option<Rect>,
}
impl<W: io::Write> ScrollBackend<W> {
    pub(crate) fn new(writer: W, region: SharedRegion) -> Self {
        Self {
            writer,
            region,
            prev: None,
            native_scroll: std::env::var_os("YOURAI_TUI_SCROLL").as_deref()
                != Some(std::ffi::OsStr::new("off"))
                && !inside_tmux(),
            #[cfg(test)]
            test_area: None,
        }
    }
    /// Overrides the native-scroll decision in tests, whatever the host env.
    #[cfg(test)]
    fn test_set_native_scroll(&mut self, on: bool) {
        self.native_scroll = on;
    }
    fn model_area(&self, diff: &[(u16, u16, Cell)]) -> Rect {
        #[cfg(test)]
        if let Some(area) = self.test_area {
            return area;
        }
        if let Some(size) = stdio_window_size() {
            return Rect::new(0, 0, size.columns_rows.width, size.columns_rows.height);
        }
        if let Some(prev) = &self.prev {
            return prev.area;
        }
        let width = diff.iter().map(|(x, _, _)| *x).max().map_or(0, |w| w + 1);
        let height = diff.iter().map(|(_, y, _)| *y).max().map_or(0, |h| h + 1);
        Rect::new(0, 0, width, height)
    }
    fn publish(&mut self, next: Buffer) -> io::Result<()> {
        self.prev = Some(next);
        Ok(())
    }
}
impl<W: io::Write> Backend for ScrollBackend<W> {
    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let diff: Vec<(u16, u16, Cell)> =
            content.map(|(x, y, cell)| (x, y, cell.clone())).collect();
        let area = self.model_area(&diff);
        let mut next = match &self.prev {
            Some(prev) if prev.area == area => prev.clone(),
            _ => Buffer::empty(area),
        };
        for (x, y, cell) in &diff {
            if let Some(target) = next.cell_mut(Position::new(*x, *y)) {
                *target = cell.clone();
            }
        }
        let region = self
            .region
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .filter(|r| r.area() > 0 && r.height >= 2 && r.y + r.height <= area.height);
        let mut out = Vec::with_capacity(diff.len() * 8 + 64);
        let mut encoder = Encoder::new(&mut out);
        let shift = match (&self.prev, region) {
            (Some(prev), Some(region))
                if self.native_scroll
                    && prev.area == next.area
                    // DECSTBM + SU/SD moves whole terminal rows. A partial
                    // width rectangle cannot use this optimization, regardless
                    // of what widget occupies the remaining columns.
                    && region.x == area.x
                    && region.width == area.width =>
            {
                detect_shift(prev, &next, region)
            }
            _ => None,
        };
        if let Some((shift, region)) = shift {
            encoder.scroll_region(region, shift);
            let (top, exposed): (u16, u16) = match shift {
                Shift::Up(n) => (region.bottom() - n, n),
                Shift::Down(n) => (region.y, n),
            };
            for y in top..top + exposed {
                encoder.row(&next, y, region.x, region.width);
            }
            // Outside the region every diff cell is encoded; `Buffer::diff`
            // already removed skipped and wide-char-covered cells.
            for (x, y, cell) in &diff {
                if !(y >= &region.y && y < &(region.y + region.height)) {
                    encoder.cell(*x, *y, cell);
                }
            }
        } else {
            for (x, y, cell) in &diff {
                encoder.cell(*x, *y, cell);
            }
        }
        encoder.finish();
        self.publish(next)?;
        self.writer.write_all(&out)
    }
    fn hide_cursor(&mut self) -> io::Result<()> {
        queue!(self.writer, crossterm::cursor::Hide)
    }
    fn show_cursor(&mut self) -> io::Result<()> {
        queue!(self.writer, crossterm::cursor::Show)
    }
    fn get_cursor_position(&mut self) -> io::Result<Position> {
        let (column, row) = crossterm::cursor::position()?;
        Ok(Position::new(column, row))
    }
    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let position = position.into();
        queue!(
            self.writer,
            crossterm::cursor::MoveTo(position.x, position.y)
        )
    }
    fn clear(&mut self) -> io::Result<()> {
        self.prev = None;
        queue!(
            self.writer,
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All)
        )
    }
    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.prev = None;
        let crossterm_clear = match clear_type {
            ClearType::All => crossterm::terminal::ClearType::All,
            ClearType::AfterCursor => crossterm::terminal::ClearType::FromCursorDown,
            ClearType::BeforeCursor => crossterm::terminal::ClearType::FromCursorUp,
            ClearType::CurrentLine => crossterm::terminal::ClearType::CurrentLine,
            ClearType::UntilNewLine => crossterm::terminal::ClearType::UntilNewLine,
        };
        queue!(self.writer, crossterm::terminal::Clear(crossterm_clear))
    }
    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.prev = None;
        let newlines = "\n".repeat(usize::from(n));
        queue!(self.writer, crossterm::style::Print(newlines))
    }
    fn size(&self) -> io::Result<Size> {
        if let Some(size) = stdio_window_size() {
            return Ok(size.columns_rows);
        }
        // Same fallback the stock crossterm backend provides: prefer the
        // controlling terminal when our own stdio is not a tty.
        crossterm::terminal::size()
            .map(|(width, height)| Size::new(width, height))
            .map_err(|e| io::Error::other(e.to_string()))
    }
    fn window_size(&mut self) -> io::Result<WindowSize> {
        stdio_window_size().ok_or_else(|| io::Error::other("window size unavailable without a tty"))
    }
    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

/// A pure shift requires every retained row to be identical; the exposed rows
/// are free content. Small shifts are checked first: wheel steps, streaming
/// appends and the shrink of a collapsed group all move few lines.
fn detect_shift(prev: &Buffer, next: &Buffer, region: Rect) -> Option<(Shift, Rect)> {
    let rows_equal = |a: &Buffer, y1: u16, b: &Buffer, y2: u16| -> bool {
        if y1 >= a.area.height || y2 >= b.area.height {
            return false;
        }
        let stride = a.area.width as usize;
        let start = y1 as usize * stride + region.x as usize;
        let other = y2 as usize * b.area.width as usize + region.x as usize;
        let width = region.width as usize;
        a.content[start..start + width] == b.content[other..other + width]
    };
    let bottom = region.y + region.height;
    // An unchanged region must not "shift": identical blank rows satisfy any
    // shift pattern vacuously, and scrolling them would flicker.
    if !(region.y..bottom).any(|y| !rows_equal(prev, y, next, y)) {
        return None;
    }
    for n in 1..region.height {
        // Content moved up: next row y repeats prev row y + n.
        if rows_equal(next, region.y, prev, region.y + n)
            && (region.y + 1..bottom - n).all(|y| rows_equal(next, y, prev, y + n))
        {
            return Some((Shift::Up(n), region));
        }
        // Content moved down: next row y + n repeats prev row y.
        if rows_equal(next, region.y + n, prev, region.y)
            && (region.y + 1..bottom - n).all(|y| rows_equal(next, y + n, prev, y))
        {
            return Some((Shift::Down(n), region));
        }
    }
    None
}

/// Minimal cell encoder: batches cursor moves and tracks fg, bg and modifiers
/// independently so a token recolor does not resend an unchanged background.
struct Encoder<'a> {
    out: &'a mut Vec<u8>,
    fg: Color,
    bg: Color,
    modifier: Modifier,
    last_pos: Option<Position>,
}
impl Encoder<'_> {
    fn new(out: &mut Vec<u8>) -> Encoder<'_> {
        Encoder {
            out,
            fg: Color::Reset,
            bg: Color::Reset,
            modifier: Modifier::empty(),
            last_pos: None,
        }
    }
    fn scroll_region(&mut self, region: Rect, shift: Shift) {
        let (op, n) = match shift {
            Shift::Up(n) => ("S", n),
            Shift::Down(n) => ("T", n),
        };
        let _ = write!(
            self.out,
            "\x1b[{};{}r\x1b[{};1H\x1b[{}{op}\x1b[r",
            region.y + 1,
            region.y + region.height,
            region.y + 1,
            n
        );
        self.last_pos = None;
        self.fg = Color::Reset;
        self.bg = Color::Reset;
        self.modifier = Modifier::empty();
    }
    /// Paint one full row of `buffer`; cells covered by a preceding multi-width
    /// symbol are skipped exactly like `Buffer::diff` does.
    fn row(&mut self, buffer: &Buffer, y: u16, x: u16, width: u16) {
        let mut to_skip = 0usize;
        for column in x..x + width {
            if to_skip > 0 {
                to_skip -= 1;
                continue;
            }
            if let Some(cell) = buffer.cell(Position::new(column, y)) {
                if cell.skip {
                    continue;
                }
                self.cell(column, y, cell);
                to_skip = cell.symbol().width().saturating_sub(1);
            }
        }
    }
    fn cell(&mut self, x: u16, y: u16, cell: &Cell) {
        if !matches!(self.last_pos, Some(p) if x == p.x + 1 && y == p.y) {
            let _ = write!(self.out, "\x1b[{};{}H", y + 1, x + 1);
        }
        self.last_pos = Some(Position::new(x, y));
        if cell.modifier != self.modifier {
            self.modifier_diff(cell.modifier);
            self.modifier = cell.modifier;
        }
        if cell.fg != self.fg {
            self.color(cell.fg, true);
            self.fg = cell.fg;
        }
        if cell.bg != self.bg {
            self.color(cell.bg, false);
            self.bg = cell.bg;
        }
        let _ = self.out.write_all(cell.symbol().as_bytes());
    }
    fn color(&mut self, color: Color, foreground: bool) {
        let code = match color {
            Color::Reset => (if foreground { 39 } else { 49 }).to_string(),
            Color::Rgb(r, g, b) => {
                format!("{};2;{r};{g};{b}", if foreground { 38 } else { 48 })
            }
            Color::Indexed(i) => format!("{};5;{i}", if foreground { 38 } else { 48 }),
            Color::Black => ansi(foreground, 30, 40),
            Color::Red => ansi(foreground, 31, 41),
            Color::Green => ansi(foreground, 32, 42),
            Color::Yellow => ansi(foreground, 33, 43),
            Color::Blue => ansi(foreground, 34, 44),
            Color::Magenta => ansi(foreground, 35, 45),
            Color::Cyan => ansi(foreground, 36, 46),
            Color::Gray => ansi(foreground, 37, 47),
            Color::DarkGray => ansi(foreground, 90, 100),
            Color::LightRed => ansi(foreground, 91, 101),
            Color::LightGreen => ansi(foreground, 92, 102),
            Color::LightYellow => ansi(foreground, 93, 103),
            Color::LightBlue => ansi(foreground, 94, 104),
            Color::LightMagenta => ansi(foreground, 95, 105),
            Color::LightCyan => ansi(foreground, 96, 106),
            Color::White => ansi(foreground, 97, 107),
        };
        let _ = write!(self.out, "\x1b[{code}m");
    }
    fn modifier_diff(&mut self, to: Modifier) {
        let from = self.modifier;
        // Rows sharing a reset code must stay adjacent: BOLD/DIM → 22,
        // the blink pair → 25, so a shared reset is emitted once.
        const MODIFIER_SGR: &[(Modifier, &[u8], &[u8])] = &[
            (Modifier::BOLD, b"\x1b[22m", b"\x1b[1m"),
            (Modifier::DIM, b"\x1b[22m", b"\x1b[2m"),
            (Modifier::ITALIC, b"\x1b[23m", b"\x1b[3m"),
            (Modifier::UNDERLINED, b"\x1b[24m", b"\x1b[4m"),
            (Modifier::SLOW_BLINK, b"\x1b[25m", b"\x1b[5m"),
            (Modifier::RAPID_BLINK, b"\x1b[25m", b"\x1b[6m"),
            (Modifier::REVERSED, b"\x1b[27m", b"\x1b[7m"),
            (Modifier::HIDDEN, b"\x1b[28m", b"\x1b[8m"),
            (Modifier::CROSSED_OUT, b"\x1b[29m", b"\x1b[9m"),
        ];
        let mut last_reset: Option<&[u8]> = None;
        for &(bit, reset, _) in MODIFIER_SGR {
            if from.contains(bit) && !to.contains(bit) && Some(reset) != last_reset {
                self.out.extend_from_slice(reset);
                last_reset = Some(reset);
            }
        }
        for &(bit, _, set) in MODIFIER_SGR {
            if to.contains(bit) && !from.contains(bit) {
                self.out.extend_from_slice(set);
            }
        }
    }
    fn finish(&mut self) {
        self.out.extend_from_slice(b"\x1b[39m\x1b[49m\x1b[0m");
    }
}
/// Named colors map to fixed SGR codes: dim 30–37/40–47, bright 90–97/100–107.
fn ansi(foreground: bool, fg_code: u8, bg_code: u8) -> String {
    (if foreground { fg_code } else { bg_code }).to_string()
}
/// tmux re-renders panes from its own grid; native scroll regions only mark
/// the pane fully dirty there. Both the escape hatch and TERM work under SSH.
pub(crate) fn inside_tmux() -> bool {
    std::env::var_os("TMUX").is_some()
        || std::env::var("TERM").is_ok_and(|term| term.contains("tmux"))
}

/// Buffers each frame and wraps it in synchronized-output markers so the
/// terminal paints the whole frame atomically instead of ~1KB LineWriter
/// chunks (which tear while streaming). Unknown private modes are ignored
/// by terminals without synchronized-output support.
pub(crate) struct SyncWriter {
    inner: io::Stdout,
    buffer: Vec<u8>,
    /// Whether frames are wrapped in `\x1b[?2026h/l`. See [`SyncWriter::new`].
    wrapped: bool,
}
impl SyncWriter {
    pub(crate) fn new() -> Self {
        // `YOURAI_TUI_SYNC=on|off` forces the choice for measurements or
        // terminals with known-better behavior.
        let forced = std::env::var_os("YOURAI_TUI_SYNC")
            .as_deref()
            .and_then(|v| match v.to_str() {
                Some("on") => Some(true),
                Some("off") => Some(false),
                _ => None,
            });
        Self {
            inner: io::stdout(),
            buffer: Vec::with_capacity(1 << 16),
            // Synchronized output prevents tearing on direct terminal
            // connections. Inside a mux it backfires: tmux defers the pane
            // update and then re-renders the client from scratch, ~6x the
            // incremental client bytes of an unwrapped frame (measured
            // 8.97KB vs 1.47KB per scroll frame). Frames are written in a
            // single write() either way, so the mux still applies them
            // atomically without the wrapper.
            wrapped: forced.unwrap_or_else(|| !inside_tmux()),
        }
    }
}
impl Write for SyncWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return self.inner.flush();
        }
        if !self.wrapped {
            let frame = std::mem::take(&mut self.buffer);
            self.inner.write_all(&frame)?;
            return self.inner.flush();
        }
        let mut frame = Vec::with_capacity(self.buffer.len() + 16);
        frame.extend_from_slice(b"\x1b[?2026h");
        frame.append(&mut self.buffer);
        frame.extend_from_slice(b"\x1b[?2026l");
        self.inner.write_all(&frame)?;
        self.inner.flush()
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
    #[allow(clippy::wildcard_imports)]
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

    /// Collects the ANSI stream a ScrollBackend emits, frame by frame.
    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);
    impl SharedBuf {
        fn take(&self) -> Vec<u8> {
            std::mem::take(&mut *self.0.lock().unwrap())
        }
    }
    impl io::Write for SharedBuf {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Minimal VT emulator: cursor addressing, scroll regions, SU/SD and
    /// printable text. SGR and private modes are ignored.
    struct Vt {
        grid: Vec<Vec<char>>,
        x: usize,
        y: usize,
        top: usize,
        bottom: usize,
    }
    impl Vt {
        fn new(width: usize, height: usize) -> Self {
            Self {
                grid: vec![vec![' '; width]; height],
                x: 0,
                y: 0,
                top: 1,
                bottom: height,
            }
        }
        fn feed(&mut self, bytes: &[u8]) {
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] != 0x1b {
                    if self.y < self.grid.len() && self.x < self.grid[0].len() {
                        self.grid[self.y][self.x] = bytes[i] as char;
                    }
                    self.x += 1;
                    i += 1;
                    continue;
                }
                if bytes.get(i + 1) != Some(&b'[') {
                    i += 2;
                    continue;
                }
                let params: Vec<u8> = bytes[i + 2..]
                    .iter()
                    .take_while(|b| b.is_ascii_digit() || **b == b';' || **b == b'?')
                    .copied()
                    .collect();
                let final_byte = bytes[i + 2 + params.len()];
                let numbers: Vec<usize> = std::str::from_utf8(&params)
                    .unwrap_or_default()
                    .split(';')
                    .map(|p| p.parse::<usize>().unwrap_or(0))
                    .collect();
                let param = |index: usize, default: usize| {
                    numbers
                        .get(index)
                        .filter(|n| **n > 0)
                        .copied()
                        .unwrap_or(default)
                };
                match final_byte {
                    b'H' => {
                        self.y = param(0, 1) - 1;
                        self.x = param(1, 1) - 1;
                    }
                    b'r' => {
                        self.top = param(0, 1);
                        self.bottom = param(1, self.grid.len());
                    }
                    b'S' => self.scroll(param(0, 1), true),
                    b'T' => self.scroll(param(0, 1), false),
                    _ => {}
                }
                i += 2 + params.len() + 1;
            }
        }
        fn scroll(&mut self, n: usize, up: bool) {
            let (top, bottom) = (self.top - 1, self.bottom);
            let width = self.grid[0].len();
            let region = self.grid[top..bottom].to_vec();
            let blank = vec![' '; width];
            for (offset, row) in self
                .grid
                .iter_mut()
                .enumerate()
                .skip(top)
                .take(bottom - top)
            {
                let relative = offset - top;
                let source = if up {
                    Some(relative + n)
                } else {
                    relative.checked_sub(n)
                };
                *row = source
                    .and_then(|index| region.get(index))
                    .unwrap_or(&blank)
                    .clone();
            }
        }
        fn text(&self) -> Vec<String> {
            self.grid.iter().map(|row| row.iter().collect()).collect()
        }
    }

    /// Paint every cell so the diff extents always cover the whole area.
    fn paint(buffer: &mut Buffer, y: u16, text: &str, fg: Color) {
        let width = buffer.area.width as usize;
        let padded = format!("{text:<width$}");
        buffer.set_string(
            0,
            y,
            padded,
            Style::default().fg(fg).bg(Color::Rgb(1, 2, 3)),
        );
    }
    fn screen(rows: &[&str]) -> Buffer {
        let mut buffer = Buffer::empty(Rect::new(0, 0, rows[0].len() as u16, rows.len() as u16));
        for (y, row) in rows.iter().enumerate() {
            // Identical styling across rows: row equality must come from content.
            paint(&mut buffer, y as u16, row, Color::Rgb(9, 9, 9));
        }
        buffer
    }
    fn decoded(buffer: &Buffer) -> Vec<String> {
        buffer
            .content
            .chunks(buffer.area.width as usize)
            .map(|row| {
                row.iter()
                    .map(|cell| cell.symbol().chars().next().unwrap_or(' '))
                    .collect()
            })
            .collect()
    }

    type Scroll = ScrollBackend<SharedBuf>;
    fn drive(backend: &mut Scroll, prev: &Buffer, next: &Buffer) -> Vec<u8> {
        let diff = prev.diff(next);
        backend.draw(diff.into_iter()).unwrap();
        let out = backend.writer_shared().take();
        assert!(!out.is_empty(), "frame must emit output");
        out
    }

    impl ScrollBackend<SharedBuf> {
        fn writer_shared(&self) -> &SharedBuf {
            &self.writer
        }
    }

    #[test]
    fn pure_scroll_up_repaints_only_the_new_bottom_row() {
        let region = Rect::new(0, 0, 10, 3);
        let shared = SharedBuf::default();
        let mut backend = ScrollBackend::new(shared.clone(), Arc::new(Mutex::new(Some(region))));
        backend.test_area = Some(Rect::new(0, 0, 10, 4));
        backend.test_set_native_scroll(true);
        let first = screen(&["AAAAAAAAAA", "BBBBBBBBBB", "CCCCCCCCCC", "footer----"]);
        let empty = Buffer::empty(first.area);
        backend.draw(empty.diff(&first).into_iter()).unwrap();
        shared.take();

        let scrolled = screen(&["BBBBBBBBBB", "CCCCCCCCCC", "DDDDDDDDDD", "footer++++"]);
        let out = drive(&mut backend, &first, &scrolled);

        assert!(
            out.windows(6).any(|w| w == b"\x1b[1;3r"),
            "sets region: {out:?}"
        );
        assert!(
            out.windows(4).any(|w| w == b"\x1b[1S"),
            "scrolls up: {out:?}"
        );
        assert!(
            out.windows(3).any(|w| w == b"\x1b[r"),
            "resets region: {out:?}"
        );
        assert!(
            !out.windows(8).any(|w| w == b"AAAAAAAA"),
            "old rows are not repainted: {out:?}"
        );
        assert!(
            out.len() < 140,
            "a shifted frame costs a row, not a screen: {} bytes",
            out.len()
        );

        let mut vt = Vt::new(10, 4);
        for (y, row) in decoded(&first).iter().enumerate() {
            vt.feed(format!("\x1b[{};1H{row}", y + 1).as_bytes());
        }
        vt.feed(&out);
        assert_eq!(vt.text(), decoded(&scrolled));
    }

    #[test]
    fn pure_scroll_down_bring_back_history() {
        let region = Rect::new(0, 0, 10, 3);
        let shared = SharedBuf::default();
        let mut backend = ScrollBackend::new(shared.clone(), Arc::new(Mutex::new(Some(region))));
        backend.test_area = Some(Rect::new(0, 0, 10, 4));
        backend.test_set_native_scroll(true);
        let first = screen(&["BBBBBBBBBB", "CCCCCCCCCC", "DDDDDDDDDD", "footer----"]);
        let empty = Buffer::empty(first.area);
        backend.draw(empty.diff(&first).into_iter()).unwrap();
        shared.take();

        let scrolled = screen(&["ZZZZZZZZZZ", "BBBBBBBBBB", "CCCCCCCCCC", "footer----"]);
        let out = drive(&mut backend, &first, &scrolled);

        assert!(
            out.windows(4).any(|w| w == b"\x1b[1T"),
            "scrolls down: {out:?}"
        );
        assert!(
            !out.windows(8).any(|w| w == b"BBBBBBBB"),
            "retained rows not repainted: {out:?}"
        );

        let mut vt = Vt::new(10, 4);
        for (y, row) in decoded(&first).iter().enumerate() {
            vt.feed(format!("\x1b[{};1H{row}", y + 1).as_bytes());
        }
        vt.feed(&out);
        assert_eq!(vt.text(), decoded(&scrolled));
    }

    #[test]
    fn in_region_edit_disables_the_scroll_shortcut() {
        let region = Rect::new(0, 0, 10, 3);
        let shared = SharedBuf::default();
        let mut backend = ScrollBackend::new(shared.clone(), Arc::new(Mutex::new(Some(region))));
        backend.test_area = Some(Rect::new(0, 0, 10, 4));
        backend.test_set_native_scroll(true);
        let first = screen(&["AAAAAAAAAA", "BBBBBBBBBB", "CCCCCCCCCC", "footer----"]);
        let empty = Buffer::empty(first.area);
        backend.draw(empty.diff(&first).into_iter()).unwrap();
        shared.take();

        let edited = screen(&["AAAAAAAAAA", "BXBBBBBBBB", "CCCCCCCCCC", "footer----"]);
        let out = drive(&mut backend, &first, &edited);
        assert!(
            !out.windows(4).any(|w| w == b"\x1b[1S"),
            "no shortcut: {out:?}"
        );
        assert!(
            !out.windows(4).any(|w| w == b"\x1b[1T"),
            "no shortcut: {out:?}"
        );

        let mut vt = Vt::new(10, 4);
        for (y, row) in decoded(&first).iter().enumerate() {
            vt.feed(format!("\x1b[{};1H{row}", y + 1).as_bytes());
        }
        vt.feed(&out);
        assert_eq!(vt.text(), decoded(&edited));
    }

    #[test]
    fn changed_background_is_not_resent_for_every_foreground_token() {
        let region = Rect::new(0, 0, 0, 0);
        let shared = SharedBuf::default();
        let mut backend = ScrollBackend::new(shared.clone(), Arc::new(Mutex::new(Some(region))));
        backend.test_area = Some(Rect::new(0, 0, 10, 1));
        backend.test_set_native_scroll(true);
        let mut first = screen(&["AAAAAAAAAA"]);
        first.set_style(
            Rect::new(0, 0, 10, 1),
            Style::default().fg(Color::Reset).bg(Color::Rgb(9, 9, 9)),
        );
        let empty = Buffer::empty(first.area);
        backend.draw(empty.diff(&first).into_iter()).unwrap();
        shared.take();

        let mut next = first.clone();
        next.set_string(
            0,
            0,
            "rG",
            Style::default()
                .fg(Color::Rgb(1, 1, 1))
                .bg(Color::Rgb(9, 9, 9)),
        );
        let out = drive(&mut backend, &first, &next);
        let bg = b"\x1b[48;2;9;9;9m";
        assert_eq!(
            out.windows(bg.len()).filter(|w| *w == bg).count(),
            1,
            "unchanged background is emitted once: {out:?}"
        );
    }

    #[test]
    fn modifier_transitions_share_reset_codes() {
        let transition = |from: Modifier, to: Modifier| {
            let mut out = Vec::new();
            let mut encoder = Encoder::new(&mut out);
            encoder.modifier = from;
            encoder.modifier_diff(to);
            out
        };
        // Adding never resets.
        assert_eq!(
            transition(Modifier::empty(), Modifier::BOLD | Modifier::DIM),
            b"\x1b[1m\x1b[2m"
        );
        // BOLD and DIM share reset 22; removing both emits it once, not twice.
        assert_eq!(
            transition(Modifier::BOLD | Modifier::DIM, Modifier::empty()),
            b"\x1b[22m"
        );
        // The blink pair shares 25 the same way.
        assert_eq!(
            transition(
                Modifier::SLOW_BLINK | Modifier::RAPID_BLINK,
                Modifier::empty()
            ),
            b"\x1b[25m"
        );
        // A swap resets the old bit and sets the new one.
        assert_eq!(
            transition(Modifier::BOLD, Modifier::ITALIC),
            b"\x1b[22m\x1b[3m"
        );
    }

    #[test]
    fn shift_detection_requires_the_region_to_be_inside_the_screen() {
        let next = screen(&["AAAA", "BBBB"]);
        let prev = screen(&["BBBB", "AAAA"]);
        // Region taller than the screen: no shift must be detected.
        assert_eq!(detect_shift(&prev, &next, Rect::new(0, 0, 4, 3)), None);
        // A blank region never "shifts" because nothing changes.
        let blank = Buffer::empty(Rect::new(0, 0, 4, 2));
        assert_eq!(detect_shift(&blank, &blank, Rect::new(0, 0, 4, 2)), None);
    }
    #[test]
    fn partial_width_scroll_preserves_every_cell_outside_the_transcript() {
        for native in [false, true] {
            for region in [Rect::new(0, 0, 4, 3), Rect::new(2, 0, 4, 3)] {
                let shared = SharedBuf::default();
                let mut backend =
                    ScrollBackend::new(shared.clone(), Arc::new(Mutex::new(Some(region))));
                backend.test_area = Some(Rect::new(0, 0, 8, 4));
                backend.test_set_native_scroll(native);
                let first = screen(&["AAAAAAAA", "BBBBBBBB", "CCCCCCCC", "footer--"]);
                let mut next = first.clone();
                for (y, text) in ["BBBB", "CCCC", "DDDD"].iter().enumerate() {
                    next.set_string(
                        region.x,
                        y as u16,
                        text,
                        first[(region.x, y as u16)].style(),
                    );
                }
                let mut vt = Vt::new(8, 4);
                backend
                    .draw(Buffer::empty(first.area).diff(&first).into_iter())
                    .unwrap();
                vt.feed(&shared.take());
                vt.feed(&drive(&mut backend, &first, &next));
                assert_eq!(
                    vt.text(),
                    decoded(&next),
                    "native={native}, region={region:?}"
                );
                vt.feed(&drive(&mut backend, &next, &first));
                assert_eq!(
                    vt.text(),
                    decoded(&first),
                    "roundtrip native={native}, region={region:?}"
                );
            }
        }
    }
}

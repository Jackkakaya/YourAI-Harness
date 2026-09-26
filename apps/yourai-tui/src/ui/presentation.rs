//! Prepare a complete screen before deciding whether terminal I/O is necessary.
//! The screen and cursor are the invalidation contract: callers never maintain
//! a second list of the model fields that happen to affect rendering.
use crate::terminal::SharedRegion;
use ratatui::{
    backend::Backend,
    buffer::Buffer,
    layout::{Position, Rect},
    widgets::Widget,
    Frame, Terminal,
};
use std::{
    io,
    time::{Duration, Instant},
};

pub(super) struct Canvas {
    buffer: Buffer,
    cursor: Option<Position>,
    /// The native-scroll window for this frame, published to the terminal
    /// backend before draw. Not part of the visual comparison: identical
    /// pixels owe no I/O even when the window moved.
    scroll: Option<Rect>,
}
// The screen and cursor are the contract; the scroll window is backend
// configuration, not pixels.
impl PartialEq for Canvas {
    fn eq(&self, other: &Self) -> bool {
        self.buffer == other.buffer && self.cursor == other.cursor
    }
}
impl Canvas {
    pub fn new(area: Rect) -> Self {
        Self {
            buffer: Buffer::empty(area),
            cursor: None,
            scroll: None,
        }
    }
    pub fn area(&self) -> Rect {
        self.buffer.area
    }
    pub fn buffer_mut(&mut self) -> &mut Buffer {
        &mut self.buffer
    }
    pub fn render_widget(&mut self, widget: impl Widget, area: Rect) {
        widget.render(area, &mut self.buffer);
    }
    pub fn set_cursor_position(&mut self, position: impl Into<Position>) {
        self.cursor = Some(position.into());
    }
    pub fn set_scroll_region(&mut self, region: Rect) {
        self.scroll = Some(region);
    }
    /// Submission only copies prepared pixels; no model mutation or layout here.
    pub fn paint(&self, frame: &mut Frame<'_>) {
        frame.buffer_mut().clone_from(&self.buffer);
        if let Some(cursor) = self.cursor {
            frame.set_cursor_position(cursor);
        }
    }
}

pub(super) struct Presentation {
    painted: Option<Canvas>,
    last_write: Instant,
    pending: bool,
}
impl Default for Presentation {
    fn default() -> Self {
        Self {
            painted: None,
            last_write: Instant::now() - Self::INTERVAL,
            pending: true,
        }
    }
}
impl Presentation {
    const INTERVAL: Duration = Duration::from_millis(12);
    pub fn request(&mut self) {
        self.pending = true;
    }
    pub fn pending(&self) -> bool {
        self.pending
    }
    pub fn deadline(&self) -> tokio::time::Instant {
        (self.last_write + Self::INTERVAL).into()
    }
    pub fn flush<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        now: Instant,
        scroll: &SharedRegion,
        prepare: impl FnOnce(Rect) -> Canvas,
    ) -> io::Result<bool> {
        if !self.pending || now.saturating_duration_since(self.last_write) < Self::INTERVAL {
            return Ok(false);
        }
        // Resize first, even when there are no model changes. The backend must
        // discard its physical-screen model before comparing differently sized frames.
        terminal.autoresize()?;
        let canvas = prepare(terminal.get_frame().area());
        self.pending = false;
        // Publish the frame's native-scroll window before draw; the backend
        // reads it while encoding shifts. None leaves the previous window.
        if let Some(region) = canvas.scroll {
            *scroll
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(region);
        }
        if self.painted.as_ref() == Some(&canvas) {
            return Ok(false);
        }
        terminal.draw(|frame| canvas.paint(frame))?;
        self.painted = Some(canvas);
        self.last_write = now;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::{Canvas, Presentation};
    use crate::terminal::SharedRegion;
    use crate::ui::frame_time::FrameTime;
    use crate::ui::{
        render::{Metadata, Renderer},
        state::View,
    };
    use ratatui::{
        backend::TestBackend,
        layout::Rect,
        style::{Color, Style},
        widgets::Paragraph,
        Terminal,
    };
    use std::{cell::Cell, sync::Arc, time::Instant};

    fn region() -> SharedRegion {
        Arc::new(std::sync::Mutex::new(None))
    }

    #[test]
    fn complete_screen_and_cursor_determine_presentation_not_model_field_lists() {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut presentation = Presentation::default();
        let scroll = region();
        let mut now = Instant::now();
        let mut renderer = Renderer::default();
        let mut view = View::default();
        let mut meta = Metadata {
            session: "test".into(),
            cwd: "/tmp".into(),
            trusted_shell: false,
            yolo: false,
        };
        let mut show = |view: &mut View, renderer: &mut Renderer, meta: &Metadata| {
            now += Presentation::INTERVAL;
            presentation.request();
            presentation
                .flush(&mut terminal, now, &scroll, |area| {
                    renderer.prepare(
                        area,
                        view,
                        meta,
                        FrameTime {
                            monotonic: now,
                            unix_seconds: 0,
                        },
                        0,
                        false,
                    )
                })
                .unwrap()
        };
        assert!(show(&mut view, &mut renderer, &meta));
        assert!(!show(&mut view, &mut renderer, &meta));
        view.editor.insert("/");
        assert!(show(&mut view, &mut renderer, &meta));
        view.menu().step(false);
        assert!(show(&mut view, &mut renderer, &meta));
        view.menu().dismiss();
        assert!(show(&mut view, &mut renderer, &meta));
        assert!(!show(&mut view, &mut renderer, &meta));
        // Moving the cursor changes no text or cell styles.
        view.editor.left();
        assert!(show(&mut view, &mut renderer, &meta));
        // Metadata is outside View and is deliberately not registered anywhere.
        meta.yolo = true;
        assert!(show(&mut view, &mut renderer, &meta));
        assert!(!show(&mut view, &mut renderer, &meta));
    }

    #[test]
    fn flush_publishes_the_scroll_window_and_none_leaves_it_untouched() {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut presentation = Presentation::default();
        let scroll = region();
        let now = Instant::now();
        let prepare = |area: Rect, window: Option<Rect>| {
            let mut canvas = Canvas::new(area);
            canvas.scroll = window;
            canvas
        };
        presentation
            .flush(&mut terminal, now, &scroll, |a| {
                prepare(a, Some(Rect::new(0, 1, 80, 20)))
            })
            .unwrap();
        assert_eq!(*scroll.lock().unwrap(), Some(Rect::new(0, 1, 80, 20)));
        // A frame without a window leaves the previous one in place, and the
        // publish happens even when the pixels compare equal (no draw).
        presentation.request();
        assert!(!presentation
            .flush(&mut terminal, now + Presentation::INTERVAL, &scroll, |a| {
                prepare(a, None)
            })
            .unwrap());
        assert_eq!(*scroll.lock().unwrap(), Some(Rect::new(0, 1, 80, 20)));
        // A moved window is published even though the empty pixels are equal.
        presentation.request();
        assert!(!presentation
            .flush(
                &mut terminal,
                now + Presentation::INTERVAL * 2,
                &scroll,
                |a| prepare(a, Some(Rect::new(0, 2, 80, 18)))
            )
            .unwrap());
        assert_eq!(*scroll.lock().unwrap(), Some(Rect::new(0, 2, 80, 18)));
    }

    #[test]
    fn paced_changes_coalesce_without_losing_the_latest_frame_or_resize() {
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        let mut presentation = Presentation::default();
        let scroll = region();
        let now = Instant::now();
        let calls = Cell::new(0);
        let prepare = |area: Rect, text: &str, color| {
            calls.set(calls.get() + 1);
            let mut canvas = Canvas::new(area);
            canvas.render_widget(Paragraph::new(text).style(Style::default().fg(color)), area);
            canvas
        };
        assert!(presentation
            .flush(&mut terminal, now, &scroll, |r| prepare(
                r,
                "first",
                Color::Red
            ))
            .unwrap());
        presentation.request();
        assert!(!presentation
            .flush(&mut terminal, now, &scroll, |r| prepare(
                r,
                "intermediate",
                Color::Red
            ))
            .unwrap());
        assert!(presentation.pending());
        assert_eq!(calls.get(), 1, "do not run layout while the frame is paced");
        let later = now + Presentation::INTERVAL;
        assert!(presentation
            .flush(&mut terminal, later, &scroll, |r| prepare(
                r,
                "latest",
                Color::Red
            ))
            .unwrap());
        assert!(!presentation.pending());
        assert_eq!(terminal.backend().buffer()[(0, 0)].symbol(), "l");
        presentation.request();
        assert!(presentation
            .flush(
                &mut terminal,
                later + Presentation::INTERVAL,
                &scroll,
                |r| { prepare(r, "latest", Color::Blue) }
            )
            .unwrap());
        terminal.backend_mut().resize(35, 10);
        presentation.request();
        assert!(presentation
            .flush(
                &mut terminal,
                later + Presentation::INTERVAL * 2,
                &scroll,
                |r| { prepare(r, "latest", Color::Blue) }
            )
            .unwrap());
        assert_eq!(terminal.backend().buffer().area, Rect::new(0, 0, 35, 10));
        presentation.request();
        assert!(!presentation
            .flush(
                &mut terminal,
                later + Presentation::INTERVAL * 3,
                &scroll,
                |r| { prepare(r, "latest", Color::Blue) }
            )
            .unwrap());
    }
}

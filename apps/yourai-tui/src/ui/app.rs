//! The action layer: one terminal event in, every state mutation out.
//! ui.rs keeps wake policy, burst coalescing and terminal I/O; event
//! interpretation and its side effects live here so the composition
//! (input → state change → repaint) is testable without a PTY.
use super::{
    clipboard, commands,
    frame_time::FrameTime,
    overlay::{Action as OverlayAction, Overlay},
    presentation::Canvas,
    render::{Hit, Metadata, Renderer},
    session::{Controller, Reply, Target},
    state::{self, View},
    theme,
};
use crate::config::Error;
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::prelude::{Position, Rect};
use std::time::Instant;
use tokio::task::JoinHandle;
use yourai_core::prelude::*;

/// What the loop should do after one event. Quit is the only control flow
/// the loop cannot express locally: it drops the rest of the burst, skips
/// the frame tail and returns from the UI future.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Flow {
    Continue,
    Quit,
}

pub(super) struct App {
    controller: Controller,
    view: View,
    meta: Metadata,
    renderer: Renderer,
    clipboard_task: Option<JoinHandle<std::io::Result<()>>>,
    /// The TaskBoard version already reflected in `view.session.todos`; None means
    /// the next sync must run (also reset on session switch).
    synced_todos: Option<u64>,
}

impl App {
    pub(super) fn new(controller: Controller, mut view: View, meta: Metadata) -> Self {
        view.draft.set_cwd(&controller.harness().host.context().cwd);
        Self {
            renderer: Renderer::default(),
            controller,
            view,
            meta,
            clipboard_task: None,
            synced_todos: None,
        }
    }

    /// The session-output channel, lent to the loop's `select!`.
    pub(super) async fn recv(&mut self) -> Option<Out> {
        self.controller.recv().await
    }

    /// Reconcile one streaming output and any finished session operation.
    /// On a completed switch, refresh header metadata and rebuild the
    /// renderer. Runs once per wake, before the event batch: the harness
    /// identity cannot change mid-burst.
    pub(super) async fn poll(&mut self, first: Option<Out>) {
        if self.controller.poll(&mut self.view, first).await {
            let context = self.controller.harness().host.context();
            self.meta.session = context.id.as_str().to_owned();
            self.meta.cwd = context.cwd.to_string_lossy().into();
            self.view.draft.set_cwd(&context.cwd);
            self.renderer = Renderer::default();
            self.synced_todos = None;
        }
    }

    /// Interpret one terminal event and apply every resulting mutation.
    /// Synchronous by construction: async work is only spawned here and
    /// reconciled by `poll` and the frame tail. The only output channels
    /// are field mutation, `tokio::spawn` and `Flow`.
    pub(super) fn handle(&mut self, event: Event) -> Flow {
        match event {
            Event::Resize(_, _) => Flow::Continue,
            Event::Mouse(e) if !self.view.overlay.is_open() => {
                self.mouse(e);
                Flow::Continue
            }
            Event::Mouse(_) => Flow::Continue,
            Event::Paste(text) => {
                self.paste(&text);
                Flow::Continue
            }
            Event::Key(key) if key.kind != KeyEventKind::Release => self.key(key),
            _ => Flow::Continue,
        }
    }

    // ───────────────────────── frame tail ─────────────────────────
    // One call per concern; none of them touches the terminal.

    /// Refresh the Todo sidebar from the harness task board, only when the
    /// board changed since the last sync.
    pub(super) fn sync_todos(&mut self) {
        let h = self.controller.harness();
        if let Some(tasks) = &h.tasks {
            let version = tasks.version();
            if self.synced_todos != Some(version) {
                self.synced_todos = Some(version);
                let todos = tasks
                    .list()
                    .into_iter()
                    .map(|t| state::Todo {
                        id: t.id,
                        text: crate::text::bounded(&t.subject),
                        completed: t.completed,
                    })
                    .collect();
                self.view.session.todos.set(todos);
            }
        }
    }

    /// Reap a finished clipboard copy into a toast.
    pub(super) async fn settle_clipboard(&mut self) {
        if self
            .clipboard_task
            .as_ref()
            .is_some_and(|t| t.is_finished())
        {
            let message = match self.clipboard_task.take().unwrap().await {
                Ok(Ok(())) => "✓ Copied".into(),
                Ok(Err(e)) => format!("Copy failed: {e}"),
                Err(e) => format!("Copy failed: {e}"),
            };
            self.view.toast = Some((message, Instant::now()));
        }
    }

    pub(super) async fn settle_draft(&mut self) {
        if let Some((level, message)) = self.view.draft.poll().await {
            self.view.notice(level, message);
        }
    }

    /// `(queued inputs, compacting)` for the status line.
    pub(super) fn pressure(&self) -> (usize, bool) {
        (
            self.controller.harness().host.queued(),
            self.controller.compacting(),
        )
    }

    /// Build the complete canvas for `area`. Hit regions recorded here are
    /// what the next mouse event resolves against.
    pub(super) fn paint(
        &mut self,
        area: Rect,
        time: FrameTime,
        queued: usize,
        compacting: bool,
    ) -> Canvas {
        // The permission mirror is refreshed from its owner every frame.
        self.meta.yolo = self.controller.yolo();
        self.renderer
            .prepare(area, &mut self.view, &self.meta, time, queued, compacting)
    }

    // ───────────────────────── shutdown ─────────────────────────

    /// Abort an in-flight clipboard copy, then close the session (finishing
    /// any pending operation rather than dropping its handle).
    pub(super) async fn close(mut self) -> Result<(String, Vec<In>), Error> {
        if let Some(task) = self.clipboard_task.take() {
            task.abort();
        }
        drop(self.view); // Cancel draft-owned work before waiting for session shutdown.
        self.controller.close().await
    }

    // ───────────────────────── input routing ─────────────────────────

    /// The key routing order is the behavior contract: quit → copy →
    /// selection escape → modal → command menu → bindings → editor.
    fn key(&mut self, key: KeyEvent) -> Flow {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if key.code == KeyCode::Char('q') && ctrl {
            return Flow::Quit;
        }
        if matches!(key.code, KeyCode::Char('c' | 'C'))
            && (alt || (ctrl && key.modifiers.contains(KeyModifiers::SHIFT)))
        {
            self.copy_selection();
            return Flow::Continue;
        }
        if key.code == KeyCode::Esc && self.renderer.selection.active() {
            self.renderer.selection.clear();
            return Flow::Continue;
        }
        self.renderer.selection.clear();
        if let Some(action) = self.view.overlay.key(key, self.view.model_choices.len()) {
            self.overlay_action(action);
            return Flow::Continue;
        }
        if self.view.asks_empty() && self.view.draft.intercept(key) {
            return Flow::Continue;
        }
        let commands = self.view.menu().items();
        if !commands.is_empty() && !ctrl && !alt {
            match key.code {
                KeyCode::Up => {
                    self.view.menu().step(true);
                    return Flow::Continue;
                }
                KeyCode::Down => {
                    self.view.menu().step(false);
                    return Flow::Continue;
                }
                KeyCode::Esc => {
                    self.view.menu().dismiss();
                    return Flow::Continue;
                }
                KeyCode::Tab | KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                    let command = commands[self.view.menu().selected.min(commands.len() - 1)];
                    self.view.draft.set_text("");
                    self.view.draft.insert(command.text);
                    if command.argument {
                        self.view.draft.insert(" ");
                    }
                    if key.code == KeyCode::Tab || command.argument {
                        return Flow::Continue;
                    }
                    self.view.menu().dismiss();
                }
                _ => {}
            }
        }
        self.binding(key)
    }

    /// Global bindings and the editor fallback. Reached only when no
    /// earlier consumer took the key.
    fn binding(&mut self, key: KeyEvent) -> Flow {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::F(1) => self.view.overlay = Overlay::Help { scroll: 0 },
            KeyCode::F(6) => {
                self.view
                    .select_next(key.modifiers.contains(KeyModifiers::SHIFT));
                self.view.session.navigation.reveal(self.view.selected());
            }
            KeyCode::Char('t') if ctrl => {
                self.view.session.todos.panel = !self.view.session.todos.panel;
            }
            KeyCode::Char('o') if ctrl => {
                self.view.toggle_recent(false);
                self.view.session.navigation.reveal(self.view.selected());
            }
            KeyCode::Char('r') if ctrl => {
                self.view.toggle_recent(true);
                self.view.session.navigation.reveal(self.view.selected());
            }
            KeyCode::Char('b') if ctrl => self.view.overlay = Overlay::Stats { scroll: 0 },
            KeyCode::Char('y') if ctrl => self.view.theme = self.view.theme.next(),
            KeyCode::Home if ctrl => self.renderer.latest_turn(&mut self.view),
            KeyCode::Up if ctrl => {
                self.renderer.jump_turn(&mut self.view, true);
            }
            KeyCode::Down if ctrl => {
                self.renderer.jump_turn(&mut self.view, false);
            }
            KeyCode::Char('g') if ctrl => {
                let enabled = !self.controller.yolo();
                self.set_yolo(enabled);
            }
            KeyCode::End if ctrl => {
                self.view.follow();
            }
            KeyCode::PageUp if alt && !self.view.asks_empty() => {
                if let Some(ask) = self.view.ask_mut() {
                    ask.scroll = ask.scroll.saturating_sub(5);
                }
            }
            KeyCode::PageDown if alt && !self.view.asks_empty() => {
                if let Some(ask) = self.view.ask_mut() {
                    ask.scroll = ask.scroll.saturating_add(5);
                }
            }
            KeyCode::PageUp if alt => self.view.session.todos.nudge(5, true),
            KeyCode::PageDown if alt => self.view.session.todos.nudge(5, false),
            KeyCode::PageUp => self.renderer.scroll(&mut self.view, 10, true),
            KeyCode::PageDown => self.renderer.scroll(&mut self.view, 10, false),
            KeyCode::Esc | KeyCode::Char('c') if key.code == KeyCode::Esc || ctrl => {
                self.controller.harness().host.interrupt();
                self.view.dismiss_asks();
                self.view.notice(Level::Info, "Cancellation requested.");
            }
            KeyCode::Enter
                if !key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) =>
            {
                return self.submit();
            }
            _ => {
                if let Some(ask) = self.view.ask_mut() {
                    ask.editor.key(key);
                } else {
                    self.view.draft.key(key);
                }
            }
        }
        Flow::Continue
    }

    fn mouse(&mut self, e: MouseEvent) {
        match e.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.renderer.begin_selection(e.column, e.row)
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.renderer.selection.drag(Position::new(e.column, e.row));
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.renderer.selection.drag(Position::new(e.column, e.row));
                if self.renderer.selection.active() {
                    self.copy_selection();
                } else {
                    self.renderer.selection.clear();
                    self.click_dispatch(e.column, e.row);
                }
            }
            MouseEventKind::ScrollUp => {
                self.renderer.selection.clear();
                self.wheel_dispatch(e.column, e.row, true);
            }
            MouseEventKind::ScrollDown => {
                self.renderer.selection.clear();
                self.wheel_dispatch(e.column, e.row, false);
            }
            _ => {}
        }
    }

    /// Paste routing: overlay query → pending ask → the draft editor. Empty
    /// bracketed paste usually means the clipboard holds an image (most
    /// terminals cannot paste images as text); best-effort image read so
    /// Ctrl+V also works when the terminal intercepts it.
    fn paste(&mut self, text: &str) {
        if self.view.overlay.is_open() {
            self.view.overlay.paste(text);
        } else if let Some(ask) = self.view.ask_mut() {
            ask.editor.insert(text);
        } else if text.is_empty() {
            self.view.draft.read_image();
        } else {
            self.view.draft.insert(text);
        }
    }

    /// Enter: reply to a pending ask, run a slash command, or submit the
    /// draft. `/quit` is the only path returning Quit.
    fn submit(&mut self) -> Flow {
        if let Some(ask) = self.view.ask_mut() {
            match ask.answer() {
                Ok(payload) => {
                    let id = ask.id.clone();
                    match self.controller.reply(id, payload, &mut self.view) {
                        Reply::Sent => {
                            self.view.dismiss_ask();
                            self.view.notice(Level::Info, "Reply sent.");
                        }
                        Reply::Rejected(e) => {
                            self.view.dismiss_ask();
                            self.view
                                .notice(Level::Warning, format!("Reply was not accepted: {e}"));
                        }
                        // A session operation is in flight; the guard notice
                        // is shown and the ask stays for retry.
                        Reply::Deferred => {}
                    }
                }
                Err(e) => ask.error = Some(e),
            }
            return Flow::Continue;
        }
        let text = self.view.draft.text().to_owned();
        if self.view.draft.is_empty() {
            return Flow::Continue;
        }
        // Command dispatch: parse in commands.rs, side effects here.
        // `queued_body` carries /queue's body; plain user text falls through
        // as None.
        let queued_body = match commands::parse(&text) {
            Some(commands::Parsed::Quit) => return Flow::Quit,
            Some(commands::Parsed::Help) => {
                self.view.overlay = Overlay::Help { scroll: 0 };
                self.view.draft.set_text("");
                return Flow::Continue;
            }
            Some(commands::Parsed::Status) => {
                self.view.overlay = Overlay::Stats { scroll: 0 };
                self.view.draft.set_text("");
                return Flow::Continue;
            }
            Some(commands::Parsed::New) => {
                self.view.draft.set_text("");
                self.controller.switch(Target::New, &mut self.view);
                return Flow::Continue;
            }
            Some(commands::Parsed::Yolo(arg)) => {
                let enabled = match arg {
                    commands::YoloArg::Toggle => !self.controller.yolo(),
                    commands::YoloArg::On => true,
                    commands::YoloArg::Off => false,
                    // A usage error keeps the draft for editing, like the
                    // unknown-command path below.
                    commands::YoloArg::Usage => {
                        self.view.notice(Level::Warning, "Usage: /yolo [on|off]");
                        return Flow::Continue;
                    }
                };
                self.view.draft.set_text("");
                self.set_yolo(enabled);
                return Flow::Continue;
            }
            Some(commands::Parsed::Continue) => {
                self.controller.resume(&mut self.view);
                self.view.draft.set_text("");
                return Flow::Continue;
            }
            Some(commands::Parsed::Theme(name)) => {
                match name {
                    None => {
                        // No argument: open the picker overlay.
                        let current_idx = theme::Theme::ALL
                            .iter()
                            .position(|t| *t == self.view.theme)
                            .unwrap_or(0);
                        self.view.overlay = Overlay::Themes(current_idx);
                    }
                    Some(name) => match theme::Theme::parse(&name) {
                        Some(theme) => self.view.theme = theme,
                        None => self.view.notice(
                            Level::Warning,
                            format!("Themes: {}. /theme NAME", theme_names()),
                        ),
                    },
                }
                self.view.draft.set_text("");
                return Flow::Continue;
            }
            Some(commands::Parsed::Models { id, variant }) => {
                self.view.draft.set_text("");
                match id {
                    None => self.view.overlay = Overlay::Models(0),
                    Some(model_id) => {
                        self.controller.model(model_id, variant, &mut self.view);
                    }
                }
                return Flow::Continue;
            }
            Some(commands::Parsed::Sessions) => {
                self.view.draft.set_text("");
                self.controller.list(&mut self.view);
                return Flow::Continue;
            }
            Some(commands::Parsed::Compact) => {
                self.view.draft.set_text("");
                self.controller.compact(&mut self.view);
                return Flow::Continue;
            }
            Some(commands::Parsed::Queue(body)) => {
                if body.trim().is_empty() {
                    return Flow::Continue;
                }
                Some(body)
            }
            None if text.starts_with('/') => {
                self.view
                    .notice(Level::Warning, "Unknown command. /help lists commands.");
                return Flow::Continue;
            }
            None => None,
        };
        let (queued, body) = match queued_body {
            Some(body) => (true, body),
            None => (false, text.clone()),
        };
        if self.view.draft.reading_image() {
            self.view
                .notice(Level::Info, "Reading clipboard image; wait before sending.");
            return Flow::Continue;
        }
        let atts = self.view.draft.attachments();
        let n_images = atts
            .iter()
            .filter(|a| matches!(a.data, AttachmentData::Base64(_)))
            .count();
        let n_refs = atts.len() - n_images;
        // /queue is a mid-turn steer; it cannot carry attachments (images or
        // file references).
        if queued && !atts.is_empty() {
            self.view.notice(
                Level::Warning,
                "Attachments cannot be queued. Send without /queue, or press Esc to clear them.",
            );
            return Flow::Continue;
        }
        // Allow image-only messages (no text body).
        if body.trim().is_empty() && atts.is_empty() {
            return Flow::Continue;
        }
        let input = if queued {
            In::follow_up(&body)
        } else if atts.is_empty() {
            In::user_text(&body)
        } else {
            In::user_text_with_attachments(&body, atts)
        };
        if self.controller.submit(input, &mut self.view) {
            // Clear attachments only after a successful submit, so a failure
            // (e.g. turn-in-flight rejection) preserves them for retry.
            self.view.draft.submitted();
            let mut display = body.clone();
            if n_images > 0 {
                display.push_str(&format!(" [img×{n_images}]"));
            }
            if n_refs > 0 {
                display.push_str(&format!(" [ref×{n_refs}]"));
            }
            self.view.user(&display, queued);
            self.view.follow();
        }
        Flow::Continue
    }

    fn overlay_action(&mut self, action: OverlayAction) {
        match action {
            OverlayAction::None => {}
            OverlayAction::Model(index) => {
                if let Some(choice) = self.view.model_choices.get(index) {
                    self.controller.model(
                        choice.id.clone(),
                        choice.variant.clone(),
                        &mut self.view,
                    );
                }
            }
            OverlayAction::Theme(theme) => self.view.theme = theme,
            OverlayAction::Session(id) => {
                self.controller.switch(Target::Resume(id), &mut self.view);
            }
            OverlayAction::Delete(id) => self.controller.delete(id, &mut self.view),
        }
    }

    fn copy_selection(&mut self) {
        if let Some(text) = self
            .renderer
            .selection
            .text()
            .filter(|text| !text.is_empty())
        {
            if let Some(previous) = self.clipboard_task.take() {
                previous.abort();
            }
            self.clipboard_task = Some(tokio::spawn(clipboard::copy(text)));
        }
    }

    fn set_yolo(&mut self, enabled: bool) {
        match self.controller.set_yolo(enabled) {
            Ok(()) => {
                self.view.notice(
                    Level::Info,
                    if enabled {
                        "YOLO enabled for subsequent turns. /yolo off restores approvals."
                    } else {
                        "YOLO disabled. Original approval policy restored."
                    },
                );
            }
            Err(_) => self.view.notice(
                Level::Warning,
                "Permission mode can change between turns. Press Esc to cancel, then Ctrl-G or /yolo.",
            ),
        }
    }

    fn click_dispatch(&mut self, x: u16, y: u16) {
        click_dispatch(&mut self.renderer, &mut self.view, x, y);
    }

    fn wheel_dispatch(&mut self, x: u16, y: u16, up: bool) {
        wheel_dispatch(&mut self.renderer, &mut self.view, x, y, up);
    }
}

/// Resolve a click against the last painted frame and apply it to the view.
/// The single place that turns hit regions into View mutations — the
/// renderer itself only answers queries (see `Renderer::hit_test`).
pub(super) fn click_dispatch(renderer: &mut Renderer, view: &mut View, x: u16, y: u16) {
    match renderer.hit_test(x, y) {
        Some(Hit::FollowLatest) => {
            view.follow();
        }
        Some(Hit::Command(command)) => {
            view.draft.set_text("");
            view.draft.insert(command.text);
            if command.argument {
                view.draft.insert(" ");
            }
        }
        Some(Hit::Mention(index)) => view.draft.accept_mention(index),
        Some(Hit::TodoToggle) => view.session.todos.panel = !view.session.todos.panel,
        Some(Hit::Block(id)) => {
            renderer.anchor(view, id, y);
            view.toggle(id);
        }
        None => {}
    }
}

/// Route a wheel event: the Todo panel scrolls its own list (when visible),
/// everything else scrolls the transcript; reaching the bottom re-follows.
pub(super) fn wheel_dispatch(renderer: &mut Renderer, view: &mut View, x: u16, y: u16, up: bool) {
    if renderer.wheel_on_todo(x, y, view.session.todos.panel) {
        view.session.todos.nudge(1, up);
    } else {
        renderer.scroll(view, 3, up);
    }
}

/// Comma-separated theme list for the /theme usage notice, generated from
/// the registry so new themes cannot stale the message.
fn theme_names() -> String {
    theme::Theme::ALL
        .iter()
        .map(|t| t.name())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::{App, Flow};
    use crate::ui::{
        overlay::Overlay,
        render::Metadata,
        session,
        state::{Item, Role, SessionPickerState, View},
    };
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    };
    use ratatui::layout::Rect;
    use std::time::{Duration, Instant};
    use yourai_core::prelude::*;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }
    fn ctrl(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::CONTROL))
    }
    fn alt(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::ALT))
    }
    fn mouse(kind: MouseEventKind, x: u16, y: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        })
    }
    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            assert_eq!(app.handle(key(KeyCode::Char(c))), Flow::Continue);
        }
    }
    async fn fixture() -> (tempfile::TempDir, App) {
        let (dir, controller, view) = session::fixture().await;
        let meta = Metadata {
            session: "test".into(),
            cwd: "/tmp".into(),
            trusted_shell: false,
            yolo: false,
        };
        let app = App::new(controller, view, meta);
        (dir, app)
    }
    fn last_notice(view: &View) -> Option<(Level, &str)> {
        view.items().iter().rev().find_map(|item| match item {
            Item::Notice { level, text } => Some((*level, text.as_str())),
            _ => None,
        })
    }
    async fn finish(app: &mut App) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while app.controller.busy() {
                app.poll(None).await;
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("session operation did not complete");
    }

    #[tokio::test]
    async fn plain_keys_edit_the_draft() {
        let (_dir, mut app) = fixture().await;
        type_text(&mut app, "hello world");
        assert_eq!(app.view.draft.text(), "hello world");
        // Release events and resizes never reach the editor.
        app.handle(Event::Key(KeyEvent {
            code: KeyCode::Char('x'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: crossterm::event::KeyEventState::NONE,
        }));
        assert_eq!(app.view.draft.text(), "hello world");
        app.close().await.unwrap();
    }

    #[tokio::test]
    async fn ctrl_q_and_quit_command_return_quit() {
        let (_dir, mut app) = fixture().await;
        assert_eq!(app.handle(key(KeyCode::F(1))), Flow::Continue);
        assert!(app.view.overlay.is_open());
        // Quit wins even while a modal is open.
        assert_eq!(app.handle(ctrl(KeyCode::Char('q'))), Flow::Quit);
        let (_dir2, mut app2) = fixture().await;
        type_text(&mut app2, "/quit");
        assert_eq!(app2.handle(key(KeyCode::Enter)), Flow::Quit);
        // An empty draft submits nothing.
        let (_dir3, mut app3) = fixture().await;
        assert_eq!(app3.handle(key(KeyCode::Enter)), Flow::Continue);
        assert_eq!(app3.view.draft.text(), "");
        assert!(app3.view.items().is_empty());
    }

    #[tokio::test]
    async fn esc_clears_an_active_selection_before_interrupting() {
        let (_dir, mut app) = fixture().await;
        // Paint once so hit regions and the selection snapshot exist.
        app.paint(
            Rect::new(0, 0, 80, 24),
            super::FrameTime {
                monotonic: Instant::now(),
                unix_seconds: 0,
            },
            0,
            false,
        );
        app.handle(mouse(MouseEventKind::Down(MouseButton::Left), 40, 10));
        app.handle(mouse(MouseEventKind::Drag(MouseButton::Left), 40, 12));
        assert!(app.renderer.selection.active());
        // First Esc only clears the selection; no cancellation notice.
        app.handle(key(KeyCode::Esc));
        assert!(!app.renderer.selection.active());
        assert!(last_notice(&app.view).is_none());
        // Second Esc interrupts.
        app.handle(key(KeyCode::Esc));
        assert_eq!(
            last_notice(&app.view),
            Some((Level::Info, "Cancellation requested."))
        );
        app.close().await.unwrap();
    }

    #[tokio::test]
    async fn alt_c_consumes_the_key_without_inserting() {
        let (_dir, mut app) = fixture().await;
        assert_eq!(app.handle(alt(KeyCode::Char('c'))), Flow::Continue);
        // Nothing was selected: no clipboard task, no 'c' in the draft.
        assert_eq!(app.view.draft.text(), "");
        assert!(app.clipboard_task.is_none());
        app.close().await.unwrap();
    }

    #[tokio::test]
    async fn menu_enter_completes_and_dispatches_in_one_event() {
        let (_dir, mut app) = fixture().await;
        type_text(&mut app, "/he");
        app.handle(key(KeyCode::Enter));
        // The completion filled "/help" and fell through to the submit path.
        assert!(matches!(app.view.overlay, Overlay::Help { .. }));
        assert_eq!(app.view.draft.text(), "");
        app.close().await.unwrap();
    }

    #[tokio::test]
    async fn menu_tab_and_argument_rows_fill_without_dispatching() {
        let (_dir, mut app) = fixture().await;
        type_text(&mut app, "/the");
        app.handle(key(KeyCode::Tab));
        assert_eq!(app.view.draft.text(), "/theme");
        assert!(!app.view.overlay.is_open());
        app.view.draft.set_text("");
        type_text(&mut app, "/qu");
        app.handle(key(KeyCode::Enter));
        // /queue takes an argument: Enter completes it but does not submit.
        assert_eq!(app.view.draft.text(), "/queue ");
        assert!(app.view.items().is_empty());
        app.close().await.unwrap();
    }

    #[tokio::test]
    async fn unknown_command_warns_and_keeps_the_draft() {
        let (_dir, mut app) = fixture().await;
        type_text(&mut app, "/nope");
        app.handle(key(KeyCode::Enter));
        assert_eq!(
            last_notice(&app.view),
            Some((Level::Warning, "Unknown command. /help lists commands."))
        );
        assert_eq!(app.view.draft.text(), "/nope");
        app.close().await.unwrap();
    }

    #[tokio::test]
    async fn yolo_usage_preserves_the_draft_and_on_updates_meta() {
        let (_dir, mut app) = fixture().await;
        type_text(&mut app, "/yolo junk");
        app.handle(key(KeyCode::Enter));
        assert_eq!(
            last_notice(&app.view),
            Some((Level::Warning, "Usage: /yolo [on|off]"))
        );
        // A usage error keeps the draft for editing, like unknown commands.
        assert_eq!(app.view.draft.text(), "/yolo junk");
        app.view.draft.set_text("");
        type_text(&mut app, "/yolo on");
        app.handle(key(KeyCode::Enter));
        assert!(app.controller.yolo());
        // The Metadata mirror refreshes from its owner at paint time.
        app.paint(
            Rect::new(0, 0, 80, 24),
            super::FrameTime {
                monotonic: Instant::now(),
                unix_seconds: 0,
            },
            0,
            false,
        );
        assert!(app.meta.yolo);
        assert_eq!(app.view.draft.text(), "");
        app.close().await.unwrap();
    }

    #[tokio::test]
    async fn submit_records_the_user_item_and_history() {
        let (_dir, mut app) = fixture().await;
        type_text(&mut app, "first task");
        app.handle(key(KeyCode::Enter));
        assert_eq!(app.view.draft.text(), "");
        assert!(app.view.items().iter().any(|i| matches!(
            i,
            Item::Text { role: Role::User, text } if text.contains("first task")
        )));
        // Ctrl-P recalls the remembered draft.
        app.handle(ctrl(KeyCode::Char('p')));
        assert_eq!(app.view.draft.text(), "first task");
        app.close().await.unwrap();
    }

    #[tokio::test]
    async fn new_session_switches_and_keeps_the_inflight_draft() {
        let (_dir, mut app) = fixture().await;
        let old = app.controller.harness().host.context().id;
        type_text(&mut app, "/new");
        app.handle(key(KeyCode::Enter));
        assert!(app.controller.busy());
        type_text(&mut app, "draft typed while opening");
        finish(&mut app).await;
        assert_ne!(app.controller.harness().host.context().id, old);
        assert_eq!(app.view.draft.text(), "draft typed while opening");
        assert_ne!(app.meta.session, old.as_str());
        app.close().await.unwrap();
    }

    #[tokio::test]
    async fn todo_sync_runs_only_when_the_board_changes() {
        let (_dir, mut app) = fixture().await;
        let tasks = app.controller.harness().tasks.clone().unwrap();
        app.sync_todos();
        let cached = app.synced_todos;
        assert!(app.view.session.todos.is_empty());
        // An unchanged board is not re-copied.
        app.sync_todos();
        assert_eq!(app.synced_todos, cached);
        // A committed mutation bumps the version and the next sync adopts it.
        tasks
            .create("write the regression first".into(), None, None)
            .await
            .unwrap();
        app.sync_todos();
        assert_ne!(app.synced_todos, cached);
        assert_eq!(app.view.session.todos.items().len(), 1);
        assert_eq!(
            app.view.session.todos.items()[0].text,
            "write the regression first"
        );
        app.close().await.unwrap();
    }

    #[tokio::test]
    async fn paste_routes_to_the_overlay_query_then_the_draft() {
        let (_dir, mut app) = fixture().await;
        app.view.overlay = Overlay::Sessions(SessionPickerState {
            pending_delete: None,
            rows: vec![],
            query: String::new(),
            selected: 0,
        });
        app.handle(Event::Paste("previous ".into()));
        assert_eq!(app.view.draft.text(), "");
        // Esc closes the picker; paste then lands in the draft editor.
        app.handle(key(KeyCode::Esc));
        assert!(!app.view.overlay.is_open());
        app.handle(Event::Paste("draft".into()));
        assert_eq!(app.view.draft.text(), "draft");
        // Mouse events never reach the transcript while a modal is open.
        app.view.overlay = Overlay::Help { scroll: 0 };
        app.handle(mouse(MouseEventKind::Down(MouseButton::Left), 40, 10));
        assert!(!app.renderer.selection.active());
        app.close().await.unwrap();
    }
    #[tokio::test]
    async fn review_mention_allows_typing_query() {
        let (_dir, mut app) = fixture().await;
        type_text(&mut app, "@src");
        assert_eq!(app.view.draft.text(), "@src");
    }
    #[tokio::test]
    async fn review_paste_refreshes_mention_query() {
        let (_dir, mut app) = fixture().await;
        type_text(&mut app, "@");
        app.handle(Event::Paste("src".into()));
        assert_eq!(app.view.draft.mention().query, "src");
    }
    #[tokio::test]
    async fn review_overlay_escape_preserves_draft_attachment() {
        let (_dir, mut app) = fixture().await;
        app.view
            .draft
            .stage_image(super::clipboard::ClipboardImage {
                mime: "image/png".into(),
                data: "aGVsbG8=".into(),
            });
        app.handle(key(KeyCode::F(1)));
        app.handle(key(KeyCode::Esc));
        assert_eq!(app.view.draft.attachments().len(), 1);
        assert!(!app.view.overlay.is_open());
    }
    #[tokio::test]
    async fn review_switch_preserves_whole_draft() {
        let (_dir, mut app) = fixture().await;
        type_text(&mut app, "/new");
        app.handle(key(KeyCode::Enter));
        type_text(&mut app, "draft typed while opening");
        app.view
            .draft
            .stage_image(super::clipboard::ClipboardImage {
                mime: "image/png".into(),
                data: "aGVsbG8=".into(),
            });
        finish(&mut app).await;
        assert_eq!(app.view.draft.text(), "draft typed while opening");
        assert_eq!(app.view.draft.attachments().len(), 1);
    }

    #[tokio::test]
    async fn rejected_input_uses_normal_history_without_overwriting_new_edits() {
        let (_dir, mut app) = fixture().await;
        type_text(&mut app, "inspect image");
        app.view
            .draft
            .stage_image(super::clipboard::ClipboardImage {
                mime: "image/png".into(),
                data: "invalid".into(),
            });
        app.handle(key(KeyCode::Enter));
        type_text(&mut app, "new unsent draft");
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                app.poll(None).await;
                if matches!(last_notice(&app.view), Some((Level::Warning, text)) if text.starts_with("Input rejected:")) { break; }
                tokio::task::yield_now().await;
            }
        }).await.unwrap();
        assert_eq!(app.view.draft.text(), "new unsent draft");
        app.handle(key(KeyCode::Up));
        assert_eq!(app.view.draft.text(), "inspect image");
        assert_eq!(
            app.view.draft.attachments()[0].name.as_deref(),
            Some("clipboard-1.png")
        );
        app.handle(key(KeyCode::Down));
        assert_eq!(app.view.draft.text(), "new unsent draft");
        assert!(app.view.draft.attachments().is_empty());
        app.handle(ctrl(KeyCode::Char('u')));
        type_text(&mut app, "/new");
        app.handle(key(KeyCode::Enter));
        finish(&mut app).await;
        app.handle(key(KeyCode::Up));
        assert_eq!(app.view.draft.text(), "inspect image");
        assert_eq!(app.view.draft.attachment_counts(), (1, 0));
        // No rejection-specific copy in history: a second Up stays at this input.
        app.handle(key(KeyCode::Up));
        app.handle(key(KeyCode::Down));
        assert_eq!(app.view.draft.text(), "");
        app.close().await.unwrap();
    }
}

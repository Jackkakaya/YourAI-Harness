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
    state::{self, PermissionChoice, View},
    theme,
};
use crate::config::Error;
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::prelude::{Position, Rect};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use yourai_core::prelude::*;

/// What the loop should do after one event. Quit and Editor are the only
/// control flows the loop cannot express locally: Quit drops the rest of the
/// burst, skips the frame tail and returns from the UI future; Editor hands
/// the terminal to `$VISUAL`/`$EDITOR`, which needs the loop's reader and
/// terminal handles.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Flow {
    Continue,
    Quit,
    Editor,
}

pub(super) struct App {
    controller: Controller,
    view: View,
    meta: Metadata,
    renderer: Renderer,
    clipboard_task: Option<JoinHandle<std::io::Result<()>>>,
    /// In-flight git-context refresh; results land in `view.git`.
    git_task: Option<JoinHandle<crate::git::GitContext>>,
    /// When the git context was last refreshed; None spawns immediately.
    git_refreshed: Option<Instant>,
    /// The TaskManager version already reflected in `view.session.todos`; None means
    /// the next sync must run (also reset on session switch).
    synced_todos: Option<u64>,
    /// Terminal focus from crossterm focus events; assumed focused until a
    /// FocusLost arrives. Attention bells only fire while unfocused.
    focused: bool,
    /// A bell the loop should ring at the next frame tail.
    attention: bool,
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
            git_task: None,
            git_refreshed: None,
            synced_todos: None,
            focused: true,
            attention: false,
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
        // Attention signals: an approval/reply request that just appeared, or
        // a turn that just finished. Only announced while the terminal is
        // unfocused; the visible UI is its own notification otherwise.
        let had_ask = !self.view.asks_empty();
        let was_active = self.view.session.active;
        let switched = self.controller.poll(&mut self.view, first).await;
        let context = self.controller.harness().host.context();
        if switched {
            self.meta.session = context.id.as_str().to_owned();
            self.renderer = Renderer::default();
            self.synced_todos = None;
        }
        // Workspace changes can happen within a session, not just on switch.
        // Footer, file completion and git must observe the same live cwd.
        let cwd = context.cwd.to_string_lossy().into_owned();
        self.view.draft.set_cwd(&context.cwd);
        if self.meta.cwd != cwd {
            self.meta.cwd = cwd;
            if let Some(task) = self.git_task.take() {
                task.abort();
            }
            self.git_refreshed = None;
            self.view.git = Default::default();
        }
        if !self.focused
            && ((!had_ask && !self.view.asks_empty())
                || (was_active && !self.view.session.active && self.view.asks_empty()))
        {
            self.attention = true;
        }
    }

    /// Consume a pending attention bell request. The loop rings it in the
    /// frame tail so it never interleaves with a frame write.
    pub(super) fn take_attention(&mut self) -> bool {
        std::mem::take(&mut self.attention)
    }

    /// Interpret one terminal event and apply every resulting mutation.
    /// Synchronous by construction: async work is only spawned here and
    /// reconciled by `poll` and the frame tail. The only output channels
    /// are field mutation, `tokio::spawn` and `Flow`.
    pub(super) fn handle(&mut self, event: Event) -> Flow {
        match event {
            Event::Resize(_, _) => Flow::Continue,
            Event::FocusGained => {
                self.focused = true;
                Flow::Continue
            }
            Event::FocusLost => {
                self.focused = false;
                Flow::Continue
            }
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

    /// Keep `view.git` current: reap a finished detection and, at a bounded
    /// cadence, start the next one from the live session cwd (the agent may
    /// switch branches or enter a worktree mid-session). Cheap on every wake:
    /// one finished-task check plus a time gate.
    pub(super) async fn settle_git(&mut self) {
        if self.git_task.as_ref().is_some_and(|t| t.is_finished()) {
            if let Ok(ctx) = self.git_task.take().unwrap().await {
                self.view.git = ctx;
            }
        }
        const GIT_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
        let due = self
            .git_refreshed
            .is_none_or(|at| at.elapsed() >= GIT_REFRESH_INTERVAL);
        if self.git_task.is_none() && due {
            let cwd = self.controller.harness().host.context().cwd.clone();
            self.git_task = Some(tokio::spawn(async move {
                crate::git::GitContext::detect(&cwd).await
            }));
            self.git_refreshed = Some(Instant::now());
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

    /// Abort clipboard and git work, then close the session (finishing
    /// any pending operation rather than dropping its handle).
    pub(super) async fn close(mut self) -> Result<(String, Vec<In>), Error> {
        if let Some(task) = self.clipboard_task.take() {
            task.abort();
        }
        if let Some(task) = self.git_task.take() {
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
        // Permission asks are a choice list, not free text: y/a/n answer
        // directly, arrows move, Enter confirms the selection. Everything
        // else falls through (Esc/Ctrl-C still interrupt the turn).
        if !self.view.asks_empty()
            && self.view.ask().is_some_and(|a| a.permission())
            && !ctrl
            && !alt
        {
            match key.code {
                KeyCode::Char('y') => return self.permission_submit(PermissionChoice::Once),
                KeyCode::Char('a') => return self.permission_submit(PermissionChoice::Always),
                KeyCode::Char('n') => return self.permission_submit(PermissionChoice::Deny),
                KeyCode::Up | KeyCode::Down => {
                    if let Some(ask) = self.view.ask_mut() {
                        let next = ask.permission_choice;
                        ask.permission_choice = match (key.code, next) {
                            (KeyCode::Up, 0) | (KeyCode::Down, 2) => next,
                            (KeyCode::Up, _) => next - 1,
                            (KeyCode::Down, _) => next + 1,
                            _ => next,
                        };
                    }
                    return Flow::Continue;
                }
                KeyCode::Enter => {
                    let choice = self
                        .view
                        .ask()
                        .map(|a| a.permission_selection())
                        .unwrap_or(PermissionChoice::Once);
                    return self.permission_submit(choice);
                }
                // Other plain keys would land in an invisible editor.
                KeyCode::Char(_) => return Flow::Continue,
                _ => {}
            }
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
                    if key.code == KeyCode::Tab {
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
            KeyCode::F(2) => self.cycle_model(),
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
            // Hand the draft to $VISUAL/$EDITOR. Only while no ask is pending:
            // the ask's own reply editor must keep the keyboard then. The
            // readline association (bash's Ctrl-X Ctrl-E) carries the intent.
            KeyCode::Char('x') if ctrl && self.view.asks_empty() => return Flow::Editor,
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
                self.controller.interrupt();
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
            // Permission asks were answered by key selection before reaching
            // the editor path; a stray Enter confirms the selected row.
            if ask.permission() {
                let choice = ask.permission_selection();
                return self.permission_submit(choice);
            }
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
        match commands::parse(&text) {
            Some(commands::Parsed::Quit) => return Flow::Quit,
            Some(commands::Parsed::Help) => {
                self.view.overlay = Overlay::Help { scroll: 0 };
                self.view.draft.set_text("");
                return Flow::Continue;
            }
            // The draft is the editor's buffer; it must survive dispatch.
            Some(commands::Parsed::Editor) => return Flow::Editor,
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
                        self.controller
                            .model(model_id, variant, None, &mut self.view);
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
            None if text.starts_with('/') => {
                self.view
                    .notice(Level::Warning, "Unknown command. /help lists commands.");
                return Flow::Continue;
            }
            None => {}
        };
        let body = text;
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
        // Allow image-only messages (no text body).
        if body.trim().is_empty() && atts.is_empty() {
            return Flow::Continue;
        }
        let input = In::UserText {
            id: None,
            text: body.clone(),
            mode: InputMode::FollowUp,
            attachments: atts,
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
            self.view.user(&display);
            self.view.follow();
        }
        Flow::Continue
    }

    fn overlay_action(&mut self, action: OverlayAction) {
        match action {
            OverlayAction::None => {}
            OverlayAction::PickEffort(index) => {
                // Preselect the entry's effective effort; "default" when unset.
                let Some(choice) = self.view.model_choices.get(index) else {
                    return;
                };
                let selected = choice
                    .effort
                    .as_deref()
                    .and_then(|effort| {
                        crate::models::EFFORT_CHOICES
                            .iter()
                            .position(|level| *level == Some(effort))
                    })
                    .unwrap_or(0);
                self.view.overlay = Overlay::Effort {
                    model: index,
                    selected,
                };
            }
            OverlayAction::Effort { model, effort } => {
                let Some(choice) = self.view.model_choices.get(model) else {
                    return;
                };
                // Confirming the preselected value keeps its source intact:
                // an inherited variant must not acquire an explicit override.
                let selection = if effort.as_deref() == choice.effort.as_deref() {
                    None
                } else {
                    Some(match effort {
                        Some(effort) => crate::models::EffortChoice::Set(effort),
                        None => crate::models::EffortChoice::Config,
                    })
                };
                self.controller.model(
                    choice.id.clone(),
                    choice.variant.clone(),
                    selection,
                    &mut self.view,
                );
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

    /// Switch to the model after the current one in picker order (F2).
    fn cycle_model(&mut self) {
        if let Some(at) = self.view.next_model() {
            let choice = self.view.model_choices[at].clone();
            self.controller
                .model(choice.id, choice.variant, None, &mut self.view);
        }
    }

    /// The text the external editor starts from.
    pub(super) fn draft_text(&self) -> &str {
        self.view.draft.text()
    }

    /// Apply the external editor's result: the saved text, or a warning that
    /// the draft is unchanged. Editors terminate files with a newline; it is
    /// not part of the text. `set_text` keeps explicitly staged attachments
    /// and drops mention references whose markers are gone.
    pub(super) fn editor_finished(&mut self, result: Result<String, String>) {
        match result {
            Ok(text) => self
                .view
                .draft
                .set_text(text.trim_end_matches(['\n', '\r'])),
            Err(message) => self.view.notice(Level::Warning, message),
        }
    }

    /// Submit an approval choice. Security records session-scoped approvals
    /// after accepting the reply; the UI owns no permission rules.
    fn permission_submit(&mut self, choice: PermissionChoice) -> Flow {
        let Some(ask) = self.view.ask() else {
            return Flow::Continue;
        };
        if !ask.permission() {
            return Flow::Continue;
        }
        let id = ask.id.clone();
        let tool = ask.payload["tool_name"]
            .as_str()
            .unwrap_or("tool")
            .to_owned();
        match self.controller.reply(id, choice.reply(), &mut self.view) {
            Reply::Sent => {
                self.view.dismiss_ask();
                self.view.notice(
                    Level::Info,
                    match choice {
                        PermissionChoice::Always => {
                            format!("Session approval submitted for {tool}.")
                        }
                        PermissionChoice::Deny => format!("Denial submitted for {tool}."),
                        PermissionChoice::Once => format!("Approval submitted for {tool}."),
                    },
                );
            }
            Reply::Rejected(e) => {
                self.view.dismiss_ask();
                self.view
                    .notice(Level::Warning, format!("Reply was not accepted: {e}"));
            }
            // A session operation is in flight; the ask stays for retry.
            Reply::Deferred => {}
        }
        Flow::Continue
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
        if let Some(Hit::Steer(id)) = self.renderer.hit_test(x, y) {
            self.controller.steer_pending(id.to_owned(), &mut self.view);
        } else {
            click_dispatch(&mut self.renderer, &mut self.view, x, y);
        }
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
        }
        Some(Hit::Mention(index)) => view.draft.accept_mention(index),
        Some(Hit::TodoToggle) => view.session.todos.panel = !view.session.todos.panel,
        Some(Hit::Block(id)) => {
            renderer.anchor(view, id, y);
            view.toggle(id);
        }
        Some(Hit::Steer(_)) | None => {}
    }
}

/// Route a wheel event: the Todo panel scrolls its own list (when visible),
/// everything else scrolls the transcript; reaching the bottom re-follows.
pub(super) fn wheel_dispatch(renderer: &mut Renderer, view: &mut View, x: u16, y: u16, up: bool) {
    if renderer.wheel_on_pending(x, y) {
        view.session.pending_offset = if up {
            view.session.pending_offset.saturating_sub(1)
        } else {
            (view.session.pending_offset + 1)
                .min(view.session.pending_inputs.len().saturating_sub(1))
        };
    } else if renderer.wheel_on_todo(x, y, view.session.todos.panel) {
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

    struct WaitingModel;
    impl ModelProvider for WaitingModel {
        fn model_iden(&self) -> &str {
            "waiting"
        }
        fn complete<'a>(
            &'a self,
            _: ModelRequest,
        ) -> BoxFuture<'a, Result<ChatResponse, YourAiError>> {
            Box::pin(std::future::pending())
        }
        fn stream_events<'a>(
            &'a self,
            _: ModelRequest,
        ) -> BoxFuture<'a, Result<ModelEventStream, YourAiError>> {
            Box::pin(std::future::pending())
        }
    }
    #[tokio::test]
    async fn ordinary_send_queues_and_click_promotes_that_message() {
        let (_dir, mut app) = fixture().await;
        app.controller
            .harness()
            .switch_model(std::sync::Arc::new(WaitingModel), ContextPolicy::default())
            .await
            .unwrap();
        type_text(&mut app, "first task");
        app.handle(key(KeyCode::Enter));
        tokio::time::timeout(Duration::from_secs(5), async {
            while !matches!(
                app.controller.harness().host.status(),
                SessionStatus::Running { .. }
            ) {
                app.poll(None).await;
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        type_text(&mut app, "change direction");
        app.handle(key(KeyCode::Enter));
        tokio::time::timeout(Duration::from_secs(5), async {
            while !app.view.session.pending_inputs.iter().any(
                |entry| matches!(entry, In::UserText { text, .. } if text == "change direction"),
            ) {
                app.poll(None).await;
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let queued = &app.view.session.pending_inputs[0];
        assert!(
            matches!(queued, In::UserText { mode: InputMode::FollowUp, text, .. } if text == "change direction")
        );
        let id = queued.id().unwrap().to_owned();
        for width in [30, 80] {
            app.paint(
                Rect::new(0, 0, width, 24),
                super::FrameTime {
                    monotonic: Instant::now(),
                    unix_seconds: 0,
                },
                1,
                false,
            );
            let hit = (0..24).flat_map(|y| (0..width).map(move |x| (x,y)))
                .find(|(x,y)| matches!(app.renderer.hit_test(*x,*y), Some(super::Hit::Steer(found)) if found == id));
            let (x, y) = hit.expect("queued message must have a clickable Steer action");
            if width == 80 {
                app.handle(mouse(MouseEventKind::Down(MouseButton::Left), x, y));
                app.handle(mouse(MouseEventKind::Up(MouseButton::Left), x, y));
            }
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while app.controller.harness().host.queued() != 0 {
                app.poll(None).await;
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        app.close().await.unwrap();
    }

    #[tokio::test]
    async fn ctrl_x_and_editor_command_hand_the_draft_to_the_editor() {
        let (_dir, mut app) = fixture().await;
        type_text(&mut app, "long draft");
        // Ctrl-X is the readline "edit command line" convention; the draft is
        // the editor's buffer and must survive dispatch.
        assert_eq!(app.handle(ctrl(KeyCode::Char('x'))), Flow::Editor);
        assert_eq!(app.view.draft.text(), "long draft");
        // A saved result replaces the text (trailing editor newline trimmed);
        // a cancelled run keeps it.
        app.editor_finished(Ok("edited in vi\n".into()));
        assert_eq!(app.view.draft.text(), "edited in vi");
        app.editor_finished(Err("Editor exited with 1; draft unchanged.".into()));
        assert_eq!(app.view.draft.text(), "edited in vi");
        // /editor takes the same path.
        app.view.draft.set_text("");
        type_text(&mut app, "/editor");
        assert_eq!(app.handle(key(KeyCode::Enter)), Flow::Editor);
        app.close().await.unwrap();
    }

    #[tokio::test]
    async fn f2_cycles_models() {
        let (_dir, mut app) = fixture().await;
        // One choice only: F2 has nowhere to go and starts no operation.
        app.view.model_choices = vec![crate::models::ModelChoice {
            id: "mock/test".into(),
            variant: None,
            label: "mock/test".into(),
            effort: None,
        }];
        app.handle(key(KeyCode::F(2)));
        assert!(!app.controller.busy());
        // Two choices: F2 starts a switch to the next one in picker order.
        app.view.model_choices.push(crate::models::ModelChoice {
            id: "mock/other".into(),
            variant: None,
            label: "mock/other".into(),
            effort: None,
        });
        app.handle(key(KeyCode::F(2)));
        assert!(app.controller.busy());
        finish(&mut app).await;
        app.close().await.unwrap();
    }

    #[tokio::test]
    async fn permission_asks_answer_by_key() {
        let (_dir, mut app) = fixture().await;
        fn ask(app: &mut App, tool: &str) {
            app.view.event(Out::Ask {
                id: format!("ask-{tool}"),
                payload: serde_json::json!({
                    "kind":"permission", "tool_name":tool,
                    "reason":"Tool permission", "input":{"command":"cargo test"}
                }),
            });
        }
        // 'a' approves and remembers the tool for this session.
        ask(&mut app, "shell");
        assert_eq!(app.handle(key(KeyCode::Char('a'))), Flow::Continue);
        assert!(app.view.asks_empty(), "answered ask is dismissed");
        // 'y' approves once and remembers nothing.
        ask(&mut app, "edit");
        app.handle(key(KeyCode::Char('y')));
        assert!(app.view.asks_empty());
        // 'n' denies.
        ask(&mut app, "write");
        app.handle(key(KeyCode::Char('n')));
        assert!(app.view.asks_empty());
        // Arrows move the selection; Enter confirms the selected row.
        ask(&mut app, "webfetch");
        app.handle(key(KeyCode::Down));
        app.handle(key(KeyCode::Down));
        assert_eq!(
            app.view.ask().unwrap().permission_selection(),
            crate::ui::state::PermissionChoice::Deny
        );
        app.handle(key(KeyCode::Enter));
        assert!(app.view.asks_empty());
        app.close().await.unwrap();
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
    async fn menu_tab_fills_without_dispatching() {
        let (_dir, mut app) = fixture().await;
        type_text(&mut app, "/the");
        app.handle(key(KeyCode::Tab));
        assert_eq!(app.view.draft.text(), "/theme");
        assert!(!app.view.overlay.is_open());
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
    #[tokio::test]
    async fn regression_cwd_change_refreshes_footer_and_file_completion() {
        let (dir, mut app) = fixture().await;
        let host = app.controller.harness().host.clone();
        let original = host.context().cwd;
        app.meta.cwd = original.to_string_lossy().into_owned();
        let next = dir.path().join("next-workspace");
        std::fs::create_dir(&next).unwrap();
        std::fs::write(original.join("old-only.rs"), "old").unwrap();
        std::fs::write(next.join("new-only.rs"), "new").unwrap();
        host.workspace().unwrap().change_cwd(&next).await.unwrap();
        app.poll(None).await;
        app.view.draft.insert("@");
        tokio::time::timeout(Duration::from_secs(3), async {
            while app.view.draft.mention().entries.is_empty() {
                app.settle_draft().await;
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let entries: Vec<_> = app
            .view
            .draft
            .mention()
            .entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect();
        let displayed = app.meta.cwd.clone();
        let expected = host.context().cwd;
        app.close().await.unwrap();
        assert_eq!(
            displayed,
            expected.to_string_lossy(),
            "completion entries: {entries:?}"
        );
        assert!(
            entries
                .iter()
                .any(|path| path == &expected.join("new-only.rs")),
            "{entries:?}"
        );
        assert!(
            !entries
                .iter()
                .any(|path| path == &original.join("old-only.rs")),
            "{entries:?}"
        );
    }

    #[tokio::test]
    async fn regression_new_session_preserves_live_working_directory() {
        let (dir, mut app) = fixture().await;
        let host = app.controller.harness().host.clone();
        let next = dir.path().join("next-workspace");
        std::fs::create_dir(&next).unwrap();
        host.workspace().unwrap().change_cwd(&next).await.unwrap();
        let expected = host.context().cwd;
        app.poll(None).await;
        app.controller.switch(super::Target::New, &mut app.view);
        finish(&mut app).await;
        let actual = app.controller.harness().host.context().cwd;
        app.close().await.unwrap();
        assert_eq!(actual, expected);
    }
    #[tokio::test]
    async fn model_picker_confirms_in_two_steps_and_preserves_an_implicit_model() {
        let (_dir, mut app) = fixture().await;
        // This model resolves from the provider without a models-table entry.
        app.view.model_choices = vec![crate::models::ModelChoice {
            id: "mock/test".into(),
            variant: None,
            label: "mock/test".into(),
            effort: None,
        }];
        app.view.draft.set_text("draft to keep");
        app.view.overlay = Overlay::Models(0);
        app.handle(key(KeyCode::Enter));
        assert!(matches!(
            app.view.overlay,
            Overlay::Effort {
                model: 0,
                selected: 0
            }
        ));
        assert!(
            !app.controller.busy(),
            "the first Enter must not switch models"
        );
        app.handle(key(KeyCode::Down));
        app.handle(key(KeyCode::Esc));
        assert!(matches!(app.view.overlay, Overlay::Models(0)));
        assert!(!app.controller.busy(), "cancelling must not apply effort");
        app.handle(key(KeyCode::Enter));
        app.handle(key(KeyCode::Enter));
        assert!(!app.view.overlay.is_open());
        finish(&mut app).await;
        assert!(
            matches!(last_notice(&app.view), Some((Level::Info, text)) if text.contains("Model switched to mock/test")),
            "{:?}",
            last_notice(&app.view)
        );
        assert_eq!(app.view.draft.text(), "draft to keep");
        // Explicit edits work even though the initial model was implicit.
        app.view.overlay = Overlay::Models(0);
        app.handle(key(KeyCode::Enter));
        for _ in 0..5 {
            app.handle(key(KeyCode::Down));
        }
        app.handle(key(KeyCode::Enter));
        finish(&mut app).await;
        assert_eq!(app.view.model.effort.as_deref(), Some("high"));
        app.view.overlay = Overlay::Models(0);
        app.handle(key(KeyCode::Enter));
        for _ in 0..5 {
            app.handle(key(KeyCode::Up));
        }
        app.handle(key(KeyCode::Enter));
        finish(&mut app).await;
        assert_eq!(app.view.model.effort, None);
        app.close().await.unwrap();
    }
    async fn model_fixture(
        variants: bool,
    ) -> (
        tempfile::TempDir,
        App,
        std::sync::Arc<std::sync::Mutex<crate::config::Config>>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut value = serde_json::json!({
            "model":"mock/a", "extensions":false,
            "provider":{"mock":{"options":{"baseURL":"http://127.0.0.1:1/v1","apiKey":"test"},"models":{
                "a":{"options":{"reasoningEffort":"low"}},"b":{},"c":{}
            }}}
        });
        if variants {
            value["provider"]["mock"]["models"]["a"]["variants"] =
                serde_json::json!({"inherit":{}});
        }
        let cfg: crate::config::Config = serde_json::from_value(value).unwrap();
        let model = cfg.resolve(None).unwrap().model;
        let mut hc =
            yourai_harness::HarnessConfig::new(dir.path().join("sessions"), dir.path().into());
        hc.system_prompt = Some("test".into());
        let h = yourai_harness::Harness::open(hc.clone(), model)
            .await
            .unwrap();
        let mut view = View::default();
        view.model.label = "mock/a".into();
        view.model.effort = Some("low".into());
        view.model_choices = crate::models::model_choices(&cfg);
        let cfg = std::sync::Arc::new(std::sync::Mutex::new(cfg));
        let controller = session::Controller::new(h, cfg.clone(), hc, TurnLimits::default(), false);
        let app = App::new(
            controller,
            view,
            Metadata {
                session: "test".into(),
                cwd: dir.path().to_string_lossy().into(),
                trusted_shell: false,
                yolo: false,
            },
        );
        (dir, app, cfg)
    }

    #[tokio::test]
    async fn f2_visits_all_three_models_after_async_switches() {
        let (_dir, mut app, _cfg) = model_fixture(false).await;
        let mut visited = Vec::new();
        for _ in 0..4 {
            app.handle(key(KeyCode::F(2)));
            finish(&mut app).await;
            visited.push(app.view.model.label.clone());
        }
        app.close().await.unwrap();
        assert_eq!(visited, ["mock/b", "mock/c", "mock/a", "mock/b"]);
    }

    #[tokio::test]
    async fn confirming_effort_preserves_inheritance_and_explicit_overrides() {
        let (_dir, mut app, cfg) = model_fixture(true).await;
        let inherited = app
            .view
            .model_choices
            .iter()
            .position(|c| c.variant.as_deref() == Some("inherit"))
            .unwrap();
        app.view.overlay = Overlay::Models(inherited);
        app.handle(key(KeyCode::Enter));
        app.handle(key(KeyCode::Enter));
        finish(&mut app).await;
        assert!(
            !cfg.lock().unwrap().provider["mock"].models["a"].variants["inherit"]
                .contains_key("reasoningEffort")
        );
        // A base change must still reach the variant after that confirmation.
        app.controller.model(
            "mock/a".into(),
            None,
            Some(crate::models::EffortChoice::Set("high".into())),
            &mut app.view,
        );
        finish(&mut app).await;
        app.view.overlay = Overlay::Models(inherited);
        app.handle(key(KeyCode::Enter));
        app.handle(key(KeyCode::Enter));
        finish(&mut app).await;
        assert_eq!(app.view.model.effort.as_deref(), Some("high"));
        assert!(
            !cfg.lock().unwrap().provider["mock"].models["a"].variants["inherit"]
                .contains_key("reasoningEffort")
        );
        // Intentionally changing high to low pins only the variant.
        app.view.overlay = Overlay::Models(inherited);
        app.handle(key(KeyCode::Enter));
        app.handle(key(KeyCode::Up));
        app.handle(key(KeyCode::Up));
        app.handle(key(KeyCode::Enter));
        finish(&mut app).await;
        app.controller.model(
            "mock/a".into(),
            None,
            Some(crate::models::EffortChoice::Set("medium".into())),
            &mut app.view,
        );
        finish(&mut app).await;
        app.view.overlay = Overlay::Models(inherited);
        app.handle(key(KeyCode::Enter));
        app.handle(key(KeyCode::Enter));
        finish(&mut app).await;
        assert_eq!(app.view.model.effort.as_deref(), Some("low"));
        assert_eq!(
            cfg.lock().unwrap().provider["mock"].models["a"].variants["inherit"]["reasoningEffort"],
            "low"
        );
        // Config default removes that override and resumes live inheritance.
        app.view.overlay = Overlay::Models(inherited);
        app.handle(key(KeyCode::Enter));
        for _ in 0..3 {
            app.handle(key(KeyCode::Up));
        }
        app.handle(key(KeyCode::Enter));
        finish(&mut app).await;
        assert_eq!(app.view.model.effort.as_deref(), Some("medium"));
        assert!(
            !cfg.lock().unwrap().provider["mock"].models["a"].variants["inherit"]
                .contains_key("reasoningEffort")
        );
        app.close().await.unwrap();
    }
}

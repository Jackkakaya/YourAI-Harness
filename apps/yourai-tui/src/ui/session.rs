//! Session lifecycle and asynchronous UI effects. A pending operation serializes
//! session mutations while the UI continues editing, navigating and painting.
mod runtime;
mod setup;
use super::{
    overlay::Overlay,
    state::{SessionPickerState, View},
};
use crate::config::{Config, Error};
use runtime::Runtime;
use serde_json::Value;
use setup::ModelSelection;
use std::sync::Arc;
use tokio::task::JoinHandle;
use yourai_core::prelude::*;
use yourai_harness::{Harness, HarnessConfig};

pub(super) async fn restore_history(h: &Harness, view: &mut View) -> Result<(), YourAiError> {
    setup::restore_history(h, view).await
}
pub(super) enum Target {
    New,
    Resume(SessionId),
}
/// The outcome of answering a pending ask. `Deferred` means a session
/// operation is in flight: the ask stays for retry once it completes.
#[derive(Debug)]
pub(super) enum Reply {
    Sent,
    Rejected(String),
    Deferred,
}
enum Effect {
    Listed(Result<Vec<crate::sessions::SessionRow>, YourAiError>),
    Deleted(SessionId, Result<(), YourAiError>),
    Model(Result<ModelSelection, String>),
    Opened(Result<Box<(Harness, View)>, (Level, String)>),
}
pub(super) struct Controller {
    runtime: Runtime,
    /// A user-initiated session operation (list/delete/model/switch) in flight.
    operation: Option<JoinHandle<Effect>>,
    /// Background close of the session a switch replaced. Cleanup only: it
    /// feeds notices and the new session's first `resume`, never blocks user
    /// input — the new session already owns the UI and storage writes are
    /// serialized by SQLite (WAL + busy timeout).
    retired: Option<JoinHandle<Result<Vec<In>, YourAiError>>>,
    config: Arc<std::sync::Mutex<Config>>,
    template: HarnessConfig,
    limits: TurnLimits,
    /// The permission mode of the current session. Owned here because every
    /// transition (set, switch) goes through the controller; Metadata's copy
    /// is refreshed from this each frame.
    yolo: bool,
}
impl Controller {
    pub fn new(
        h: Harness,
        config: Arc<std::sync::Mutex<Config>>,
        template: HarnessConfig,
        limits: TurnLimits,
        yolo: bool,
    ) -> Self {
        let mut runtime = Runtime::new(h, limits.clone());
        runtime.resume();
        Self {
            runtime,
            operation: None,
            retired: None,
            config,
            template,
            limits,
            yolo,
        }
    }
    /// The current session's permission mode.
    pub fn yolo(&self) -> bool {
        self.yolo
    }
    pub fn harness(&self) -> Arc<Harness> {
        self.runtime.h.clone()
    }
    pub fn compacting(&self) -> bool {
        self.runtime.compacting()
    }
    /// Whether a background session operation (list/delete/model/switch) or
    /// the previous session's close is in flight.
    #[cfg(test)]
    pub fn busy(&self) -> bool {
        self.operation.is_some() || self.retired.is_some()
    }
    pub async fn recv(&mut self) -> Option<Out> {
        self.runtime.recv().await
    }
    /// Whether a session mutation may start. Both a user-initiated operation
    /// and the previous session's close serialize mutations: a switch must
    /// not race the close of the session it replaced.
    fn available(&self, view: &mut View) -> bool {
        if self.operation.is_some() || self.retired.is_some() {
            view.notice(
                Level::Warning,
                "A session operation is in progress. Please wait; your draft is kept.",
            );
            false
        } else {
            true
        }
    }
    /// Whether user input (a submit or an ask reply) is accepted right now.
    /// Only a user-initiated operation blocks it — the previous session's
    /// background close must not: the switch already took effect on screen,
    /// and dropping an Enter in that window reads as the app eating input.
    /// `submit` starts the new session's driver itself, so the message is
    /// served as soon as it lands.
    fn accepting(&self, view: &mut View) -> bool {
        if self.operation.is_some() {
            view.notice(
                Level::Warning,
                "A session operation is in progress. Please wait; your draft is kept.",
            );
            false
        } else {
            true
        }
    }
    fn idle(&self, view: &mut View) -> bool {
        if !self.available(view) {
            return false;
        }
        if !self.runtime.idle() || view.active || !view.asks_empty() {
            view.notice(Level::Warning, "Finish or cancel the current turn, compaction and queued inputs before changing sessions.");
            false
        } else {
            true
        }
    }
    pub fn submit(&mut self, input: In, view: &mut View) -> bool {
        if !self.accepting(view) {
            return false;
        }
        match self.runtime.h.host.submit(input) {
            Ok(()) => {
                self.runtime.resume();
                true
            }
            Err(e) => {
                view.notice(Level::Error, e.to_string());
                false
            }
        }
    }
    /// Reply to a pending ask under the same input guard as `submit`. The
    /// host stays authoritative for acceptance (a rejected reply dismisses
    /// the ask with a warning, as before).
    pub fn reply(&mut self, id: String, payload: Value, view: &mut View) -> Reply {
        if !self.accepting(view) {
            return Reply::Deferred;
        }
        match self.runtime.h.host.submit(In::Reply { id, payload }) {
            Ok(()) => {
                self.runtime.resume();
                Reply::Sent
            }
            Err(e) => Reply::Rejected(e.to_string()),
        }
    }
    pub fn resume(&mut self, view: &mut View) {
        if self.available(view) {
            self.runtime.resume();
        }
    }
    pub fn compact(&mut self, view: &mut View) {
        if self.idle(view) {
            self.runtime.compact();
        }
    }
    pub fn save_title(&mut self, title: String) {
        self.runtime.save_title(title);
    }
    pub fn set_yolo(&mut self, enabled: bool) -> Result<(), YourAiError> {
        if self.operation.is_some() || self.retired.is_some() {
            return Err(ErrorKind::Config("A session operation is in progress".into()).into());
        }
        self.runtime.h.set_yolo(enabled)?;
        self.yolo = enabled;
        Ok(())
    }
    pub fn list(&mut self, view: &mut View) {
        if !self.idle(view) {
            return;
        }
        view.overlay = Overlay::LoadingSessions;
        let h = self.harness();
        self.operation = Some(tokio::spawn(async move {
            let current = h.host.context().id;
            Effect::Listed(
                h.sessions
                    .list_sessions()
                    .await
                    .map(|rows| crate::sessions::rows_from(rows, Some(&current))),
            )
        }));
    }
    pub fn delete(&mut self, id: SessionId, view: &mut View) {
        if !self.available(view) {
            return;
        }
        if id == self.runtime.h.host.context().id {
            view.notice(Level::Warning, "The current session cannot be deleted.");
            return;
        }
        let h = self.harness();
        self.operation = Some(tokio::spawn(async move {
            let result = h.sessions.delete_session(&id).await;
            Effect::Deleted(id, result)
        }));
    }
    pub fn model(&mut self, id: String, variant: Option<String>, view: &mut View) {
        if !self.idle(view) {
            return;
        }
        let h = self.harness();
        let config = self.config.clone();
        self.operation = Some(tokio::spawn(async move {
            Effect::Model(setup::switch_model(&config, &h, &id, variant).await)
        }));
    }
    pub fn switch(&mut self, target: Target, view: &mut View) {
        if !self.idle(view) {
            return;
        }
        let id = match target {
            Target::New => None,
            Target::Resume(id) if id == self.runtime.h.host.context().id => return,
            Target::Resume(id) => Some(id),
        };
        let config = self.config.clone();
        let template = self.template.clone();
        let yolo = self.yolo;
        self.operation = Some(tokio::spawn(async move {
            Effect::Opened(
                setup::open_session(&config, &template, yolo, id)
                    .await
                    .map(Box::new),
            )
        }));
    }
    /// Applying completions is the only point that replaces the current session.
    /// Each Runtime has a different channel, so retired producers cannot publish
    /// into a new view. No pending storage operation is awaited by this method.
    pub async fn poll(&mut self, view: &mut View, first: Option<Out>) -> bool {
        self.runtime.poll(view, first).await;
        // The previous session's close only feeds notices and the new
        // session's first resume; user input never waits for it.
        if self.retired.as_ref().is_some_and(|t| t.is_finished()) {
            match self.retired.take().unwrap().await {
                Ok(Ok(pending)) if !pending.is_empty() => view.notice(
                    Level::Warning,
                    format!(
                        "{} queued inputs from previous session discarded.",
                        pending.len()
                    ),
                ),
                Ok(Err(e)) => view.notice(
                    Level::Error,
                    format!("Could not close previous session: {e}"),
                ),
                Err(e) => view.notice(
                    Level::Error,
                    format!("Previous session close task failed: {e}"),
                ),
                _ => {}
            }
            self.runtime.resume();
            view.notice(
                Level::Info,
                "Session ready. Previous sessions remain in /sessions.",
            );
        }
        if !self.operation.as_ref().is_some_and(|t| t.is_finished()) {
            return false;
        }
        let effect = match self.operation.take().unwrap().await {
            Ok(effect) => effect,
            Err(e) => {
                view.notice(Level::Error, format!("Session operation failed: {e}"));
                return false;
            }
        };
        match effect {
            Effect::Listed(result) => {
                // Esc or another overlay supersedes the request; late data must
                // not reopen the picker and steal focus.
                let show = matches!(view.overlay, Overlay::LoadingSessions);
                match result {
                    Ok(rows) if show => {
                        view.overlay = Overlay::Sessions(SessionPickerState {
                            pending_delete: None,
                            rows,
                            query: String::new(),
                            selected: 0,
                        })
                    }
                    Err(e) => {
                        if show {
                            view.overlay = Overlay::None;
                        }
                        view.notice(Level::Error, format!("Could not list sessions: {e}"));
                    }
                    _ => {}
                }
            }
            Effect::Deleted(id, result) => match result {
                Ok(()) => {
                    if let Overlay::Sessions(picker) = &mut view.overlay {
                        picker.rows.retain(|row| row.id != id);
                        picker.selected = picker.selected.min(
                            crate::sessions::filter_sessions(&picker.rows, &picker.query)
                                .len()
                                .saturating_sub(1),
                        );
                    }
                    view.toast = Some(("Session deleted".into(), std::time::Instant::now()));
                }
                Err(e) => view.notice(Level::Error, format!("Delete failed: {e}")),
            },
            Effect::Model(result) => match result {
                Ok(selected) => {
                    view.notice(Level::Info, format!("Model switched to {}", selected.label));
                    view.model = selected;
                }
                Err(e) => view.notice(Level::Error, format!("Model switch failed: {e}")),
            },
            Effect::Opened(result) => match result {
                Ok(opened) => {
                    let (h, mut fresh) = *opened;
                    // Everything that survives a switch is re-read from the
                    // live view at apply time: edits made in flight, the
                    // theme, the model selection and the picker labels. No
                    // pre-flight capture is needed — operations are
                    // serialized, so the view cannot be replaced while the
                    // switch is being prepared.
                    fresh.editor.text = view.editor.text.clone();
                    fresh.editor.cursor = view.editor.cursor;
                    fresh.theme = view.theme;
                    fresh.model = crate::ui::state::ModelInfo {
                        label: view.model.label.clone(),
                        pricing: view.model.pricing,
                    };
                    fresh.model_choices = view.model_choices.clone();
                    *view = fresh;
                    let old =
                        std::mem::replace(&mut self.runtime, Runtime::new(h, self.limits.clone()));
                    self.retired = Some(tokio::spawn(old.close()));
                    return true;
                }
                Err((level, message)) => view.notice(level, message),
            },
        }
        false
    }
    pub async fn close(mut self) -> Result<(String, Vec<In>), Error> {
        // Finish effects rather than dropping their handles: a successfully
        // prepared candidate must be closed even when the user quits immediately.
        // Other in-flight effects (list/delete/model) are storage work on an
        // Arc'd harness that completes on its own; detaching them is safe.
        if let Some(task) = self.operation.take() {
            if let Ok(Effect::Opened(Ok(opened))) = task.await {
                opened.0.close().await?;
            }
        }
        // The previous session's close is awaited for data consistency; its
        // result only feeds notices, which have nowhere to go during shutdown.
        if let Some(task) = self.retired.take() {
            let _ = task.await;
        }
        let id = self.runtime.h.host.context().id.0;
        Ok((id, self.runtime.close().await?))
    }
}

/// An offline controller: real harness, mock provider on an unreachable
/// endpoint, tempdir storage. Shared with the action-layer tests.
#[cfg(test)]
pub(super) async fn fixture() -> (tempfile::TempDir, Controller, View) {
    let dir = tempfile::tempdir().unwrap();
    let cfg: Config = serde_json::from_value(serde_json::json!({
        "model":"mock/test", "extensions":false,
        "provider":{"mock":{"options":{"baseURL":"http://127.0.0.1:1/v1","apiKey":"test"},"models":{}}}
    })).unwrap();
    let (model, _) = cfg.resolve(None).unwrap();
    let mut hc = HarnessConfig::new(dir.path().join("sessions"), dir.path().into());
    hc.system_prompt = Some("test".into());
    let h = Harness::open(hc.clone(), model).await.unwrap();
    let mut view = View::default();
    view.model.label = "mock/test".into();
    (
        dir,
        Controller::new(
            h,
            Arc::new(std::sync::Mutex::new(cfg)),
            hc,
            TurnLimits::default(),
            false,
        ),
        view,
    )
}

#[cfg(test)]
mod tests {
    use super::fixture;
    use super::{Controller, Effect, Target};
    use crate::ui::{
        overlay::Overlay,
        state::{Item, View},
        theme::Theme,
    };
    use std::time::Duration;
    use tokio::sync::oneshot;
    use yourai_core::prelude::*;

    async fn finish(controller: &mut Controller, view: &mut View) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while controller.operation.is_some() || controller.retired.is_some() {
                controller.poll(view, None).await;
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("session operation did not complete");
    }

    #[tokio::test]
    async fn input_is_accepted_while_the_previous_session_closes() {
        let (_dir, mut controller, mut view) = fixture().await;
        let old = controller.harness();
        let id = old.host.context().id;
        controller.switch(Target::New, &mut view);
        // Let the switch complete so only the background close remains.
        tokio::time::timeout(Duration::from_secs(5), async {
            while controller.operation.is_some() {
                controller.poll(&mut view, None).await;
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("switch did not complete");
        assert_ne!(controller.harness().host.context().id, id);
        // Pin the retire window open: the previous session's close blocks.
        let (release, waiting) = oneshot::channel();
        controller.retired = Some(tokio::spawn(async move {
            waiting.await.unwrap();
            Ok(vec![In::user_text("discarded")])
        }));
        // User input lands in the new session without waiting for the close.
        assert!(controller.submit(In::user_text("fresh start"), &mut view));
        // Session mutations still serialize against the pending close.
        controller.list(&mut view);
        assert!(controller.operation.is_none());
        release.send(()).unwrap();
        finish(&mut controller, &mut view).await;
        assert!(view.items().iter().any(|item| matches!(
            item,
            Item::Notice { level: Level::Warning, text } if text.contains("1 queued inputs")
        )));
        controller.close().await.unwrap();
    }

    #[tokio::test]
    async fn pending_storage_does_not_block_ui_or_reopen_a_dismissed_picker() {
        let (_dir, mut controller, mut view) = fixture().await;
        let (release, waiting) = oneshot::channel();
        controller.operation = Some(tokio::spawn(async move {
            waiting.await.unwrap();
            Effect::Listed(Ok(vec![]))
        }));
        view.overlay = Overlay::LoadingSessions;
        tokio::time::timeout(Duration::from_secs(1), controller.poll(&mut view, None))
            .await
            .unwrap();
        view.overlay = Overlay::None; // user pressed Esc while storage was waiting
        view.editor.insert("editable while waiting");
        assert!(!controller.submit(
            In::user_text("must not submit during a mutation"),
            &mut view
        ));
        assert_eq!(view.editor.text, "editable while waiting");
        release.send(()).unwrap();
        finish(&mut controller, &mut view).await;
        assert!(matches!(view.overlay, Overlay::None));
        assert_eq!(view.editor.text, "editable while waiting");
        controller.close().await.unwrap();
    }

    #[tokio::test]
    async fn failed_switch_keeps_current_session_and_success_preserves_inflight_draft() {
        let (_dir, mut controller, mut view) = fixture().await;
        let old = controller.harness();
        let id = old.host.context().id;
        controller.switch(Target::Resume(SessionId::new()), &mut view);
        finish(&mut controller, &mut view).await;
        assert_eq!(controller.harness().host.context().id, id);
        assert_eq!(old.host.status(), SessionStatus::Idle);
        controller.switch(Target::New, &mut view);
        assert!(controller.operation.is_some());
        view.editor.insert("draft typed during opening");
        view.theme = Theme::Nord;
        // A second operation cannot supersede the first or mutate its settings.
        controller.model("missing/model".into(), None, &mut view);
        finish(&mut controller, &mut view).await;
        assert_ne!(controller.harness().host.context().id, id);
        assert_eq!(old.host.status(), SessionStatus::Closed);
        assert_eq!(view.editor.text, "draft typed during opening");
        assert_eq!(view.theme, Theme::Nord);
        assert_eq!(view.model.label, "mock/test");
        controller.close().await.unwrap();
    }

    #[tokio::test]
    async fn ask_reply_shares_the_mutation_guard_and_stays_host_authoritative() {
        let (_dir, mut controller, mut view) = fixture().await;
        view.event(Out::Ask {
            id: "ask-1".into(),
            payload: serde_json::json!({"kind": "question"}),
        });
        let (release, waiting) = oneshot::channel();
        controller.operation = Some(tokio::spawn(async move {
            waiting.await.unwrap();
            Effect::Deleted(SessionId("x".into()), Ok(()))
        }));
        // A pending session operation defers the reply; the guard notices and
        // the ask survives for retry.
        assert!(matches!(
            controller.reply("ask-1".into(), serde_json::json!("yes"), &mut view),
            super::Reply::Deferred
        ));
        assert!(view.ask().is_some());
        release.send(()).unwrap();
        finish(&mut controller, &mut view).await;
        // The host never held this ask, so it is the one to reject it.
        match controller.reply("ask-1".into(), serde_json::json!("yes"), &mut view) {
            super::Reply::Rejected(reason) => assert!(!reason.is_empty()),
            other => panic!("expected rejection, got {other:?}"),
        }
        controller.close().await.unwrap();
    }

    #[tokio::test]
    async fn queued_inputs_guard_both_new_and_resume() {
        let (_dir, mut controller, mut view) = fixture().await;
        controller.runtime.h.host.interrupt();
        // The host queues synchronously; do not yield between submit and guards.
        controller
            .runtime
            .h
            .host
            .submit(In::follow_up("queued"))
            .unwrap();
        let id = controller.harness().host.context().id;
        controller.switch(Target::New, &mut view);
        assert!(controller.operation.is_none());
        controller.switch(Target::Resume(SessionId::new()), &mut view);
        assert!(controller.operation.is_none());
        assert_eq!(controller.harness().host.context().id, id);
        controller.close().await.unwrap();
    }
}

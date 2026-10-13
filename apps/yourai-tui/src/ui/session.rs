//! Frontend operations and display projection. All session control belongs to the backend.
use yourai_harness::config::Config;
#[cfg(test)]
mod projection_tests;
mod setup;
use super::{
    overlay::Overlay,
    state::{SessionPickerState, View},
};
use crate::config::Error;
pub(super) use setup::restore_history;
use std::{
    sync::{Arc, Mutex},
    time::Instant,
};
use yourai_core::prelude::*;
pub(super) use yourai_harness::controller::Reply;
use yourai_harness::{
    controller::{SessionController, SessionEvent, SessionHistory},
    Harness, SessionOptions,
};
pub(super) enum Target {
    New,
    Resume(SessionId),
}
pub(super) struct Controller {
    backend: SessionController,
    config: Arc<Mutex<Config>>,
    revision: Option<u64>,
}
impl Controller {
    pub fn new(
        h: Harness,
        config: Arc<Mutex<Config>>,
        template: SessionOptions,
        limits: TurnLimits,
    ) -> Self {
        Self {
            backend: SessionController::new(h, config.clone(), template, limits),
            config,
            revision: Some(0),
        }
    }
    pub fn yolo(&self) -> bool {
        self.backend.yolo()
    }
    #[cfg(test)]
    pub fn harness(&self) -> Arc<Harness> {
        self.backend.harness()
    }
    pub fn environment(&self) -> SessionEnvironment {
        self.backend.environment()
    }
    pub fn queued(&self) -> usize {
        self.backend.queued()
    }
    pub fn tasks_since(&self, previous: Option<u64>) -> Option<(u64, Vec<Task>)> {
        self.backend.tasks_since(previous)
    }
    pub fn compacting(&self) -> bool {
        self.backend.compacting()
    }
    #[cfg(test)]
    pub fn busy(&self) -> bool {
        self.backend.busy()
    }
    pub async fn recv(&mut self) -> Option<SessionEvent> {
        self.backend.recv().await
    }
    pub fn submit(&mut self, input: In, view: &mut View) -> bool {
        if self.backend.submit(input) {
            true
        } else {
            view.notice(Level::Warning, "A session operation is in progress or the input queue is closed; your draft is kept.");
            false
        }
    }
    pub fn reply(&mut self, id: String, payload: serde_json::Value, _view: &mut View) -> Reply {
        self.backend.reply(id, payload)
    }
    pub fn interrupt(&mut self) {
        self.backend.interrupt();
    }
    fn show_error(result: Result<(), String>, view: &mut View) {
        if let Err(message) = result {
            view.notice(Level::Warning, message);
        }
    }
    pub fn steer_pending(&mut self, id: String, view: &mut View) {
        Self::show_error(self.backend.steer_pending(id), view);
    }
    pub fn compact(&mut self, view: &mut View) {
        Self::show_error(self.backend.compact(), view);
    }
    pub fn set_theme(&mut self, theme: &str) -> Result<(), String> {
        self.backend.set_theme(theme)
    }
    pub fn set_yolo(&mut self, enabled: bool) -> Result<(), YourAiError> {
        self.backend.set_yolo(enabled)
    }
    pub fn list(&mut self, view: &mut View) {
        match self.backend.list() {
            Ok(()) => view.overlay = Overlay::LoadingSessions,
            Err(message) => view.notice(Level::Warning, message),
        }
    }
    pub fn delete(&mut self, id: SessionId, view: &mut View) {
        Self::show_error(self.backend.delete(id), view);
    }
    pub fn model(
        &mut self,
        id: String,
        variant: Option<String>,
        effort: Option<crate::models::EffortChoice>,
        view: &mut View,
    ) {
        Self::show_error(self.backend.model(id, variant, effort), view);
    }
    pub fn switch(&mut self, target: Target, view: &mut View) {
        let id = match target {
            Target::New => None,
            Target::Resume(id) => Some(id),
        };
        Self::show_error(self.backend.switch(id), view);
    }
    fn event(&mut self, event: SessionEvent, view: &mut View) -> bool {
        match event {
            SessionEvent::Lagged { skipped } => view.notice(
                Level::Warning,
                format!("Missed {skipped} session events; displayed history may be incomplete. Latest session status is refreshed from the backend."),
            ),
            SessionEvent::Output(event) => view.event(event),
            SessionEvent::Settled => view.settle(),
            SessionEvent::Usage(usage) => view.replace_usage(usage),
            SessionEvent::Listed(result) => {
                let show = matches!(view.overlay, Overlay::LoadingSessions);
                match result {
                    Ok(rows) if show => {
                        view.overlay = Overlay::Sessions(SessionPickerState {
                            pending_delete: None,
                            rows: crate::sessions::rows_from(rows, Some(&self.environment().id)),
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
            SessionEvent::Deleted(id, result) => match result {
                Ok(()) => {
                    if let Overlay::Sessions(picker) = &mut view.overlay {
                        picker.rows.retain(|row| row.id != id);
                        picker.selected = picker.selected.min(
                            crate::sessions::filter_sessions(&picker.rows, &picker.query)
                                .len()
                                .saturating_sub(1),
                        );
                    }
                    view.toast = Some(("Session deleted".into(), Instant::now()));
                }
                Err(e) => view.notice(Level::Error, format!("Delete failed: {e}")),
            },
            SessionEvent::Model(result) => match result {
                Ok(selected) => {
                    let mut message = format!("Model switched to {}", selected.label);
                    if let Some(effort) = &selected.effort {
                        message.push_str(&format!(" · thinking {effort}"));
                    }
                    view.notice(Level::Info, message);
                    if let Ok(config) = self.config.lock() {
                        view.replace_model_choices(crate::models::model_choices(&config));
                    }
                    view.model = selected;
                }
                Err(e) => view.notice(Level::Error, format!("Model switch failed: {e}")),
            },
            SessionEvent::Opened(result) => match result {
                Ok(history) => {
                    let mut fresh = View::default();
                    apply_history(history, &mut fresh);
                    view.session = fresh.session;
                    if let Ok(preferences) = self.config.lock() {
                        let selected = yourai_harness::controller::ModelSelection::from_config(&preferences);
                        if view.model.label != selected.label || view.model.effort != selected.effort {
                            view.replace_model_choices(crate::models::model_choices(&preferences));
                        }
                        view.model = selected;
                    }
                    view.overlay = Overlay::None;
                    self.revision = None;
                    return true;
                }
                Err(message) => view.notice(Level::Error, message),
            },
        }
        false
    }
    /// Apply data only. Calling this method never starts or resumes backend work.
    pub async fn poll(&mut self, view: &mut View, first: Option<SessionEvent>) -> bool {
        let mut switched = first.is_some_and(|event| self.event(event, view));
        for _ in 0..256 {
            match self.backend.try_recv() {
                Some(event) => switched |= self.event(event, view),
                None => break,
            }
        }
        let snapshot = self.backend.snapshot();
        view.session.pending_inputs = snapshot.pending_inputs;
        view.session.can_steer = snapshot.can_steer;
        if snapshot.active && !view.session.active {
            view.session.active = true;
            view.session.since = Some(Instant::now());
        }
        if !snapshot.active
            && !self.backend.has_events()
            && (view.session.active || !view.asks_empty())
        {
            view.idle();
        }
        view.session.model_metrics = snapshot.model_metrics;
        if self.revision != Some(snapshot.revision) {
            self.revision = Some(snapshot.revision);
            view.session.context_usage = snapshot.context_usage;
            view.session.title = snapshot.title;
            if let Some(count) = snapshot.response_count {
                view.set_response_count(count);
            }
            if let Some(error) = snapshot.refresh_error {
                view.toast = Some((error, Instant::now()));
            }
        }
        switched
    }
    pub async fn close(self) -> Result<(String, Vec<In>), Error> {
        self.backend.close().await
    }
}
fn apply_history(history: SessionHistory, view: &mut View) {
    view.restore(history.messages);
    view.restore_usage(history.usage, history.response_count);
    view.session.title = history.title;
    view.settle();
    view.follow();
}

/// An offline controller: real harness, mock provider on an unreachable
/// endpoint, tempdir storage. Shared with the action-layer tests.
#[cfg(test)]
pub(super) async fn fixture() -> (tempfile::TempDir, Controller, View) {
    let dir = tempfile::tempdir().unwrap();
    let cfg: Config = serde_json::from_value(serde_json::json!({
      "models": {
        "selected": "mock/test",
        "providers": {
          "mock": {
            "options": {
              "baseURL": "http://127.0.0.1:1/v1",
              "apiKey": "test"
            },
            "models": {}
          }
        }
      },
      "session": {
        "extensions": false
      }
    }))
    .unwrap();
    let path = dir.path().join("yourai.json");
    std::fs::write(&path, serde_json::to_vec(&cfg).unwrap()).unwrap();
    let cfg = Config::load(&path).unwrap();
    let model = cfg.resolve(None).unwrap().model;
    let mut hc = SessionOptions::new(dir.path().join("sessions"), dir.path().into());
    hc.config.prompt.system_prompt = Some("test".into());
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
        ),
        view,
    )
}

#[cfg(test)]
mod tests {
    use super::fixture;
    use super::{Controller, Target};
    use crate::ui::{
        state::{Item, View},
        theme::Theme,
    };
    use std::time::Duration;
    use yourai_core::prelude::*;

    async fn finish(controller: &mut Controller, view: &mut View) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while controller.backend.busy() {
                controller.poll(view, None).await;
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("session operation did not complete");
        controller.poll(view, None).await;
    }

    #[tokio::test]
    async fn failed_switch_keeps_current_session_and_success_preserves_inflight_draft() {
        let (_dir, mut controller, mut view) = fixture().await;
        let old = controller.harness();
        let id = old.host.environment().id;
        controller.switch(Target::Resume(SessionId::new()), &mut view);
        finish(&mut controller, &mut view).await;
        assert_eq!(controller.harness().host.environment().id, id);
        assert_eq!(old.host.status(), SessionStatus::Idle);
        controller.switch(Target::New, &mut view);
        assert!(controller.backend.operating());
        view.draft.insert("draft typed during opening");
        view.theme = Theme::Nord;
        view.model_choices = vec![crate::models::ModelChoice {
            id: "mock/test".into(),
            variant: Some("fast".into()),
            label: "mock/test · fast".into(),
            effort: None,
        }];
        view.user("belongs to the old session");
        view.session.title = Some("old title".into());
        view.restore_usage(
            Usage {
                input_tokens: 1,
                output_tokens: 2,
                total_tokens: 3,
            },
            1,
        );
        // A second operation cannot supersede the first or mutate its settings.
        controller.model("missing/model".into(), None, None, &mut view);
        finish(&mut controller, &mut view).await;
        assert_ne!(controller.harness().host.environment().id, id);
        assert_eq!(old.host.status(), SessionStatus::Closed);
        assert_eq!(view.draft.text(), "draft typed during opening");
        assert_eq!(view.theme, Theme::Nord);
        assert_eq!(view.model.label, "mock/test");
        assert_eq!(view.model_choices[0].variant.as_deref(), Some("fast"));
        assert!(!view.items().iter().any(
            |item| matches!(item, Item::Text { text, .. } if text == "belongs to the old session")
        ));
        assert!(view.session.title.is_none());
        assert_eq!(view.usage().total_tokens, 0);
        assert_eq!(view.recorded_responses(), 0);
        controller.close().await.unwrap();
    }

    #[tokio::test]
    async fn queued_inputs_guard_both_new_and_resume() {
        let (_dir, mut controller, mut view) = fixture().await;
        controller.harness().host.interrupt();
        // The host queues synchronously; do not yield between submit and guards.
        controller
            .harness()
            .host
            .submit(In::follow_up("queued"))
            .unwrap();
        let id = controller.harness().host.environment().id;
        controller.switch(Target::New, &mut view);
        assert!(!controller.backend.operating());
        controller.switch(Target::Resume(SessionId::new()), &mut view);
        assert!(!controller.backend.operating());
        assert_eq!(controller.harness().host.environment().id, id);
        controller.close().await.unwrap();
    }
    #[tokio::test]
    async fn invalid_attachment_does_not_replace_draft_or_block_session_switch() {
        let (dir, mut controller, mut view) = fixture().await;
        let file = dir.path().join("unsupported.zip");
        std::fs::write(&file, b"unsupported").unwrap();
        let input = In::user_text_with_attachments(
            "inspect @unsupported.zip",
            vec![UserAttachment::file(file.to_string_lossy(), None)],
        );
        assert!(controller.submit(input, &mut view));
        view.draft.insert("new unsent draft");
        let id = controller.harness().host.environment().id;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                controller.poll(&mut view, None).await;
                // Host completion can race with poll's UI-state snapshot.
                // Session switching requires both states to have settled.
                if controller.backend.idle() && !view.session.active && view.items().iter().any(|item| matches!(item, crate::ui::state::Item::Notice { text, .. } if text.starts_with("Input rejected:"))) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(controller.harness().host.queued(), 0);
        assert_eq!(view.draft.text(), "new unsent draft");
        assert_eq!(view.draft.attachment_counts(), (0, 0));
        controller.switch(Target::New, &mut view);
        finish(&mut controller, &mut view).await;
        assert_ne!(controller.harness().host.environment().id, id);
        assert_eq!(view.draft.attachment_counts(), (0, 0));
        let (_, pending) = controller.close().await.unwrap();
        assert!(pending.is_empty());
    }
    #[tokio::test]
    async fn failed_async_submission_preserves_new_draft_and_blocks_session_switch_until_settled() {
        let (_dir, mut controller, mut view) = fixture().await;
        let id = controller.harness().host.environment().id;
        let db = rusqlite::Connection::open(
            controller
                .harness()
                .host
                .environment()
                .transcript_path
                .unwrap(),
        )
        .unwrap();
        db.execute_batch("CREATE TRIGGER fail_input_insert BEFORE INSERT ON session_state WHEN NEW.state_key='runtime' BEGIN SELECT RAISE(ABORT,'injected state failure'); END; CREATE TRIGGER fail_input_update BEFORE UPDATE ON session_state WHEN NEW.state_key='runtime' BEGIN SELECT RAISE(ABORT,'injected state failure'); END;").unwrap();
        assert!(controller.submit(In::user_text("failed input"), &mut view));
        assert!(!controller.backend.idle());
        controller.switch(Target::New, &mut view);
        assert!(!controller.backend.operating());
        view.draft.insert("new draft while storage is pending");
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                controller.poll(&mut view, None).await;
                if controller.backend.idle() && view.items().iter().any(|item| matches!(item, Item::Notice { text, .. } if text.starts_with("Input rejected:"))) { break; }
                tokio::task::yield_now().await;
            }
        }).await.unwrap();
        assert_eq!(view.draft.text(), "new draft while storage is pending");
        assert_eq!(controller.harness().host.queued(), 0);
        assert_eq!(controller.harness().host.environment().id, id);
        db.execute_batch("DROP TRIGGER fail_input_insert; DROP TRIGGER fail_input_update;")
            .unwrap();
        controller.close().await.unwrap();
    }
}

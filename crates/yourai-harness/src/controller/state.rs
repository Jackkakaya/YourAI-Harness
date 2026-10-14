//! Sole owner of admission, queued-input bookkeeping and current-session identity.
//! Parent code can call transitions and queries but cannot write these fields.
use crate::{model::EffortChoice, Harness};
use std::sync::Arc;
use tokio::sync::{mpsc, Notify};
use yourai_core::prelude::{In, Out, SessionId, SessionStatus};

pub(super) enum InputAction {
    Submit(In),
    Steer(String),
}
pub(super) enum Command {
    List,
    Delete(SessionId),
    Model(String, Option<String>, Option<EffortChoice>),
    Switch(Option<SessionId>),
    Compact,
    Refresh,
}
pub(super) struct Shared {
    harness: Arc<Harness>,
    output: mpsc::UnboundedSender<Out>,
    lifecycle: Lifecycle,
    compacting: bool,
    pending: usize,
    restart: bool,
    wake: Arc<Notify>,
}
// Client-visible admission state. Runner task handles own execution, while all
// admission rules and transitions live here under the shared lock.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lifecycle {
    Ready,
    Operating,
    Retiring,
    Closing,
}
#[derive(Clone, Copy)]
pub(super) enum AdmissionError {
    Closed,
    Busy,
}
impl AdmissionError {
    pub(super) fn message(self) -> &'static str {
        match self {
            Self::Closed => "Session backend stopped",
            Self::Busy => "A session operation is in progress. Please wait.",
        }
    }
}
impl Shared {
    pub(super) fn new(
        harness: Arc<Harness>,
        output: mpsc::UnboundedSender<Out>,
        wake: Arc<Notify>,
    ) -> Self {
        Self {
            harness,
            output,
            lifecycle: Lifecycle::Ready,
            compacting: false,
            pending: 0,
            restart: false,
            wake,
        }
    }
    pub(super) fn harness(&self) -> Arc<Harness> {
        self.harness.clone()
    }
    pub(super) fn output(&self) -> mpsc::UnboundedSender<Out> {
        self.output.clone()
    }
    pub(super) fn check_open(&self) -> Result<(), AdmissionError> {
        if self.lifecycle == Lifecycle::Closing {
            Err(AdmissionError::Closed)
        } else {
            Ok(())
        }
    }
    pub(super) fn check_input(&self) -> Result<(), AdmissionError> {
        self.check_open()?;
        // The new session may accept input while the old session retires.
        if self.lifecycle == Lifecycle::Operating {
            Err(AdmissionError::Busy)
        } else {
            Ok(())
        }
    }
    pub(super) fn check_operation(&self) -> Result<(), AdmissionError> {
        self.check_open()?;
        if self.busy() {
            Err(AdmissionError::Busy)
        } else {
            Ok(())
        }
    }
    pub(super) fn busy(&self) -> bool {
        matches!(self.lifecycle, Lifecycle::Operating | Lifecycle::Retiring)
    }
    pub(super) fn idle(&self) -> bool {
        self.pending == 0
            && !self.compacting
            && matches!(self.harness.host.status(), SessionStatus::Idle)
            && self.harness.host.queued() == 0
    }
    pub(super) fn admit(&mut self, command: &Command) -> Result<bool, String> {
        self.check_operation().map_err(|e| e.message().to_owned())?;
        match command {
            Command::Delete(id) if *id == self.harness.host.environment().id => {
                return Err("The current session cannot be deleted.".into());
            }
            Command::Switch(Some(id)) if *id == self.harness.host.environment().id => {
                return Ok(false);
            }
            _ => {}
        }
        if matches!(
            command,
            Command::List | Command::Model(..) | Command::Switch(..) | Command::Compact
        ) && !self.idle()
        {
            return Err("Finish or cancel the current turn, compaction and queued inputs before changing sessions.".into());
        }
        self.lifecycle = Lifecycle::Operating;
        Ok(true)
    }
    pub(super) fn finish_operation(&mut self) {
        if self.lifecycle == Lifecycle::Operating {
            self.lifecycle = Lifecycle::Ready;
        }
    }
    pub(super) fn start_compaction(&mut self) {
        self.compacting = true;
        self.finish_operation();
    }
    pub(super) fn finish_compaction(&mut self) {
        self.compacting = false;
    }
    pub(super) fn retire(&mut self, harness: Arc<Harness>, output: mpsc::UnboundedSender<Out>) {
        self.harness = harness;
        self.output = output;
        self.restart = false;
        // Completion must never reopen a backend whose owner already closed it.
        if self.lifecycle != Lifecycle::Closing {
            self.lifecycle = Lifecycle::Retiring;
        }
    }
    pub(super) fn finish_retirement(&mut self) {
        if self.lifecycle == Lifecycle::Retiring {
            self.lifecycle = Lifecycle::Ready;
        }
    }
    pub(super) fn enqueue(
        &mut self,
        sender: &mpsc::UnboundedSender<InputAction>,
        input: InputAction,
    ) -> Result<(), String> {
        let resume = matches!(input, InputAction::Submit(_));
        if resume {
            self.check_input()
        } else {
            self.check_operation()
        }
        .map_err(|e| e.message().to_owned())?;
        self.pending += 1;
        if sender.send(input).is_err() {
            self.pending -= 1;
            return Err("Input queue closed".into());
        }
        if resume {
            self.request_resume();
        }
        Ok(())
    }
    pub(super) fn finish_input(&mut self) {
        self.pending -= 1;
        self.wake.notify_one();
    }
    pub(super) fn request_resume(&mut self) {
        self.restart = true;
        self.wake.notify_one();
    }
    pub(super) fn resume_started(&mut self) {
        self.restart = false;
    }
    pub(super) fn interrupt(&mut self) {
        self.restart = false;
        self.harness.host.interrupt();
    }
    pub(super) fn take_resume(&mut self) -> bool {
        if !self.restart
            || self.pending != 0
            || self.compacting
            || !matches!(self.harness.host.status(), SessionStatus::Idle)
        {
            return false;
        }
        self.restart = false;
        self.harness.host.queued() > 0
    }
    pub(super) fn submitting(&self) -> bool {
        self.pending != 0
    }
    pub(super) fn operating(&self) -> bool {
        self.lifecycle == Lifecycle::Operating
    }
    pub(super) fn compacting(&self) -> bool {
        self.compacting
    }
    pub(super) fn close(&mut self) {
        self.lifecycle = Lifecycle::Closing;
    }
}

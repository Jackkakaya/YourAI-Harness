//! Internal events are separate from user input and never impersonate approval replies.
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashSet, VecDeque},
    sync::Mutex,
};
use tokio::sync::Notify;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeEvent {
    pub id: String,
    pub context: Option<String>,
    pub notice: Option<String>,
    pub wake: bool,
}
#[derive(Debug, Default)]
pub struct RuntimeEvents {
    state: Mutex<(VecDeque<RuntimeEvent>, HashSet<String>)>,
    pub changed: Notify,
}
impl RuntimeEvents {
    pub fn push(&self, event: RuntimeEvent) -> bool {
        let mut s = self.state.lock().unwrap();
        if !s.1.insert(event.id.clone()) {
            return false;
        }
        s.0.push_back(event);
        self.changed.notify_one();
        true
    }
    pub fn front(&self) -> Option<RuntimeEvent> {
        self.state.lock().unwrap().0.front().cloned()
    }
    pub fn ack(&self, id: &str) {
        let mut s = self.state.lock().unwrap();
        if s.0.front().is_some_and(|e| e.id == id) {
            s.0.pop_front();
        }
    }
    pub fn pending(&self) -> Vec<RuntimeEvent> {
        self.state.lock().unwrap().0.iter().cloned().collect()
    }
    pub fn has_context(&self) -> bool {
        self.state
            .lock()
            .unwrap()
            .0
            .iter()
            .any(|e| e.context.is_some())
    }
}

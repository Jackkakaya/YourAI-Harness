//! Stable queue identity belongs to the host, not the turn input protocol.
use super::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingInput {
    pub id: String,
    pub input: In,
}
impl From<In> for PendingInput {
    fn from(input: In) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            input,
        }
    }
}

// Old journals contain bare In values. Upgrade them on read; subsequent commits
// preserve the assigned identities. The turn protocol itself remains unchanged.
pub(super) fn deserialize_queue<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<VecDeque<PendingInput>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Entry {
        Current(PendingInput),
        Legacy(In),
    }
    Ok(Vec::<Entry>::deserialize(d)?
        .into_iter()
        .map(|e| match e {
            Entry::Current(e) => e,
            Entry::Legacy(input) => input.into(),
        })
        .collect())
}
impl SessionHost {
    pub fn pending_inputs(&self) -> Vec<PendingInput> {
        self.live
            .lock()
            .unwrap()
            .journal
            .queue
            .iter()
            .cloned()
            .collect()
    }

    /// Move a still-pending message into the current turn at its next input
    /// boundary. A stale click is a no-op; never substitute a different entry.
    pub async fn steer_pending(&self, id: String) -> Result<bool, YourAiError> {
        self.blocking(move |host| {
            let mut journal = host.journal();
            host.ensure_open()?;
            let Some(index) = journal.queue.iter().position(|e| e.id == id) else {
                return Ok(false);
            };
            let inbox = {
                let live = host.live.lock().unwrap();
                if !matches!(live.status, SessionStatus::Running { .. }) {
                    return Ok(false);
                }
                let Some(inbox) = live.inbox.clone() else {
                    return Ok(false);
                };
                inbox
            };
            let entry = journal.queue.remove(index).unwrap();
            let mut input = entry.input.clone();
            let In::UserText { mode, .. } = &mut input else {
                return Ok(false);
            };
            *mode = InputMode::Steer;
            journal.active.push(input.clone());
            // Commit ownership before sending, exactly like ordinary steer admission.
            journal.commit()?;
            if inbox.send(input).is_err() {
                journal.active.pop();
                journal.queue.insert(index, entry);
                if let Err(error) = journal.commit() {
                    journal.retain_for_recovery(&error);
                    return Err(error);
                }
                return Ok(false);
            }
            Ok(true)
        })
        .await?
    }
}

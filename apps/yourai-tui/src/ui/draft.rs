//! The complete unsent message and its lifetime. Callers can read presentation
//! state, but every mutation goes through this module so text, mentions,
//! attachments and asynchronous work cannot acquire independent lifetimes.
use super::{
    clipboard,
    editor::Editor,
    mention::{self, MentionState},
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::path::{Path, PathBuf};
use tokio::task::JoinHandle;
use yourai_core::prelude::*;

#[derive(Clone)]
struct PendingAttachment {
    // None means explicitly staged, independent of text (clipboard).
    marker: Option<String>,
    attachment: UserAttachment,
}

/// One editable input, including work that must follow it while browsing history.
#[derive(Default)]
struct Input {
    editor: Editor,
    attachments: Vec<PendingAttachment>,
    image: Option<JoinHandle<std::io::Result<Option<clipboard::ClipboardImage>>>>,
}
impl Input {
    fn snapshot(&self) -> Self {
        Self {
            editor: self.editor.clone(),
            attachments: self.attachments.clone(),
            image: None,
        }
    }
    fn cancel_image(&mut self) {
        if let Some(task) = self.image.take() {
            task.abort();
        }
    }
}
impl Drop for Input {
    fn drop(&mut self) {
        self.cancel_image();
    }
}

#[derive(Default)]
pub(super) struct Draft {
    current: Input,
    history: Vec<Input>,
    position: Option<usize>,
    scratch: Option<Input>,
    mention: MentionState,
    cwd: Option<PathBuf>,
    scan: Option<mention::Scan>,
}

impl Draft {
    pub fn editor(&self) -> &Editor {
        &self.current.editor
    }
    pub fn mention(&self) -> &MentionState {
        &self.mention
    }
    pub fn text(&self) -> &str {
        &self.current.editor.text
    }
    pub fn reading_image(&self) -> bool {
        self.current.image.is_some()
    }
    pub fn is_empty(&self) -> bool {
        self.text().trim().is_empty() && self.attachment_counts() == (0, 0) && !self.reading_image()
    }
    pub fn set_cwd(&mut self, cwd: &Path) {
        if self.cwd.as_deref() != Some(cwd) {
            self.cancel_scan();
            self.mention.deactivate();
            self.cwd = Some(cwd.to_owned());
            self.sync_mention();
        }
    }
    pub fn insert(&mut self, text: &str) {
        self.current.editor.insert(text);
        self.changed();
    }
    pub fn key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('p') if ctrl => self.history(true),
            KeyCode::Char('n') if ctrl => self.history(false),
            KeyCode::Up if self.current.editor.on_first_line() => self.history(true),
            KeyCode::Down if self.current.editor.on_last_line() => self.history(false),
            _ => {
                self.current.editor.key(key);
                self.changed();
            }
        }
    }
    /// Slash-command completion/consumption edits text, preserving explicitly
    /// staged images. Mention references disappear with their markers.
    pub fn set_text(&mut self, text: &str) {
        self.current.editor.take();
        self.current.editor.insert(text);
        self.changed();
    }
    /// Seed input history from restored conversation text at startup.
    pub fn remember(&mut self, text: &str) {
        let mut input = Input::default();
        input.editor.insert(text);
        self.history.push(input);
        self.trim_history();
    }
    /// Keep a complete input once it is accepted into the host queue. Its
    /// eventual outcome never adds a second copy or changes the current draft.
    pub fn submitted(&mut self) {
        self.history.push(self.current.snapshot());
        self.trim_history();
        self.current = self.scratch.take().unwrap_or_default();
        self.position = None;
        self.cancel_scan();
        self.mention.deactivate();
    }
    fn trim_history(&mut self) {
        if self.history.len() > 100 {
            self.history.remove(0);
        }
    }
    fn history(&mut self, previous: bool) {
        if self.history.is_empty() {
            return;
        }
        if previous {
            let index = self
                .position
                .map_or(self.history.len() - 1, |i| i.saturating_sub(1));
            if self.position.is_none() {
                self.scratch = Some(std::mem::take(&mut self.current));
            }
            self.current = self.history[index].snapshot();
            self.position = Some(index);
        } else if let Some(index) = self.position {
            if index + 1 < self.history.len() {
                self.current = self.history[index + 1].snapshot();
                self.position = Some(index + 1);
            } else {
                self.current = self.scratch.take().unwrap_or_default();
                self.position = None;
            }
        } else {
            return;
        }
        // Recall is not a newly typed mention. Do not make a popup steal the
        // next Up/Down, and do not apply a scan from the input we just left.
        self.cancel_scan();
        self.mention.deactivate();
    }
    pub fn attachments(&self) -> Vec<UserAttachment> {
        self.current
            .attachments
            .iter()
            .map(|a| a.attachment.clone())
            .collect()
    }
    pub fn attachment_counts(&self) -> (usize, usize) {
        self.current
            .attachments
            .iter()
            .fold((0, 0), |(images, refs), a| match a.attachment.data {
                AttachmentData::Base64(_) => (images + 1, refs),
                AttachmentData::File(_) => (images, refs + 1),
            })
    }
    /// Only completion/navigation/attachment actions consume an event.
    /// Ordinary keys must reach `key`, including while a scan is pending.
    pub fn intercept(&mut self, key: KeyEvent) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('v') {
            self.read_image();
            return true;
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return false;
        }
        if self.mention.active {
            match key.code {
                KeyCode::Up => self.mention.step(true),
                KeyCode::Down => self.mention.step(false),
                KeyCode::Esc => {
                    self.cancel_scan();
                    self.mention.deactivate();
                }
                KeyCode::Tab | KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                    // While loading, Enter cannot accidentally submit an incomplete
                    // reference. An empty settled result does not trap submission.
                    if let Some(entry) = self.mention.current().cloned() {
                        if key.code == KeyCode::Tab && entry.is_dir {
                            self.expand_mention_dir(&entry.display);
                        } else {
                            self.accept_mention(self.mention.selected);
                        }
                    } else if self.scan.is_none() && key.code == KeyCode::Enter {
                        self.mention.deactivate();
                        return false;
                    }
                }
                _ => return false,
            }
            return true;
        }
        if key.code == KeyCode::Esc
            && (!self.current.attachments.is_empty() || self.current.image.is_some())
        {
            self.clear_attachments();
            return true;
        }
        false
    }
    /// Accept the mention entry at `index`: replace the `@query` with the file
    /// reference and stage its attachment. Shared by Enter and mouse clicks.
    pub fn accept_mention(&mut self, index: usize) {
        let Some(entry) = self.mention.entries.get(index).cloned() else {
            return;
        };
        let anchor = self.mention.anchor;
        let end = anchor + 1 + self.mention.query.len();
        self.cancel_scan();
        self.mention.deactivate();
        let marker = format!("@{}", entry.display);
        self.current
            .editor
            .replace_range(anchor..end, &format!("{marker} "));
        self.current
            .attachments
            .retain(|a| a.marker.as_ref() != Some(&marker));
        self.current.attachments.push(PendingAttachment {
            marker: Some(marker),
            attachment: UserAttachment::file(entry.path.to_string_lossy(), None),
        });
        self.changed();
    }
    /// Tab on a directory: extend the reference instead of completing it.
    fn expand_mention_dir(&mut self, display: &str) {
        let anchor = self.mention.anchor;
        let end = anchor + 1 + self.mention.query.len();
        self.cancel_scan();
        self.mention.deactivate();
        self.current
            .editor
            .replace_range(anchor..end, &format!("@{display}/"));
        self.changed();
    }
    pub fn read_image(&mut self) {
        self.current.cancel_image();
        self.current.image = Some(tokio::spawn(clipboard::read_image()));
    }
    pub fn clear_attachments(&mut self) {
        self.current.attachments.clear();
        self.current.cancel_image();
    }
    pub fn stage_image(&mut self, image: clipboard::ClipboardImage) {
        let n = self.current.attachments.len() + 1;
        self.current.attachments.push(PendingAttachment {
            marker: None,
            attachment: UserAttachment::base64(
                image.mime,
                image.data,
                Some(format!("clipboard-{n}.png")),
            ),
        });
    }
    /// No detached completion can write to a replaced draft: the task handle
    /// moves with this object and is taken/aborted on reset or drop.
    pub async fn poll(&mut self) -> Option<(Level, String)> {
        if self.scan.as_ref().is_some_and(|t| t.is_finished()) {
            if let Ok((anchor, query, entries)) = self.scan.take().unwrap().finish().await {
                if self.mention.active
                    && self.mention.anchor == anchor
                    && self.mention.query == query
                {
                    self.mention.entries = entries;
                    self.mention.selected = 0;
                } else {
                    self.start_scan();
                }
            }
        }
        if !self.current.image.as_ref().is_some_and(|t| t.is_finished()) {
            return None;
        }
        match self.current.image.take().unwrap().await {
            Ok(Ok(Some(img))) => {
                self.stage_image(img);
                None
            }
            Ok(Ok(None)) => Some((Level::Info, "No image in clipboard.".into())),
            Ok(Err(e)) => Some((Level::Error, format!("Clipboard read failed: {e}"))),
            Err(e) => Some((Level::Error, format!("Clipboard read failed: {e}"))),
        }
    }
    fn changed(&mut self) {
        let text = &self.current.editor.text;
        self.current
            .attachments
            .retain(|a| a.marker.as_ref().is_none_or(|m| marker_present(text, m)));
        self.sync_mention();
    }
    fn sync_mention(&mut self) {
        if let Some((anchor, query)) =
            MentionState::detect(&self.current.editor.text, self.current.editor.cursor)
        {
            if !self.mention.active || self.mention.anchor != anchor || self.mention.query != query
            {
                self.mention.activate(anchor, &query);
                self.mention.entries.clear();
                // Coalesce query changes while the current filesystem walk runs.
                // Its completion will start just the newest query if stale.
                if self.scan.is_none() {
                    self.start_scan();
                }
            }
        } else {
            self.cancel_scan();
            self.mention.deactivate();
        }
    }
    fn start_scan(&mut self) {
        if self.mention.active {
            if let Some(cwd) = &self.cwd {
                self.scan = Some(mention::Scan::start(
                    cwd,
                    self.mention.anchor,
                    self.mention.query.clone(),
                ));
            }
        }
    }
    fn cancel_scan(&mut self) {
        self.scan = None; // Scan::drop cooperatively cancels the filesystem walk.
    }
}
/// Word-boundary-aware marker search: `@src` must not match inside
/// `@src/main.rs`, and the match must start at a word boundary like the
/// mention trigger itself.
fn marker_present(text: &str, marker: &str) -> bool {
    let mut from = 0;
    while let Some(offset) = text[from..].find(marker) {
        let start = from + offset;
        let end = start + marker.len();
        let start_ok = start == 0
            || text[..start]
                .chars()
                .next_back()
                .is_none_or(|c| c.is_whitespace());
        let end_ok = text[end..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric() && !matches!(c, '/' | '_' | '-' | '.'));
        if start_ok && end_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

#[cfg(test)]
mod tests;

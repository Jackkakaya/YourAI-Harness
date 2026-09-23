//! Exclusive modal state and input routing. Actions are executed by the host loop.
use super::{state::SessionPickerState, theme::Theme};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use yourai_core::prelude::SessionId;

#[derive(Default, Debug)]
pub enum Overlay {
    #[default]
    None,
    Help {
        scroll: u16,
    },
    Stats {
        scroll: u16,
    },
    Models(usize),
    Sessions(SessionPickerState),
    Themes(usize),
}
#[derive(Debug)]
pub enum Action {
    None,
    Model(usize),
    Theme(Theme),
    Session(SessionId),
    Delete(SessionId),
}
#[derive(PartialEq, Eq)]
pub enum Snapshot {
    None,
    Help(u16),
    Stats(u16),
    Models(usize),
    Themes(usize),
    Sessions {
        query: String,
        selected: usize,
        rows: usize,
        pending_delete: Option<SessionId>,
    },
}
impl Overlay {
    pub fn is_open(&self) -> bool {
        !matches!(self, Self::None)
    }
    /// Frame invalidation excludes immutable session rows.
    pub fn snapshot(&self) -> Snapshot {
        match self {
            Self::None => Snapshot::None,
            Self::Help { scroll } => Snapshot::Help(*scroll),
            Self::Stats { scroll } => Snapshot::Stats(*scroll),
            Self::Models(i) => Snapshot::Models(*i),
            Self::Themes(i) => Snapshot::Themes(*i),
            Self::Sessions(s) => Snapshot::Sessions {
                query: s.query.clone(),
                selected: s.selected,
                rows: s.rows.len(),
                pending_delete: s.pending_delete.as_ref().map(|r| r.id.clone()),
            },
        }
    }
    #[cfg(test)]
    pub fn sessions_mut(&mut self) -> Option<&mut SessionPickerState> {
        match self {
            Self::Sessions(s) => Some(s),
            _ => None,
        }
    }
    pub fn paste(&mut self, text: &str) {
        if let Self::Sessions(s) = self {
            if s.pending_delete.is_none() {
                s.query
                    .push_str(&super::state::clean(text).replace(['\n', '\r'], " "));
                s.selected = 0;
            }
        }
    }
    /// Some means the modal consumed the key, even when it has no action.
    pub fn key(&mut self, key: KeyEvent, model_count: usize) -> Option<Action> {
        if !self.is_open() {
            return None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if let Self::Sessions(s) = self {
            if let Some(row) = &s.pending_delete {
                let action = match key.code {
                    KeyCode::Char('y' | 'Y')
                        if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
                    {
                        Action::Delete(row.id.clone())
                    }
                    KeyCode::Esc | KeyCode::Char('n' | 'N') => Action::None,
                    _ => return Some(Action::None),
                };
                s.pending_delete = None;
                return Some(action);
            }
        }
        if key.code == KeyCode::Esc {
            *self = Self::None;
            return Some(Action::None);
        }
        let up = key.code == KeyCode::Up || (ctrl && key.code == KeyCode::Char('p'));
        let down = key.code == KeyCode::Down || (ctrl && key.code == KeyCode::Char('n'));
        let mut action = Action::None;
        match self {
            Self::Help { .. } if matches!(key.code, KeyCode::F(1) | KeyCode::Enter) => {
                *self = Self::None
            }
            Self::Stats { .. } if ctrl && key.code == KeyCode::Char('b') => *self = Self::None,
            Self::Stats { scroll } | Self::Help { scroll } => match key.code {
                KeyCode::Up => *scroll = scroll.saturating_sub(1),
                KeyCode::Down => *scroll = scroll.saturating_add(1),
                KeyCode::PageUp => *scroll = scroll.saturating_sub(5),
                KeyCode::PageDown => *scroll = scroll.saturating_add(5),
                KeyCode::Home => *scroll = 0,
                _ => {}
            },
            Self::Models(index) | Self::Themes(index) if up => *index = index.saturating_sub(1),
            Self::Models(index) if down => *index = (*index + 1).min(model_count.saturating_sub(1)),
            Self::Themes(index) if down => *index = (*index + 1).min(Theme::ALL.len() - 1),
            Self::Models(index) if key.code == KeyCode::Enter && *index < model_count => {
                action = Action::Model(*index);
                *self = Self::None;
            }
            Self::Themes(index) if key.code == KeyCode::Enter => {
                if let Some(theme) = Theme::ALL.get(*index) {
                    action = Action::Theme(*theme);
                }
                *self = Self::None;
            }
            Self::Sessions(s) => {
                let filtered = crate::sessions::filter_sessions(&s.rows, &s.query);
                if up {
                    s.selected = s.selected.saturating_sub(1);
                } else if down {
                    s.selected = (s.selected + 1).min(filtered.len().saturating_sub(1));
                } else {
                    match key.code {
                        KeyCode::Char('d') if ctrl => {
                            if let Some(row) = filtered.get(s.selected).map(|i| &s.rows[*i]) {
                                if !row.is_current {
                                    s.pending_delete = Some(row.clone());
                                }
                            }
                        }
                        KeyCode::Enter => {
                            if let Some(row) = filtered.get(s.selected).map(|i| &s.rows[*i]) {
                                action = Action::Session(row.id.clone());
                                *self = Self::None;
                            }
                        }
                        KeyCode::Backspace => {
                            s.query.pop();
                            s.selected = 0;
                        }
                        KeyCode::Char(c)
                            if !key
                                .modifiers
                                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                        {
                            s.query.push(c);
                            s.selected = 0;
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        Some(action)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn picker() -> Overlay {
        Overlay::Sessions(SessionPickerState {
            rows: vec![crate::sessions::SessionRow {
                id: SessionId("old".into()),
                title: "Previous task".into(),
                model: String::new(),
                updated_at: 0,
                is_current: false,
            }],
            query: String::new(),
            selected: 0,
            pending_delete: None,
        })
    }
    #[test]
    fn modal_escape_consumes_input_and_empty_model_enter_is_safe() {
        let mut modal = Overlay::Stats { scroll: 0 };
        assert!(matches!(
            modal.key(key(KeyCode::Char('x')), 0),
            Some(Action::None)
        ));
        assert!(matches!(
            modal.key(key(KeyCode::Esc), 0),
            Some(Action::None)
        ));
        assert!(!modal.is_open());
        assert!(modal.key(key(KeyCode::Esc), 0).is_none());
        modal = Overlay::Models(0);
        assert!(matches!(
            modal.key(key(KeyCode::Enter), 0),
            Some(Action::None)
        ));
        assert!(modal.is_open());
    }
    #[test]
    fn delete_requires_explicit_yes_and_paste_stays_in_search() {
        let mut modal = picker();
        modal.paste("Previous\n");
        assert_eq!(modal.sessions_mut().unwrap().query, "Previous ");
        modal.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL), 0);
        assert!(modal.sessions_mut().unwrap().pending_delete.is_some());
        assert!(matches!(
            modal.key(key(KeyCode::Enter), 0),
            Some(Action::None)
        ));
        modal.key(key(KeyCode::Esc), 0);
        assert!(modal.is_open());
        assert!(modal.sessions_mut().unwrap().pending_delete.is_none());
        modal.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL), 0);
        assert!(
            matches!(modal.key(key(KeyCode::Char('y')),0),Some(Action::Delete(SessionId(id))) if id=="old")
        );
        modal.sessions_mut().unwrap().rows[0].is_current = true;
        modal.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL), 0);
        assert!(modal.sessions_mut().unwrap().pending_delete.is_none());
    }
}

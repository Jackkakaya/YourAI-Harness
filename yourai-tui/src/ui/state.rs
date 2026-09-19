use super::editor::Editor;
use serde_json::{json, Value};
use std::{
    collections::{HashSet, VecDeque},
    time::Instant,
};
use yourai_core::prelude::*;
const MAX_TEXT: usize = 32_000;
const MAX_ITEMS: usize = 1000;
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    User,
    Assistant,
    Thinking,
    Context,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ToolStatus {
    Running,
    Done,
    Failed,
    Interrupted,
}
pub struct ToolView {
    pub id: String,
    pub name: String,
    pub input: String,
    pub summary: String,
    pub output: String,
    pub progress: String,
    pub status: ToolStatus,
    pub started: Option<Instant>,
    pub seconds: Option<u64>,
}
pub enum Item {
    Text { role: Role, text: String },
    Tool(ToolView),
    Notice { level: Level, text: String },
}
pub struct Ask {
    pub id: String,
    pub payload: Value,
    pub details: String,
    pub editor: Editor,
    pub error: Option<String>,
    pub scroll: usize,
}
impl Ask {
    pub fn permission(&self) -> bool {
        self.payload["kind"] == "permission"
    }
    pub fn answer(&self) -> Result<Value, String> {
        let text = self.editor.text.trim();
        if self.permission() {
            return match text.to_lowercase().as_str() {
                "y" | "yes" => Ok(json!({"behavior":"allow"})),
                "n" | "no" => Ok(json!({"behavior":"deny"})),
                _ => Err("Enter y to allow once, or n to deny.".into()),
            };
        }
        if text.is_empty() {
            return Err("Enter a reply.".into());
        }
        // Schema/MCP payloads keep the existing JSON contract; plain questions accept text.
        if self.payload.get("requested_schema").is_some()
            || self.payload.get("requestedSchema").is_some()
            || self.payload.get("mode").is_some()
            || text.starts_with("/json ")
        {
            serde_json::from_str(text.strip_prefix("/json ").unwrap_or(text))
                .map_err(|_| "Enter valid JSON; MCP expects an action/content object.".into())
        } else {
            Ok(Value::String(text.into()))
        }
    }
}
#[derive(Clone, PartialEq, Eq)]
pub struct Todo {
    pub id: String,
    pub text: String,
    pub completed: bool,
}

pub struct View {
    pub model_metrics: yourai_harness::model::BudgetSnapshot,
    pub recorded_responses: u64,
    pub commands: super::commands::Menu,
    pub toast: Option<(String, Instant)>,
    pub theme: super::theme::Theme,
    pub context_usage: Option<yourai_harness::runtime::ContextUsage>,
    pub items: VecDeque<Item>,
    pub editor: Editor,
    pub asks: VecDeque<Ask>,
    assistant: Option<usize>,
    thinking: Option<usize>,
    pub scroll: usize,
    first_item_id: u64,
    pub expanded: HashSet<u64>,
    pub selected: Option<u64>,
    pub todos: Vec<Todo>,
    pub todos_expanded: bool,
    pub todo_scroll: usize,
    pub sidebar: bool,
    pub help: bool,
    pub usage: Usage,
    pub unseen: usize,
    pub revision: u64,
    pub active: bool,
    pub since: Option<Instant>,
}
impl Default for View {
    fn default() -> Self {
        Self {
            model_metrics: Default::default(),
            recorded_responses: 0,
            commands: Default::default(),
            toast: None,
            theme: Default::default(),
            context_usage: None,
            items: VecDeque::new(),
            editor: Editor::default(),
            asks: VecDeque::new(),
            assistant: None,
            thinking: None,
            scroll: 0,
            first_item_id: 0,
            expanded: HashSet::new(),
            selected: None,
            todos: vec![],
            todos_expanded: true,
            todo_scroll: 0,
            sidebar: true,
            help: false,
            usage: Usage::default(),
            unseen: 0,
            revision: 0,
            active: false,
            since: None,
        }
    }
}
impl View {
    pub fn model_activity(&self) -> &'static str {
        if self.assistant.is_some() {
            "Responding"
        } else if self.thinking.is_some() {
            "Thinking"
        } else {
            "Waiting for model"
        }
    }

    pub fn item_id(&self, index: usize) -> u64 {
        self.first_item_id + index as u64
    }
    pub fn foldable(item: &Item) -> bool {
        matches!(
            item,
            Item::Tool(_)
                | Item::Text {
                    role: Role::Thinking,
                    ..
                }
        )
    }
    pub fn toggle(&mut self, id: u64) {
        let Some(index) = id.checked_sub(self.first_item_id).map(|i| i as usize) else {
            return;
        };
        if !self.items.get(index).is_some_and(Self::foldable) {
            return;
        }
        if !self.expanded.remove(&id) {
            self.expanded.insert(id);
        }
        self.selected = Some(id);
        self.touch();
    }
    pub fn toggle_recent(&mut self, thinking: bool) {
        let matches = |item: &Item| {
            if thinking {
                matches!(
                    item,
                    Item::Text {
                        role: Role::Thinking,
                        ..
                    }
                )
            } else {
                Self::foldable(item)
            }
        };
        let selected = self
            .selected
            .and_then(|id| id.checked_sub(self.first_item_id))
            .map(|i| i as usize)
            .filter(|i| self.items.get(*i).is_some_and(matches));
        if let Some(index) = selected.or_else(|| self.items.iter().rposition(matches)) {
            self.toggle(self.item_id(index));
        }
    }
    pub fn select_next(&mut self, backwards: bool) {
        let ids: Vec<_> = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, i)| Self::foldable(i))
            .map(|(i, _)| self.item_id(i))
            .collect();
        if ids.is_empty() {
            return;
        }
        let current = self
            .selected
            .and_then(|id| ids.iter().position(|i| *i == id));
        let next = match current {
            Some(i) if backwards => (i + ids.len() - 1) % ids.len(),
            Some(i) => (i + 1) % ids.len(),
            None if backwards => ids.len() - 1,
            None => 0,
        };
        self.selected = Some(ids[next]);
        self.touch();
    }
    pub fn clear_timeline(&mut self) {
        self.first_item_id += self.items.len() as u64;
        self.items.clear();
        self.expanded.clear();
        self.selected = None;
        self.settle();
        self.follow();
    }
    pub fn set_todos(&mut self, todos: Vec<Todo>) {
        if self.todos != todos {
            self.todos = todos;
            self.touch();
        }
    }

    pub fn touch(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        if self.scroll > 0 {
            self.unseen = self.unseen.saturating_add(1);
        }
    }
    fn push(&mut self, item: Item) {
        self.items.push_back(item);
        if self.items.len() > MAX_ITEMS {
            self.items.pop_front();
            self.expanded.remove(&self.first_item_id);
            if self.selected == Some(self.first_item_id) {
                self.selected = None;
            }
            self.first_item_id += 1;
            self.assistant = self.assistant.and_then(|n| n.checked_sub(1));
            self.thinking = self.thinking.and_then(|n| n.checked_sub(1));
        }
        self.touch();
    }
    pub fn notice(&mut self, level: Level, text: impl Into<String>) {
        self.push(Item::Notice {
            level,
            text: bounded(&text.into()),
        });
    }
    pub fn user(&mut self, text: &str, queued: bool) {
        self.push(Item::Text {
            role: Role::User,
            text: bounded(&if queued {
                format!("[queued] {text}")
            } else {
                text.into()
            }),
        });
    }
    fn delta(&mut self, role: Role, text: &str) {
        let index = if role == Role::Assistant {
            self.assistant
        } else {
            self.thinking
        };
        if let Some(i) = index {
            if let Some(Item::Text { text: body, .. }) = self.items.get_mut(i) {
                append(body, text);
                self.touch();
                return;
            }
        }
        self.push(Item::Text {
            role,
            text: bounded(text),
        });
        let i = Some(self.items.len() - 1);
        if role == Role::Assistant {
            self.assistant = i;
        } else {
            self.thinking = i;
        }
    }
    /// Host status and forwarded output use separate channels. Idle may arrive before
    /// the final Message, so a status refresh must retain stream merge identities.
    pub fn idle(&mut self) {
        self.active = false;
        self.since = None;
        self.asks.clear();
    }
    pub fn settle(&mut self) {
        self.assistant = None;
        self.thinking = None;
        self.asks.clear();
        self.active = false;
        self.since = None;
        for item in &mut self.items {
            if let Item::Tool(t) = item {
                if t.status == ToolStatus::Running {
                    t.status = ToolStatus::Interrupted;
                    t.seconds = t.started.map(|s| s.elapsed().as_secs());
                }
            }
        }
        self.touch();
    }
    pub fn follow(&mut self) {
        self.scroll = 0;
        self.unseen = 0;
    }
    pub fn event(&mut self, event: Out) {
        match event {
            Out::Chunk { text } => self.delta(Role::Assistant, &text),
            Out::Reasoning { text } => self.delta(Role::Thinking, &text),
            Out::Message { text } => {
                if let Some(i) = self.assistant.take() {
                    if let Some(Item::Text { text: body, .. }) = self.items.get_mut(i) {
                        *body = bounded(&text);
                    }
                } else {
                    self.push(Item::Text {
                        role: Role::Assistant,
                        text: bounded(&text),
                    });
                }
                self.thinking = None;
                self.touch();
            }
            Out::ToolStarted { id, name, input } => {
                self.assistant = None;
                self.thinking = None;
                self.push(Item::Tool(ToolView {
                    id,
                    name,
                    summary: bounded(
                        input
                            .get("command")
                            .or_else(|| input.get("path"))
                            .or_else(|| input.get("subject"))
                            .or_else(|| input.get("action"))
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                    ),
                    input: pretty(&input),
                    output: String::new(),
                    progress: String::new(),
                    status: ToolStatus::Running,
                    started: Some(Instant::now()),
                    seconds: None,
                }));
            }
            Out::ToolProgress { id, payload } => {
                if let Some(t) = self.tool_mut(&id) {
                    if payload.get("stdout").is_some() || payload.get("stderr").is_some() {
                        for key in ["stdout", "stderr"] {
                            if let Some(s) = payload[key].as_str() {
                                append_progress(&mut t.progress, s);
                            }
                        }
                    } else {
                        append_progress(&mut t.progress, &format!("{}\n", pretty(&payload)));
                    }
                    self.touch();
                } else {
                    self.notice(Level::Info, format!("Progress {id}: {}", pretty(&payload)));
                }
            }
            Out::ToolDone {
                id,
                name,
                output,
                is_error,
            } => {
                let failed = is_error || (name == "shell" && output["ok"] == false);
                let body = tool_output(&name, &output);
                if let Some(t) = self.tool_mut(&id) {
                    t.output = body;
                    t.progress.clear();
                    t.status = if failed {
                        ToolStatus::Failed
                    } else {
                        ToolStatus::Done
                    };
                    t.seconds = t.started.map(|s| s.elapsed().as_secs());
                } else {
                    self.push(Item::Tool(ToolView {
                        id: id.clone(),
                        name,
                        input: String::new(),
                        summary: String::new(),
                        output: body,
                        progress: String::new(),
                        status: if failed {
                            ToolStatus::Failed
                        } else {
                            ToolStatus::Done
                        },
                        started: None,
                        seconds: None,
                    }));
                }
                self.asks
                    .retain(|a| a.payload["call_id"].as_str() != Some(&id));
                self.touch();
            }
            Out::Ask { id, payload } => {
                if !self.asks.iter().any(|a| a.id == id) {
                    self.asks.push_back(Ask {
                        id,
                        details: pretty(&payload),
                        payload,
                        editor: Editor::default(),
                        error: None,
                        scroll: 0,
                    });
                }
                self.touch();
            }
            Out::Usage { usage } => {
                self.usage.input_tokens =
                    self.usage.input_tokens.saturating_add(usage.input_tokens);
                self.usage.output_tokens =
                    self.usage.output_tokens.saturating_add(usage.output_tokens);
                self.usage.total_tokens =
                    self.usage.total_tokens.saturating_add(usage.total_tokens);
            }
            Out::Notice { level, message } => self.notice(level, message),
            _ => {}
        }
    }
    fn tool_mut(&mut self, id: &str) -> Option<&mut ToolView> {
        self.items.iter_mut().rev().find_map(|i| match i {
            Item::Tool(t) if t.id == id => Some(t),
            _ => None,
        })
    }
    pub fn restore(&mut self, rows: Vec<StoredMessage>) {
        for row in rows {
            if row.summary || row.runtime_context {
                continue;
            }
            let m = row.message;
            let text = m.content.texts().join("\n");
            if !text.is_empty() {
                let role = match m.role {
                    ChatRole::User => Role::User,
                    ChatRole::Assistant => Role::Assistant,
                    _ => Role::Context,
                };
                if role == Role::User {
                    self.editor.remember(&text);
                }
                self.push(Item::Text {
                    role,
                    text: bounded(&text),
                });
            }
            for part in m.content.parts() {
                if let ContentPart::ReasoningContent(reasoning) = part {
                    self.push(Item::Text {
                        role: Role::Thinking,
                        text: bounded(reasoning),
                    });
                }
            }
            for c in m.content.tool_calls() {
                self.event(Out::ToolStarted {
                    id: c.call_id.clone(),
                    name: c.fn_name.clone(),
                    input: c.fn_arguments.clone(),
                });
                if let Some(t) = self.tool_mut(&c.call_id) {
                    t.started = None;
                }
            }
            for r in m.content.tool_responses() {
                let output: Value =
                    serde_json::from_str(&r.content).unwrap_or(json!({"content":r.content}));
                let name = r
                    .fn_name
                    .clone()
                    .or_else(|| self.tool_mut(&r.call_id).map(|t| t.name.clone()))
                    .unwrap_or_else(|| "tool".into());
                let is_error = output.get("error").is_some();
                self.event(Out::ToolDone {
                    id: r.call_id.clone(),
                    name,
                    output,
                    is_error,
                });
            }
        }
        self.settle();
        self.follow();
    }
}
pub fn clean(text: &str) -> String {
    // Strip terminal controls, including ANSI CSI/OSC sequences, from untrusted output.
    let mut result = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                result.push('\n');
            }
            '\t' => result.push_str("    "),
            '\n' => result.push(c),
            _ if !c.is_control() => result.push(c),
            _ => {}
        }
    }
    result
}
pub fn bounded(text: &str) -> String {
    let mut out = clean(text);
    if out.chars().count() > MAX_TEXT {
        out = out.chars().take(MAX_TEXT).collect();
        out.push_str("\n[Display shortened in TUI.]");
    }
    out
}
fn append(body: &mut String, text: &str) {
    if body.chars().count() <= MAX_TEXT {
        *body = bounded(&format!("{body}{text}"));
    }
}
fn append_progress(body: &mut String, text: &str) {
    body.push_str(&clean(text));
    let count = body.chars().count();
    if count > MAX_TEXT {
        *body = body.chars().skip(count - MAX_TEXT).collect();
    }
}
pub fn pretty(v: &Value) -> String {
    bounded(&serde_json::to_string_pretty(v).unwrap_or_default())
}
fn tool_output(name: &str, v: &Value) -> String {
    if v.get("error").is_some() {
        return pretty(v);
    }
    bounded(&match name {
        "shell" => format!(
            "exit {} · {}{}\n{}{}",
            v["exit_code"],
            v["termination"].as_str().unwrap_or("unknown"),
            if v["output_complete"] == false {
                " · output incomplete"
            } else {
                ""
            },
            v["stdout"].as_str().unwrap_or(""),
            v["stderr"].as_str().unwrap_or("")
        ),
        "edit" if v.get("diff").is_some() => format!(
            "{}\n{}",
            v["path"].as_str().unwrap_or(""),
            v["diff"].as_str().unwrap_or("")
        ),
        "write" if v.get("bytes_written").is_some() => format!(
            "{} · {} bytes written",
            v["path"].as_str().unwrap_or(""),
            v["bytes_written"]
        ),
        "read" if v.get("content").is_some() => format!(
            "{}\n{}",
            v["path"].as_str().unwrap_or(""),
            v["content"].as_str().unwrap_or("")
        ),
        _ => pretty(v),
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn idle_status_before_final_message_does_not_duplicate_stream() {
        let mut view = View {
            active: true,
            ..Default::default()
        };
        view.user("你好", false);
        view.event(Out::Reasoning {
            text: "thinking".into(),
        });
        view.event(Out::Chunk {
            text: "你好！".into(),
        });
        // Host has completed, but its forwarding channel has not delivered Message yet.
        view.idle();
        view.event(Out::Message {
            text: "你好！有什么我可以帮你的？".into(),
        });
        let assistant = view
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Text {
                    role: Role::Assistant,
                    text,
                } => Some(text),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(assistant, vec!["你好！有什么我可以帮你的？"]);
        // A later turn is allowed to return identical text; it must not be deduplicated.
        view.user("再说一遍", false);
        view.event(Out::Chunk {
            text: "你好！有什么我可以帮你的？".into(),
        });
        view.event(Out::Message {
            text: "你好！有什么我可以帮你的？".into(),
        });
        assert_eq!(
            view.items
                .iter()
                .filter(|i| matches!(
                    i,
                    Item::Text {
                        role: Role::Assistant,
                        ..
                    }
                ))
                .count(),
            2
        );
    }
    #[test]
    fn folds_survive_stream_updates_and_do_not_transfer_after_eviction() {
        let mut view = View::default();
        view.event(Out::Reasoning { text: "a".into() });
        let id = view.item_id(0);
        view.toggle(id);
        view.event(Out::Reasoning { text: "b".into() });
        assert!(view.expanded.contains(&id));
        for _ in 0..MAX_ITEMS {
            view.user("new", false);
        }
        assert!(!view.expanded.contains(&id));
        assert!(view.selected.is_none());
        view.clear_timeline();
        view.event(Out::Reasoning {
            text: "new thinking".into(),
        });
        assert!(!view.expanded.contains(&view.item_id(0)));
    }

    #[test]
    fn history_shows_original_messages_without_recall_or_summaries() {
        let mut user = StoredMessage::new(ChatMessage::user("original request"));
        user.api_content = Some(MessageContent::from_text("injected recall"));
        user.status = MessageStatus::Compacted;
        let mut summary = StoredMessage::new(ChatMessage::assistant("generated summary"));
        summary.summary = true;
        let runtime = StoredMessage::runtime_context("hook context");
        let mut view = View::default();
        view.restore(vec![user, summary, runtime]);
        assert_eq!(view.items.len(), 1);
        assert!(
            matches!(&view.items[0], Item::Text { role: Role::User, text } if text == "original request")
        );
    }

    #[test]
    fn asks_keep_ids_and_support_text_and_structured_replies() {
        let mut view = View::default();
        view.event(Out::Ask {
            id: "question".into(),
            payload: json!({"question":"Which module?", "call_id":"a"}),
        });
        view.event(Out::Ask {
            id: "mcp".into(),
            payload: json!({"mode":"form", "call_id":"b"}),
        });
        view.asks[0].editor.insert("parser");
        assert_eq!(view.asks[0].answer().unwrap(), json!("parser"));
        view.asks[1].editor.insert("invalid JSON");
        assert!(view.asks[1].answer().is_err());
        view.asks[1].editor.set("{\"action\":\"decline\"}".into());
        assert_eq!(view.asks[1].answer().unwrap(), json!({"action":"decline"}));
        view.event(Out::ToolDone {
            id: "a".into(),
            name: "test".into(),
            output: json!({}),
            is_error: false,
        });
        assert_eq!(view.asks.len(), 1);
        assert_eq!(view.asks[0].id, "mcp");
        view.settle();
        assert!(view.asks.is_empty());
    }

    #[test]
    fn streams_merge_and_tools_are_correlated() {
        let mut v = View::default();
        v.event(Out::Chunk { text: "he".into() });
        v.event(Out::Chunk { text: "llo".into() });
        v.event(Out::Message {
            text: "hello".into(),
        });
        assert_eq!(v.items.len(), 1);
        for id in ["a", "b"] {
            v.event(Out::ToolStarted {
                id: id.into(),
                name: "shell".into(),
                input: json!({}),
            });
        }
        v.event(Out::ToolProgress {
            id: "a".into(),
            payload: json!({"stdout":"work"}),
        });
        v.event(Out::ToolDone {
            id: "b".into(),
            name: "shell".into(),
            output: json!({"ok":false,"exit_code":1}),
            is_error: false,
        });
        assert_eq!(v.tool_mut("a").unwrap().progress, "work");
        assert_eq!(v.tool_mut("b").unwrap().status, ToolStatus::Failed);
        v.settle();
        assert_eq!(v.tool_mut("a").unwrap().status, ToolStatus::Interrupted);
    }
    #[test]
    fn permissions_require_explicit_answer_and_drafts_are_separate() {
        let mut v = View::default();
        v.editor.insert("draft");
        v.event(Out::Ask {
            id: "a".into(),
            payload: json!({"kind":"permission"}),
        });
        assert!(v.asks[0].answer().is_err());
        v.asks[0].editor.insert("n");
        assert_eq!(v.asks[0].answer().unwrap(), json!({"behavior":"deny"}));
        assert_eq!(v.editor.text, "draft");
    }
    #[test]
    fn bounded_output_and_ansi() {
        assert_eq!(clean("\x1b[31mred\x1b[0m\x1b]0;bad\x07"), "red");
        let text = bounded(&"中".repeat(40000));
        assert!(text.contains("Display shortened"));
        assert!(text.chars().count() < 33000);
        let mut progress = "old".repeat(20_000);
        append_progress(&mut progress, "latest progress");
        assert!(progress.ends_with("latest progress"));
        assert_eq!(progress.chars().count(), MAX_TEXT);
    }
}

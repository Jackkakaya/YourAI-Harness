use super::editor::Editor;
use ratatui::text::{Line, Span};
use serde_json::{json, Value};
use std::{
    collections::{HashSet, VecDeque},
    time::{Duration, Instant},
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
    /// Result body with all headers stripped (stdout for shell, diff for
    /// edit, file body for read, first-meaningful-field text otherwise).
    pub output: String,
    /// Shell stderr, kept separate from `output` so previews never mix them.
    pub stderr: String,
    /// Pre-built list preview rows (websearch: "title — host"); may be empty.
    pub brief: Vec<String>,
    /// Syntax-highlighted file/page content (read/write/webfetch); file gutters
    /// are included where appropriate. Colors remain baseline palette slots.
    pub content_hl: Option<Vec<Line<'static>>>,
    /// Structured output format for content-bearing tools (notably webfetch).
    pub content_format: Option<String>,
    pub progress: String,
    pub status: ToolStatus,
    pub started: Option<Instant>,
    pub seconds: Option<u64>,
    pub exit_code: Option<i64>,
    pub adds: Option<usize>,
    pub dels: Option<usize>,
    /// write: whether the file was created (vs overwritten).
    pub created: bool,
    /// edit: parsed + per-hunk syntax-highlighted diff rows, built once when
    /// the result lands so rendering stays width-agnostic (unified or split).
    pub diff_rows: Option<Vec<DiffRow>>,
}

/// One display row of a parsed unified diff. Syntax spans hold baseline
/// palette slot colors; subtle desaturation happens at render time.
#[derive(Clone, Debug)]
pub struct DiffRow {
    /// "@@ -40,6 +40,7 @@" style header label.
    pub hunk: Option<String>,
    /// Unchanged line (identical on both sides).
    pub ctx: Option<Vec<Span<'static>>>,
    /// Removed line + its source line number.
    pub old: Option<(usize, Vec<Span<'static>>)>,
    /// Added line + its source line number.
    pub new: Option<(usize, Vec<Span<'static>>)>,
}
impl DiffRow {
    fn hunk(label: &str) -> Self {
        Self {
            hunk: Some(label.to_owned()),
            ctx: None,
            old: None,
            new: None,
        }
    }
    fn ctx(spans: Vec<Span<'static>>) -> Self {
        Self {
            hunk: None,
            ctx: Some(spans),
            old: None,
            new: None,
        }
    }
    fn change(
        old: Option<(usize, Vec<Span<'static>>)>,
        new: Option<(usize, Vec<Span<'static>>)>,
    ) -> Self {
        Self {
            hunk: None,
            ctx: None,
            old,
            new,
        }
    }
    /// Number of changed lines this row accounts for (for "N more" hints).
    pub fn changes(&self) -> usize {
        usize::from(self.old.is_some()) + usize::from(self.new.is_some())
    }
}
pub enum Item {
    Text {
        role: Role,
        text: String,
    },
    /// Boxed: ToolView (highlighted content, diff rows) dwarfs the others.
    Tool(Box<ToolView>),
    Notice {
        level: Level,
        text: String,
    },
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

pub struct RetryState {
    pub attempt: u32,
    pub max: u32,
    pub reason: String,
    pub until: Instant,
}

/// `/sessions` overlay state. Rows are loaded once on open; the filter query
/// and selected index mutate freely while the overlay is open.
pub struct SessionPickerState {
    pub rows: Vec<crate::sessions::SessionRow>,
    pub query: String,
    pub selected: usize,
}

pub struct View {
    pub model_metrics: yourai_harness::model::BudgetSnapshot,
    pub retry: Option<RetryState>,
    pub recorded_responses: u64,
    pub commands: super::commands::Menu,
    pub toast: Option<(String, Instant)>,
    pub theme: super::theme::Theme,
    pub context_usage: Option<yourai_harness::runtime::ContextUsage>,
    // Timeline state: mutate only through methods so item identities, folds and
    // the render revision (touch) cannot drift apart.
    items: VecDeque<Item>,
    pub editor: Editor,
    asks: VecDeque<Ask>,
    assistant: Option<usize>,
    thinking: Option<usize>,
    pub scroll: usize,
    first_item_id: u64,
    expanded: HashSet<u64>,
    selected: Option<u64>,
    /// Session title shown in the conversation header; derived from the first prompt.
    pub title: Option<String>,
    pub todos: Vec<Todo>,
    /// Right sidebar (Todo list + telemetry). Default on; toggled by ^T.
    pub todo_panel: bool,
    pub todo_scroll: usize,
    pub stats: bool,
    pub help: bool,
    /// `/models` picker: Some(index) when open.
    pub model_picker: Option<usize>,
    /// Candidate labels for the model picker.
    pub model_choices: Vec<String>,
    /// Current model display label (updated by /models switch).
    pub model_label: String,
    /// `/sessions` picker state: Some when the overlay is open.
    pub session_picker: Option<SessionPickerState>,
    /// `/theme` picker: Some(selected index) when open.
    pub theme_picker: Option<usize>,
    /// Optional per-model pricing for cost display (input $/M, output $/M).
    pub pricing: Option<(f64, f64)>,
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
            retry: None,
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
            title: None,
            todos: vec![],
            todo_panel: true,
            todo_scroll: 0,
            stats: false,
            help: false,
            model_picker: None,
            model_choices: vec![],
            model_label: String::new(),
            session_picker: None,
            theme_picker: None,
            pricing: None,
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

    /// Whether the item at `index` is the currently streaming thinking block.
    pub fn is_thinking_at(&self, index: usize) -> bool {
        self.thinking == Some(index)
    }

    /// True if the model is currently producing reasoning (thinking phase).
    pub fn is_thinking(&self) -> bool {
        self.thinking.is_some() && self.assistant.is_none()
    }

    /// True if the model is currently streaming an assistant response.
    pub fn is_responding(&self) -> bool {
        self.assistant.is_some()
    }

    pub fn item_id(&self, index: usize) -> u64 {
        self.first_item_id + index as u64
    }
    /// Read-only timeline access; the render cache assumes mutation goes
    /// through the methods below (they maintain ids, folds and `revision`).
    pub fn items(&self) -> &VecDeque<Item> {
        &self.items
    }
    pub fn expanded(&self, id: u64) -> bool {
        self.expanded.contains(&id)
    }
    pub fn selected(&self) -> Option<u64> {
        self.selected
    }
    /// Asks are answered strictly in arrival order; only the front one is editable.
    pub fn asks_empty(&self) -> bool {
        self.asks.is_empty()
    }
    pub fn ask(&self) -> Option<&Ask> {
        self.asks.front()
    }
    pub fn ask_mut(&mut self) -> Option<&mut Ask> {
        self.asks.front_mut()
    }
    pub fn dismiss_ask(&mut self) {
        self.asks.pop_front();
    }
    pub fn dismiss_asks(&mut self) {
        self.asks.clear();
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
    /// Todos render outside the cached timeline (sidebar repaints every frame),
    /// so changes must not bump `revision`; `touch` would also inflate `unseen`.
    pub fn set_todos(&mut self, todos: Vec<Todo>) {
        if self.todos != todos {
            self.todos = todos;
            self.todo_scroll = self.todo_scroll.min(self.todos.len().saturating_sub(1));
        }
    }
    /// Derive a display title from a prompt; the first non-empty derivation wins.
    pub fn note_title(&mut self, text: &str) -> Option<String> {
        if self.title.is_some() {
            return None;
        }
        let derived = derive_title(text)?;
        self.title = Some(derived.clone());
        Some(derived)
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
        self.retry = None;
        self.asks.clear();
    }
    pub fn settle(&mut self) {
        self.retry = None;
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
            Out::Chunk { text } => {
                self.retry = None;
                self.delta(Role::Assistant, &text);
            }
            Out::Reasoning { text } => {
                self.retry = None;
                self.delta(Role::Thinking, &text);
            }
            Out::Message { text } => {
                self.retry = None;
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
            Out::Retry {
                attempt,
                max,
                reason,
                wait_ms,
            } => {
                self.retry = Some(RetryState {
                    attempt,
                    max,
                    reason: bounded(&reason),
                    until: Instant::now() + Duration::from_millis(wait_ms),
                });
                self.touch();
            }
            Out::ToolStarted { id, name, input } => {
                self.retry = None;
                self.assistant = None;
                self.thinking = None;
                // write: highlight the incoming content right away so the card
                // previews the change while it runs.
                let content_hl = (name == "write").then(|| {
                    let path = input["path"].as_str().unwrap_or("");
                    let content = input["content"].as_str().unwrap_or("");
                    hl_lines(content, path, "+ ")
                });
                let adds = (name == "write")
                    .then(|| input["content"].as_str().unwrap_or("").lines().count());
                let created = name == "write";
                self.push(Item::Tool(Box::new(ToolView {
                    id,
                    name,
                    summary: bounded(
                        input
                            .get("command")
                            .or_else(|| input.get("path"))
                            .or_else(|| input.get("query"))
                            .or_else(|| input.get("url"))
                            .or_else(|| input.get("subject"))
                            .or_else(|| input.get("action"))
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                    ),
                    input: pretty(&input),
                    output: String::new(),
                    stderr: String::new(),
                    brief: Vec::new(),
                    content_hl,
                    content_format: None,
                    progress: String::new(),
                    status: ToolStatus::Running,
                    started: Some(Instant::now()),
                    seconds: None,
                    exit_code: None,
                    adds,
                    dels: None,
                    created,
                    diff_rows: None,
                })));
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
                        append_progress(
                            &mut t.progress,
                            &format!("{}\n", summarize_output(&payload)),
                        );
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
                // Successful results are shaped per tool so the UI never
                // renders machine blobs; failures are flattened into readable
                // diagnostic lines instead of pretty-printed JSON.
                let mut body = String::new();
                let mut stderr = String::new();
                let mut brief = Vec::new();
                let mut content_hl = None;
                let mut content_format = None;
                let mut exit_code = None;
                let mut adds = None;
                let mut dels = None;
                let mut diff_rows = None;
                let mut created = false;
                if let Some(error) = output.get("error") {
                    body = format_error(error, &output);
                } else {
                    match name.as_str() {
                        "shell" => {
                            exit_code = output["exit_code"].as_i64();
                            let mut out = clean(output["stdout"].as_str().unwrap_or(""));
                            stderr = clean(output["stderr"].as_str().unwrap_or(""));
                            if output["output_complete"] == false {
                                if !out.is_empty() {
                                    out.push('\n');
                                }
                                out.push_str("[output incomplete]");
                            }
                            body = bounded(&out);
                        }
                        "read" if output.get("content").is_some() => {
                            let path = output["path"].as_str().unwrap_or("");
                            let content = output["content"].as_str().unwrap_or("");
                            content_hl = Some(hl_read(content, path));
                            body = bounded(content);
                        }
                        "edit" if output.get("diff").is_some() => {
                            let diff = bounded(output["diff"].as_str().unwrap_or(""));
                            let (a, d) = diff_stats(&diff);
                            adds = Some(a);
                            dels = Some(d);
                            diff_rows = Some(build_diff_rows(
                                &diff,
                                output["path"].as_str().unwrap_or(""),
                            ));
                            body = diff;
                        }
                        "write" => {
                            created = output["created"] == true;
                            // Content was highlighted at ToolStarted.
                        }
                        "websearch" => {
                            let content = output["content"].as_str().unwrap_or("");
                            brief = search_brief(content);
                            body = bounded(content);
                        }
                        "webfetch" => {
                            let content = output["content"].as_str().unwrap_or("");
                            let format = output["format"].as_str().unwrap_or("text");
                            content_format = Some(format.to_owned());
                            let hint = match format {
                                "html" => Some("html"),
                                "json" => Some("json"),
                                "xml" => Some("xml"),
                                _ => None,
                            };
                            if let Some(hint) = hint {
                                content_hl = super::syntax::highlight(capped(content), hint);
                            }
                            brief = page_brief(content, format);
                            body = bounded(content);
                        }
                        _ => body = summarize_output(&output),
                    }
                }
                let status = if failed {
                    ToolStatus::Failed
                } else {
                    ToolStatus::Done
                };
                if let Some(t) = self.tool_mut(&id) {
                    t.output = body;
                    t.stderr = stderr;
                    t.brief = brief;
                    t.exit_code = exit_code;
                    if adds.is_some() {
                        t.adds = adds;
                    }
                    if dels.is_some() {
                        t.dels = dels;
                    }
                    if content_hl.is_some() {
                        t.content_hl = content_hl;
                    }
                    if content_format.is_some() {
                        t.content_format = content_format;
                    }
                    if diff_rows.is_some() {
                        t.diff_rows = diff_rows;
                    }
                    if t.name == "write" {
                        t.created = created;
                    }
                    t.progress.clear();
                    t.status = status;
                    t.seconds = t.started.map(|s| s.elapsed().as_secs());
                } else {
                    self.push(Item::Tool(Box::new(ToolView {
                        id: id.clone(),
                        name: name.clone(),
                        input: String::new(),
                        summary: bounded(output["path"].as_str().unwrap_or(&name)),
                        output: body,
                        stderr,
                        brief,
                        content_hl,
                        content_format,
                        progress: String::new(),
                        status,
                        started: None,
                        seconds: None,
                        exit_code,
                        adds,
                        dels,
                        created,
                        diff_rows,
                    })));
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
            Item::Tool(t) if t.id == id => Some(t.as_mut()),
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
/// Title from the first line of a prompt: whitespace-collapsed, truncated.
pub fn derive_title(text: &str) -> Option<String> {
    let mut title = text
        .lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if title.is_empty() {
        return None;
    }
    if title.chars().count() > 48 {
        title = title.chars().take(47).collect();
        title.push('…');
    }
    Some(title)
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
/// Count +/- lines in a unified diff; ---/+++ file headers excluded.
fn diff_stats(diff: &str) -> (usize, usize) {
    let (mut adds, mut dels) = (0, 0);
    for line in diff.lines() {
        if line.starts_with('+') && !line.starts_with("+++") {
            adds += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            dels += 1;
        }
    }
    (adds, dels)
}

/// Parse a unified diff into width-agnostic display rows. Each hunk's old
/// side (context+deletions) and new side (context+additions) are highlighted
/// as continuous text, so multiline scopes within a hunk resolve correctly.
/// `"-a,b"`/`"+c,d"` hunk offsets drive per-cell line numbers.
fn build_diff_rows(diff: &str, path: &str) -> Vec<DiffRow> {
    let mut rows = Vec::new();
    let mut lines = diff.lines().peekable();
    // Skip the ---/+++ file header block.
    while lines.peek().is_some_and(|l| !l.starts_with("@@")) {
        lines.next();
    }
    while let Some(header) = lines.next() {
        let Some((mut old_no, mut new_no)) = hunk_offsets(header) else {
            continue; // stray line before/between hunks (\ No newline…, etc.)
        };
        rows.push(DiffRow::hunk(header));
        let mut body: Vec<&str> = Vec::new();
        while let Some(l) = lines.peek() {
            if l.starts_with("@@") {
                break;
            }
            body.push(lines.next().unwrap());
        }
        let plain = |s: &str| vec![Span::styled(s.to_owned(), ratatui::style::Style::default())];
        let side = |keep: fn(char) -> bool| -> Vec<Vec<Span<'static>>> {
            let text = body
                .iter()
                .filter(|l| keep(l.chars().next().unwrap_or(' ')))
                .map(|l| l.get(1..).unwrap_or(""))
                .collect::<Vec<_>>()
                .join("\n");
            match super::syntax::highlight(&text, path) {
                Some(hl) => hl.into_iter().map(|l| l.spans).collect(),
                None => text.lines().map(plain).collect(),
            }
        };
        let old_hl = side(|c| c != '+');
        let new_hl = side(|c| c != '-');
        let (mut oi, mut ni) = (0usize, 0usize);
        let mut pend_old: Vec<(usize, Vec<Span<'static>>)> = Vec::new();
        let mut pend_new: Vec<(usize, Vec<Span<'static>>)> = Vec::new();
        let flush = |rows: &mut Vec<DiffRow>, pend_old: &mut Vec<_>, pend_new: &mut Vec<_>| {
            let n = pend_old.len().max(pend_new.len());
            for i in 0..n {
                rows.push(DiffRow::change(
                    pend_old.get(i).cloned(),
                    pend_new.get(i).cloned(),
                ));
            }
            pend_old.clear();
            pend_new.clear();
        };
        for line in body {
            let (kind, _) = line.split_at(1.min(line.len()));
            match kind {
                " " => {
                    flush(&mut rows, &mut pend_old, &mut pend_new);
                    let spans = old_hl.get(oi).cloned().unwrap_or_default();
                    rows.push(DiffRow::ctx(spans));
                    oi += 1;
                    ni += 1;
                    old_no += 1;
                    new_no += 1;
                }
                "-" => {
                    pend_old.push((old_no, old_hl.get(oi).cloned().unwrap_or_default()));
                    old_no += 1;
                    oi += 1;
                }
                "+" => {
                    pend_new.push((new_no, new_hl.get(ni).cloned().unwrap_or_default()));
                    new_no += 1;
                    ni += 1;
                }
                _ => {} // "\ No newline at end of file" and friends
            }
        }
        flush(&mut rows, &mut pend_old, &mut pend_new);
    }
    rows
}
/// "@@ -40,6 +41,7 @@" → (40, 41).
fn hunk_offsets(header: &str) -> Option<(usize, usize)> {
    let inner = header.strip_prefix("@@")?.split("@@").next()?;
    let mut old = None;
    let mut new = None;
    for part in inner.split_whitespace() {
        if let Some(rest) = part.strip_prefix('-') {
            old = rest.split(',').next()?.parse().ok();
        } else if let Some(rest) = part.strip_prefix('+') {
            new = rest.split(',').next()?.parse().ok();
        }
    }
    Some((old?, new?))
}
/// read output lines look like "12|code"; split the numeric gutter.
fn split_gutter(line: &str) -> (&str, &str) {
    let Some((num, rest)) = line.split_once('|') else {
        return ("", line);
    };
    if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
        return ("", line);
    }
    (num, rest)
}
/// Keep highlighting work bounded: write/read inputs are not size-capped
/// upstream the way card bodies are (MAX_TEXT).
fn capped(code: &str) -> &str {
    let mut end = code.len().min(MAX_TEXT);
    while !code.is_char_boundary(end) {
        end -= 1;
    }
    &code[..end]
}
/// Highlight read output, preserving source line numbers as a gutter span.
fn hl_read(content: &str, path: &str) -> Vec<Line<'static>> {
    use super::theme::FAINT;
    use ratatui::{style::Style, text::Span};
    let content = capped(content);
    let mut nums = Vec::new();
    let mut src = String::new();
    for line in content.lines() {
        let (num, code) = split_gutter(line);
        nums.push(num);
        src.push_str(code);
        src.push('\n');
    }
    let plain: Vec<Line<'static>> = src.lines().map(|l| Line::from(l.to_owned())).collect();
    let highlighted = super::syntax::highlight(&src, path).unwrap_or(plain);
    highlighted
        .into_iter()
        .zip(nums)
        .map(|(mut line, num)| {
            let mut spans = vec![Span::styled(
                format!("{num:>4} "),
                Style::default().fg(FAINT),
            )];
            spans.append(&mut line.spans);
            Line::from(spans)
        })
        .collect()
}
/// Highlight file content with a fixed gutter (write renders "+ " ghost-diff).
fn hl_lines(code: &str, hint: &str, gutter: &'static str) -> Vec<Line<'static>> {
    use super::theme::GREEN;
    use ratatui::{style::Style, text::Span};
    let code = capped(code);
    let plain: Vec<Line<'static>> = code.lines().map(|l| Line::from(l.to_owned())).collect();
    let highlighted = super::syntax::highlight(code, hint).unwrap_or(plain);
    highlighted
        .into_iter()
        .map(|mut line| {
            let mut spans = vec![Span::styled(gutter.to_owned(), Style::default().fg(GREEN))];
            spans.append(&mut line.spans);
            Line::from(spans)
        })
        .collect()
}
/// websearch content → "title — host" rows. Exa has returned several
/// equivalent shapes over time (Title/URL blocks, markdown links, JSON result
/// arrays and bare URLs), so accept all of them and deduplicate by URL.
fn search_brief(content: &str) -> Vec<String> {
    fn push(rows: &mut Vec<String>, seen: &mut HashSet<String>, title: Option<&str>, url: &str) {
        let url = url
            .trim()
            .trim_matches(|c: char| matches!(c, '<' | '>' | ')' | ']' | ','));
        if rows.len() >= 24 || !seen.insert(url.to_owned()) {
            return;
        }
        let host = url_host(url);
        let title = title.map(str::trim).filter(|s| !s.is_empty());
        let row = match (title, host) {
            (Some(t), Some(h)) => format!("{t} — {h}"),
            (Some(t), None) => t.to_owned(),
            (None, Some(h)) => format!("{h} · {url}"),
            (None, None) => url.to_owned(),
        };
        rows.push(bounded(&row));
    }
    fn json_results(value: &Value, rows: &mut Vec<String>, seen: &mut HashSet<String>) {
        match value {
            Value::Array(values) => {
                for value in values {
                    json_results(value, rows, seen);
                }
            }
            Value::Object(map) => {
                if let Some(url) = map
                    .get("url")
                    .or_else(|| map.get("link"))
                    .and_then(Value::as_str)
                {
                    let title = map
                        .get("title")
                        .or_else(|| map.get("name"))
                        .and_then(Value::as_str);
                    push(rows, seen, title, url);
                }
                for key in ["results", "items", "data", "content"] {
                    if let Some(child) = map.get(key) {
                        json_results(child, rows, seen);
                    }
                }
            }
            Value::String(text) => parse_text(text, rows, seen),
            _ => {}
        }
    }
    fn markdown_link(line: &str) -> Option<(&str, &str)> {
        let open = line.find('[')?;
        let middle = line[open + 1..].find("](")? + open + 1;
        let close = line[middle + 2..].find(')')? + middle + 2;
        Some((&line[open + 1..middle], &line[middle + 2..close]))
    }
    fn bare_url(line: &str) -> Option<&str> {
        let start = line.find("https://").or_else(|| line.find("http://"))?;
        line[start..].split_whitespace().next()
    }
    fn parse_text(text: &str, rows: &mut Vec<String>, seen: &mut HashSet<String>) {
        let mut title: Option<String> = None;
        for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
            let lower = line.to_ascii_lowercase();
            if let Some((_, rest)) = line.split_once(':') {
                if lower.starts_with("title:") || lower.starts_with("name:") {
                    title = Some(rest.trim().to_owned());
                    continue;
                }
                if lower.starts_with("url:") || lower.starts_with("link:") {
                    push(rows, seen, title.take().as_deref(), rest);
                    continue;
                }
            }
            if let Some((label, url)) = markdown_link(line) {
                push(rows, seen, Some(label), url);
                title = None;
            } else if let Some(url) = bare_url(line) {
                push(rows, seen, title.take().as_deref(), url);
            } else if line.starts_with('#') {
                title = Some(line.trim_start_matches('#').trim().to_owned());
            }
        }
    }

    let mut rows = Vec::new();
    let mut seen = HashSet::new();
    if let Ok(value) = serde_json::from_str::<Value>(content) {
        json_results(&value, &mut rows, &mut seen);
    } else {
        parse_text(content, &mut rows, &mut seen);
    }
    rows
}

fn url_host(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()?
        .host_str()
        .filter(|host| !host.is_empty())
        .map(str::to_owned)
}

/// A quiet two-line page teaser for webfetch previews. Markdown headings and
/// common HTML tags are stripped enough to avoid showing markup as the summary.
fn page_brief(content: &str, format: &str) -> Vec<String> {
    content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let cleaned = if format == "markdown" {
                line.trim_start_matches('#').trim()
            } else if format == "html" {
                line.trim_matches(|c| c == '<' || c == '>').trim()
            } else {
                line
            };
            (!cleaned.is_empty()).then(|| bounded(cleaned))
        })
        .take(3)
        .collect()
}

/// Flatten structured tool errors into stable human-readable diagnostics.
fn format_error(error: &Value, envelope: &Value) -> String {
    fn scalar(value: &Value) -> Option<String> {
        match value {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    }
    fn fields(value: &Value, lines: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                for key in ["message", "reason", "detail", "details", "code", "status"] {
                    if let Some(text) = map.get(key).and_then(scalar) {
                        let label = if key == "message" { "Error" } else { key };
                        lines.push(format!("{}: {text}", capitalize(label)));
                    }
                }
                if lines.is_empty() {
                    for (key, value) in map.iter().take(8) {
                        if let Some(text) = scalar(value) {
                            lines.push(format!("{}: {text}", capitalize(key)));
                        }
                    }
                }
            }
            Value::Array(values) => {
                for value in values.iter().take(8) {
                    if let Some(text) = scalar(value) {
                        lines.push(format!("Error: {text}"));
                    } else {
                        fields(value, lines);
                    }
                }
            }
            _ => {
                if let Some(text) = scalar(value) {
                    lines.push(format!("Error: {text}"));
                }
            }
        }
    }
    fn capitalize(text: &str) -> String {
        let mut chars = text.chars();
        match chars.next() {
            Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
            None => String::new(),
        }
    }

    let mut lines = Vec::new();
    fields(error, &mut lines);
    if lines.is_empty() {
        lines.push("Tool failed".into());
    }
    if let Some(tool) = envelope.get("tool").and_then(Value::as_str) {
        lines.push(format!("Tool: {tool}"));
    }
    bounded(&lines.join("\n"))
}

/// Human-readable body for tools with no dedicated shape: first meaningful
/// string field, array items, or a key listing. Never raw pretty-printed JSON.
fn summarize_output(v: &Value) -> String {
    for key in [
        "content", "result", "message", "text", "stdout", "output", "summary", "response",
    ] {
        if let Some(s) = v.get(key).and_then(Value::as_str) {
            let s = s.trim();
            if !s.is_empty() {
                return bounded(s);
            }
        }
    }
    match v {
        Value::Array(items) => {
            let strings: Vec<&str> = items.iter().filter_map(Value::as_str).take(8).collect();
            if strings.is_empty() {
                format!("{} items", items.len())
            } else {
                let suffix = if items.len() > 8 {
                    format!("\n… +{} more", items.len() - 8)
                } else {
                    String::new()
                };
                bounded(&format!("{}{suffix}", strings.join("\n")))
            }
        }
        Value::Object(map) => {
            let keys: Vec<&str> = map.keys().map(String::as_str).take(8).collect();
            let suffix = if map.len() > 8 { ", …" } else { "" };
            format!("{}{}", keys.join(", "), suffix)
        }
        Value::String(s) => bounded(s),
        other => other.to_string(),
    }
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
    fn retry_status_sticks_until_progress_or_settle() {
        let mut v = View::default();
        v.event(Out::Retry {
            attempt: 1,
            max: 2,
            reason: "HTTP 429".into(),
            wait_ms: 5000,
        });
        assert_eq!(v.retry.as_ref().map(|r| r.attempt), Some(1));
        assert!(v.retry.as_ref().unwrap().until > Instant::now());
        v.event(Out::Reasoning { text: "r".into() });
        assert!(v.retry.is_none());
        v.event(Out::Retry {
            attempt: 2,
            max: 2,
            reason: "HTTP 500".into(),
            wait_ms: 1000,
        });
        v.settle();
        assert!(v.retry.is_none());
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

    fn only_tool(v: &View) -> &ToolView {
        let tools: Vec<&ToolView> = v
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Tool(t) => Some(t.as_ref()),
                _ => None,
            })
            .collect();
        let [t] = tools.as_slice() else {
            panic!("expected a single tool item, got {}", tools.len());
        };
        t
    }

    #[test]
    fn shell_result_splits_streams_and_keeps_exit_code() {
        let mut v = View::default();
        v.event(Out::ToolStarted {
            id: "s".into(),
            name: "shell".into(),
            input: json!({"command":"cargo test"}),
        });
        v.event(Out::ToolDone {
            id: "s".into(),
            name: "shell".into(),
            output: json!({"ok":true,"exit_code":0,"termination":"exit","output_complete":true,"stdout":"one\ntwo\n","stderr":"warning: unused\n"}),
            is_error: false,
        });
        let t = only_tool(&v);
        assert_eq!(t.output, "one\ntwo\n");
        assert_eq!(t.stderr, "warning: unused\n");
        assert_eq!(t.exit_code, Some(0));
        // No machine headers leak into the body the card renders.
        assert!(!t.output.contains("exit 0"));
        assert!(!t.output.contains("termination"));
    }

    #[test]
    fn unknown_tool_output_never_renders_raw_json() {
        let mut v = View::default();
        v.event(Out::ToolDone {
            id: "x".into(),
            name: "mcp__jira__create".into(),
            output: json!({"result":"Issue YOUR-47 created","ok":true}),
            is_error: false,
        });
        let t = only_tool(&v);
        assert_eq!(t.output, "Issue YOUR-47 created");
        assert!(!t.output.contains('{'), "no json braces: {}", t.output);
        // Opaque object without a text field degrades to a key listing.
        let mut v = View::default();
        v.event(Out::ToolDone {
            id: "y".into(),
            name: "mcp__x".into(),
            output: json!({"ok":true,"id":7,"url":"https://x"}),
            is_error: false,
        });
        let t = only_tool(&v);
        assert!(!t.output.contains('{'));
        assert!(t.output.contains("ok"));
    }

    #[test]
    fn websearch_builds_title_host_rows() {
        let mut v = View::default();
        v.event(Out::ToolStarted {
            id: "w".into(),
            name: "websearch".into(),
            input: json!({"query":"ratatui diff"}),
        });
        v.event(Out::ToolDone {
            id: "w".into(),
            name: "websearch".into(),
            output: json!({"provider":"exa","query":"ratatui diff","content":"Title: Ratatui widgets\nURL: https://docs.rs/ratatui\n\nTitle: Repo\nURL: https://github.com/ratatui/ratatui\n"}),
            is_error: false,
        });
        let t = only_tool(&v);
        assert_eq!(
            t.brief,
            vec![
                "Ratatui widgets — docs.rs".to_owned(),
                "Repo — github.com".to_owned()
            ]
        );
    }

    #[test]
    fn read_result_highlights_with_line_number_gutter() {
        let mut v = View::default();
        v.event(Out::ToolStarted {
            id: "r".into(),
            name: "read".into(),
            input: json!({"path":"crates/x/src/main.rs","offset":40,"limit":2}),
        });
        v.event(Out::ToolDone {
            id: "r".into(),
            name: "read".into(),
            output: json!({"ok":true,"path":"crates/x/src/main.rs","offset":40,"content":"40|fn main() {\n41|    let s = \"hi\";\n"}),
            is_error: false,
        });
        let t = only_tool(&v);
        let hl = t.content_hl.as_ref().expect("highlighted content");
        assert_eq!(hl.len(), 2);
        // Gutter: right-aligned source line number comes first.
        assert_eq!(hl[0].spans[0].content.as_ref(), "  40 ");
        // Code itself no longer carries the "40|" prefix.
        let code: String = hl[0].spans[1..]
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(code, "fn main() {");
        // And it picked up at least two syntax colors.
        let colors: std::collections::HashSet<_> =
            hl[0].spans[1..].iter().filter_map(|s| s.style.fg).collect();
        assert!(colors.len() >= 2, "syntax colors: {colors:?}");
    }

    #[test]
    fn edit_diff_parses_into_paired_highlighted_rows() {
        let mut v = View::default();
        v.event(Out::ToolStarted {
            id: "e".into(),
            name: "edit".into(),
            input: json!({"path":"src/main.rs","old_text":"    old();","new_text":"    new();\n    more();"}),
        });
        v.event(Out::ToolDone {
            id: "e".into(),
            name: "edit".into(),
            output: json!({"ok":true,"path":"src/main.rs","changed":true,"diff":"--- src/main.rs\n+++ src/main.rs\n@@ -40,3 +40,4 @@\n fn main() {\n-    old();\n+    new();\n+    more();\n }\n"}),
            is_error: false,
        });
        let t = only_tool(&v);
        assert_eq!((t.adds, t.dels), (Some(2), Some(1)));
        let rows = t.diff_rows.as_ref().expect("parsed diff rows");
        // [hunk][ctx fn main][pair 41↔41][gap↔42][ctx }]
        assert_eq!(rows[0].hunk.as_deref(), Some("@@ -40,3 +40,4 @@"));
        assert!(rows[1].ctx.is_some());
        assert_eq!(rows[2].old.as_ref().map(|(n, _)| *n), Some(41));
        assert_eq!(rows[2].new.as_ref().map(|(n, _)| *n), Some(41));
        assert!(rows[3].old.is_none());
        assert_eq!(rows[3].new.as_ref().map(|(n, _)| *n), Some(42));
        // Both sides got syntax colors (more than one distinct fg).
        let new_colors: std::collections::HashSet<_> = rows[2]
            .new
            .as_ref()
            .unwrap()
            .1
            .iter()
            .filter_map(|s| s.style.fg)
            .collect();
        assert!(new_colors.len() >= 2, "highlighted: {new_colors:?}");
        let ctx_text: String = rows[1]
            .ctx
            .as_ref()
            .unwrap()
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(ctx_text, "fn main() {");
    }

    #[test]
    fn write_recognizes_created_and_line_count() {
        let mut v = View::default();
        v.event(Out::ToolStarted {
            id: "w".into(),
            name: "write".into(),
            input: json!({"path":"notes.md","content":"# hi\nbody\n"}),
        });
        v.event(Out::ToolDone {
            id: "w".into(),
            name: "write".into(),
            output: json!({"ok":true,"path":"notes.md","created":true,"bytes_written":9}),
            is_error: false,
        });
        let t = only_tool(&v);
        assert_eq!(t.adds, Some(2));
        assert!(t.created);
        assert!(t.content_hl.is_some());
    }
}

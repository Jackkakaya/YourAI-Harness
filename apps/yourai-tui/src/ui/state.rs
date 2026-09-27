mod tool_output;
use super::editor::Editor;
use crate::text::{append, append_progress, bounded, pretty};
use ratatui::text::{Line, Span};
use serde_json::{json, Value};
use std::{
    collections::{HashSet, VecDeque},
    time::{Duration, Instant},
};
use tool_output::{hl_lines, summarize_output, tool_result};
use yourai_core::prelude::*;
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
#[derive(Clone, Debug)]
pub struct SessionPickerState {
    pub pending_delete: Option<crate::sessions::SessionRow>,
    pub rows: Vec<crate::sessions::SessionRow>,
    pub query: String,
    pub selected: usize,
}

pub struct View {
    pub model_metrics: yourai_harness::model::BudgetSnapshot,
    pub retry: Option<RetryState>,
    commands: super::commands::Menu,
    pub toast: Option<(String, Instant)>,
    pub theme: super::theme::Theme,
    pub context_usage: Option<yourai_harness::runtime::ContextUsage>,
    // Timeline state: mutate only through methods so item identities, folds and
    // the render revision (touch) cannot drift apart.
    items: VecDeque<Item>,
    item_versions: VecDeque<u64>,
    pub editor: Editor,
    asks: VecDeque<Ask>,
    assistant: Option<usize>,
    thinking: Option<usize>,
    pub navigation: super::navigation::Navigation,
    first_item_id: u64,
    expanded: HashSet<u64>,
    selected: Option<u64>,
    /// Session title shown in the conversation header; derived from the first prompt.
    pub title: Option<String>,
    pub todos: Todos,
    pub overlay: super::overlay::Overlay,
    /// Candidate labels for the model picker.
    pub model_choices: Vec<String>,
    /// Current model display label and per-model pricing (input $/M, output
    /// $/M); always set as a pair, never edited field by field.
    pub model: ModelInfo,
    usage: Usage,
    recorded_responses: u64,
    pub revision: u64,
    pub active: bool,
    pub since: Option<Instant>,
}
/// The Todo list plus its panel and scroll position. Scroll is owned here:
/// it saturates on nudge, deliberately outlives a shrinking list, and is
/// clamped against the visible height when the panel is laid out.
pub struct Todos {
    /// Optional Todo panel. Default on; toggled by ^T.
    pub panel: bool,
    scroll: usize,
    items: Vec<Todo>,
}
impl Default for Todos {
    fn default() -> Self {
        Self {
            panel: true,
            scroll: 0,
            items: vec![],
        }
    }
}
impl Todos {
    /// Replace the list; the scroll position deliberately survives a shrink.
    pub fn set(&mut self, items: Vec<Todo>) {
        self.items = items;
    }
    pub fn items(&self) -> &[Todo] {
        &self.items
    }
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
    /// Whether the panel has anything to show.
    pub fn panel_open(&self) -> bool {
        self.panel && !self.items.is_empty()
    }
    pub fn nudge(&mut self, rows: usize, up: bool) {
        self.scroll = if up {
            self.scroll.saturating_sub(rows)
        } else {
            self.scroll.saturating_add(rows)
        };
    }
    #[cfg(test)]
    pub fn scroll(&self) -> usize {
        self.scroll
    }
    /// The effective scroll against the visible height; the stored position
    /// deliberately survives (the test contract pins this).
    pub fn scroll_within(&self, max: usize) -> usize {
        self.scroll.min(max)
    }
}
/// Model label and pricing travel as one pair through switches and restores.
pub struct ModelInfo {
    pub label: String,
    pub pricing: Option<(f64, f64)>,
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
            item_versions: VecDeque::new(),
            editor: Editor::default(),
            asks: VecDeque::new(),
            assistant: None,
            thinking: None,
            navigation: Default::default(),
            first_item_id: 0,
            expanded: HashSet::new(),
            selected: None,
            title: None,
            todos: Todos::default(),
            overlay: Default::default(),
            model_choices: vec![],
            model: ModelInfo {
                label: String::new(),
                pricing: None,
            },
            usage: Usage::default(),
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

    /// Stable identity independent of front-of-history eviction.
    pub fn item_id(&self, index: usize) -> u64 {
        self.first_item_id + index as u64
    }
    /// Read-only timeline access; the render cache assumes mutation goes
    /// through the methods below (they maintain ids, folds and `revision`).
    pub fn items(&self) -> &VecDeque<Item> {
        &self.items
    }
    pub fn item_version(&self, index: usize) -> u64 {
        self.item_versions[index]
    }
    fn item_mut(&mut self, index: usize) -> Option<&mut Item> {
        if let Some(version) = self.item_versions.get_mut(index) {
            *version = version.wrapping_add(1);
        }
        self.items.get_mut(index)
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
    #[cfg(test)]
    pub fn clear_timeline(&mut self) {
        self.first_item_id += self.items.len() as u64;
        self.items.clear();
        self.item_versions.clear();
        self.expanded.clear();
        self.selected = None;
        self.settle();
        self.follow();
    }
    /// Session usage accounting. Three writers, three semantics; the named
    /// methods are the only way in. Restore and the periodic storage poll
    /// are authoritative (overwrite); streaming deltas accumulate on top; a
    /// finished compaction reports replacement totals.
    pub fn restore_usage(&mut self, usage: Usage, responses: u64) {
        self.usage = usage;
        self.recorded_responses = responses;
    }
    pub fn replace_usage(&mut self, usage: Usage) {
        self.usage = usage;
    }
    pub fn accumulate_usage(&mut self, delta: &Usage) {
        self.usage.input_tokens = self.usage.input_tokens.saturating_add(delta.input_tokens);
        self.usage.output_tokens = self.usage.output_tokens.saturating_add(delta.output_tokens);
        self.usage.total_tokens = self.usage.total_tokens.saturating_add(delta.total_tokens);
    }
    pub fn set_response_count(&mut self, count: u64) {
        self.recorded_responses = count;
    }
    pub fn usage(&self) -> &Usage {
        &self.usage
    }
    pub fn recorded_responses(&self) -> u64 {
        self.recorded_responses
    }

    /// The menu is derived from the current draft and focus whenever accessed.
    /// No caller can read stale items or forget a separate synchronization step.
    pub fn menu(&mut self) -> &mut super::commands::Menu {
        self.commands.sync(
            &self.editor.text,
            self.asks_empty() && !self.overlay.is_open(),
        );
        &mut self.commands
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
    }
    fn push(&mut self, item: Item) {
        self.items.push_back(item);
        self.item_versions.push_back(0);
        if self.items.len() > MAX_ITEMS {
            self.items.pop_front();
            self.item_versions.pop_front();
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
            if let Some(Item::Text { text: body, .. }) = self.item_mut(i) {
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
        for (i, item) in self.items.iter_mut().enumerate() {
            if let Item::Tool(t) = item {
                if t.status == ToolStatus::Running {
                    self.item_versions[i] = self.item_versions[i].wrapping_add(1);
                    t.status = ToolStatus::Interrupted;
                    t.seconds = t.started.map(|s| s.elapsed().as_secs());
                }
            }
        }
        self.touch();
    }
    pub fn follow(&mut self) {
        self.navigation.follow();
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
                    if let Some(Item::Text { text: body, .. }) = self.item_mut(i) {
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
                // write previews the incoming content right away so the card
                // shows the change (with + gutters) while it runs.
                let is_write = name == "write";
                let content_hl = is_write.then(|| {
                    hl_lines(
                        input["content"].as_str().unwrap_or(""),
                        input["path"].as_str().unwrap_or(""),
                        "+ ",
                    )
                });
                let adds =
                    is_write.then(|| input["content"].as_str().unwrap_or("").lines().count());
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
                    created: is_write,
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
                let result = tool_result(&name, &output);
                let status = if failed {
                    ToolStatus::Failed
                } else {
                    ToolStatus::Done
                };
                if let Some(t) = self.tool_mut(&id) {
                    t.output = result.body;
                    t.stderr = result.stderr;
                    t.brief = result.brief;
                    t.exit_code = result.exit_code;
                    // Result-side previews win; started-side ones (write
                    // content, highlighted while running) survive a None.
                    t.adds = result.adds.or(t.adds);
                    t.dels = result.dels.or(t.dels);
                    t.content_hl = result.content_hl.or_else(|| t.content_hl.take());
                    t.content_format = result.content_format.or_else(|| t.content_format.take());
                    t.diff_rows = result.diff_rows.or_else(|| t.diff_rows.take());
                    t.created = result.created;
                    t.progress.clear();
                    t.status = status;
                    t.seconds = t.started.map(|s| s.elapsed().as_secs());
                } else {
                    // Orphan result (history replay): synthesize a finished card.
                    self.push(Item::Tool(Box::new(ToolView {
                        id: id.clone(),
                        name: name.clone(),
                        input: String::new(),
                        summary: bounded(output["path"].as_str().unwrap_or(&name)),
                        output: result.body,
                        stderr: result.stderr,
                        brief: result.brief,
                        content_hl: result.content_hl,
                        content_format: result.content_format,
                        progress: String::new(),
                        status,
                        started: None,
                        seconds: None,
                        exit_code: result.exit_code,
                        adds: result.adds,
                        dels: result.dels,
                        created: result.created,
                        diff_rows: result.diff_rows,
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
            Out::Usage { usage } => self.accumulate_usage(&usage),
            Out::Notice { level, message } => self.notice(level, message),
            _ => {}
        }
    }
    fn tool_mut(&mut self, id: &str) -> Option<&mut ToolView> {
        let index = self
            .items
            .iter()
            .rposition(|item| matches!(item, Item::Tool(t) if t.id == id))?;
        match self.item_mut(index)? {
            Item::Tool(t) => Some(t.as_mut()),
            _ => None,
        }
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
#[cfg(test)]
mod tests {
    #[allow(clippy::wildcard_imports)]
    use super::*;
    #[test]
    fn usage_writers_use_three_named_semantics() {
        let mut v = View::default();
        v.restore_usage(
            Usage {
                input_tokens: 100,
                output_tokens: 10,
                total_tokens: 110,
            },
            5,
        );
        v.accumulate_usage(&Usage {
            input_tokens: 1,
            output_tokens: 2,
            total_tokens: 3,
        });
        assert_eq!(v.usage().input_tokens, 101);
        assert_eq!(v.usage().total_tokens, 113);
        assert_eq!(v.recorded_responses(), 5);
        // A finished compaction replaces the meter but keeps the count; the
        // periodic storage poll stays authoritative for the count alone.
        v.replace_usage(Usage::default());
        assert_eq!(v.usage().total_tokens, 0);
        assert_eq!(v.recorded_responses(), 5);
        v.set_response_count(9);
        assert_eq!(v.recorded_responses(), 9);
    }
    #[test]
    fn todo_scroll_saturates_and_survives_a_shrinking_list() {
        let mut todos = Todos::default();
        assert!(!todos.panel_open());
        todos.set(vec![Todo {
            id: "1".into(),
            text: "a".into(),
            completed: false,
        }]);
        assert!(todos.panel_open());
        todos.nudge(3, false);
        assert_eq!(todos.scroll(), 3);
        todos.nudge(10, true);
        assert_eq!(todos.scroll(), 0, "upward nudge saturates at zero");
        todos.nudge(5, false);
        todos.set(vec![]);
        assert_eq!(todos.scroll(), 5, "scroll deliberately outlives the list");
        assert_eq!(todos.scroll_within(2), 2);
        assert_eq!(
            todos.scroll(),
            5,
            "display clamps without erasing the position"
        );
    }
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
    fn unknown_tool_output_preserves_structured_values() {
        let mut v = View::default();
        v.event(Out::ToolDone {
            id: "x".into(),
            name: "mcp__jira__create".into(),
            output: json!({"result":"Issue YOUR-47 created","ok":true}),
            is_error: false,
        });
        let t = only_tool(&v);
        assert!(t.output.contains("Issue YOUR-47 created"));

        // Opaque object without a text field degrades to a key listing.
        let mut v = View::default();
        v.event(Out::ToolDone {
            id: "y".into(),
            name: "mcp__x".into(),
            output: json!({"ok":true,"id":7,"url":"https://x"}),
            is_error: false,
        });
        let t = only_tool(&v);
        assert!(t.output.contains("https://x"));
        assert!(t.output.contains("7"));
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

#[cfg(test)]
mod menu_tests {
    use super::View;
    use crate::ui::overlay::Overlay;
    #[test]
    fn menu_tracks_draft_and_focus_without_a_sync_call() {
        let mut view = View::default();
        view.editor.insert("/");
        view.menu().step(true);
        assert_eq!(view.menu().items()[view.menu().selected].text, "/quit");
        view.editor.take();
        view.editor.insert("/co");
        assert_eq!(view.menu().selected, 0);
        assert_eq!(view.menu().items().len(), 2);
        view.overlay = Overlay::Help { scroll: 0 };
        assert!(view.menu().items().is_empty());
        view.overlay = Overlay::None;
        assert_eq!(view.menu().items().len(), 2);
        view.menu().dismiss();
        assert!(view.menu().items().is_empty());
    }
}

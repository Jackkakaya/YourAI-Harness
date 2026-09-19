use crate::config::Error;
use crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind,
        KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Paragraph},
};
use serde_json::{json, Value};
use std::{
    io::{self, Stdout},
    sync::Arc,
    time::Duration,
};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use unicode_width::UnicodeWidthChar;
use yourai_core::prelude::*;
use yourai_runtime::{Harness, SessionHost};

struct Screen(Terminal<CrosstermBackend<Stdout>>);
impl Screen {
    fn open() -> Result<Self, Error> {
        enable_raw_mode()?;
        let result = (|| {
            execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
            Terminal::new(CrosstermBackend::new(io::stdout()))
        })();
        match result {
            Ok(t) => Ok(Self(t)),
            Err(e) => {
                restore();
                Err(e.into())
            }
        }
    }
}
fn restore() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableBracketedPaste);
}
impl Drop for Screen {
    fn drop(&mut self) {
        restore();
    }
}
#[derive(Default)]
struct View {
    log: Vec<String>,
    input: String,
    ask: Option<(String, Value)>,
    stream: Option<usize>,
    scroll: usize,
    tokens: u64,
}
impl View {
    fn push(&mut self, text: impl Into<String>) {
        self.log.push(clean(&text.into()));
        if self.log.len() > 1000 {
            self.log.remove(0);
            self.stream = self.stream.and_then(|n| n.checked_sub(1));
        }
    }
    fn event(&mut self, event: Out) {
        match event {
            Out::Chunk { text } => {
                if self.stream.is_none() {
                    self.push("AI: ");
                    self.stream = Some(self.log.len() - 1);
                }
                let line = &mut self.log[self.stream.unwrap()];
                line.push_str(&clean(&text));
            }
            Out::Message { text } => {
                if let Some(i) = self.stream.take() {
                    self.log[i] = format!("AI: {}", clean(&text));
                } else {
                    self.push(format!("AI: {text}"));
                }
            }
            Out::Reasoning { text } => self.push(format!("Thinking: {text}")),
            Out::ToolStarted { name, input, .. } => {
                self.stream = None;
                self.push(format!("Tool {name}: {input}"));
            }
            Out::ToolProgress { payload, .. } => self.push(format!("Progress: {payload}")),
            Out::ToolDone {
                name,
                output,
                is_error,
                ..
            } => self.push(format!(
                "Tool {name}{}: {output}",
                if is_error { " failed" } else { " done" }
            )),
            Out::Ask { id, payload } => {
                self.push(format!("QUESTION: {payload}"));
                self.ask = Some((id, payload));
            }
            Out::Usage { usage } => self.tokens = self.tokens.saturating_add(usage.total_tokens),
            Out::Notice { message, .. } => self.push(format!("Notice: {message}")),
            _ => {}
        }
    }
}
fn clean(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .collect()
}
fn reply(payload: &Value, text: &str) -> Result<Value, String> {
    if payload["kind"] == "permission" {
        match text.to_lowercase().as_str() {
            "y" | "yes" => Ok(json!({"behavior":"allow"})),
            "n" | "no" => Ok(json!({"behavior":"deny"})),
            _ => Err("Permission: enter y or n".into()),
        }
    } else {
        serde_json::from_str(text)
            .map_err(|_| "Enter a JSON reply, e.g. \"answer\" or {\"action\":\"decline\"}".into())
    }
}
fn wrapped(log: &[String], width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines = vec![];
    for entry in log {
        let mut text = String::new();
        let mut used = 0;
        for c in entry.chars() {
            let n = c.width().unwrap_or(0);
            if c == '\n' || used + n > width {
                lines.push(Line::raw(std::mem::take(&mut text)));
                used = 0;
            }
            if c != '\n' {
                text.push(c);
                used += n;
            }
        }
        lines.push(Line::raw(text));
    }
    lines
}
fn draw(screen: &mut Screen, v: &View, h: &Harness, model: &str) -> Result<(), Error> {
    screen.0.draw(|frame| {
        let areas = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(frame.area());
        frame.render_widget(
            Paragraph::new(format!(
                "YourAI | {} | {:?} | queued {} | tokens {}\nSession {}",
                model,
                h.host.status(),
                h.host.queued(),
                v.tokens,
                h.host.context().id.0
            )),
            areas[0],
        );
        let lines = wrapped(&v.log, areas[1].width.saturating_sub(2) as usize);
        let height = areas[1].height.saturating_sub(2) as usize;
        let end = lines
            .len()
            .saturating_sub(v.scroll.min(lines.len().saturating_sub(height)));
        let start = end.saturating_sub(height);
        frame.render_widget(
            Paragraph::new(lines[start..end].to_vec())
                .block(Block::default().borders(Borders::ALL).title("Conversation")),
            areas[1],
        );
        let title = if let Some((_, payload)) = &v.ask {
            if payload["kind"] == "permission" {
                "Approval: y / n"
            } else {
                "Reply: JSON"
            }
        } else {
            "Input (Enter sends; running input steers)"
        };
        let width = areas[2].width.saturating_sub(3) as usize;
        let mut tail = vec![];
        let mut used = 0;
        for c in v.input.chars().rev() {
            let n = c.width().unwrap_or(0);
            if used + n > width {
                break;
            }
            tail.push(c);
            used += n;
        }
        let text: String = tail.into_iter().rev().collect();
        frame.render_widget(
            Paragraph::new(text).block(Block::default().borders(Borders::ALL).title(title)),
            areas[2],
        );
        if areas[2].width > 2 && areas[2].height > 2 {
            frame.set_cursor_position((areas[2].x + 1 + used as u16, areas[2].y + 1));
        }
        frame.render_widget(
            Paragraph::new("Esc cancel | Ctrl-Q quit | PgUp/PgDn scroll | /queue /compact /help"),
            areas[3],
        );
    })?;
    Ok(())
}
fn drive(
    host: Arc<SessionHost>,
    tx: mpsc::UnboundedSender<Out>,
    cancel: CancellationToken,
) -> JoinHandle<Result<(), YourAiError>> {
    tokio::spawn(async move { host.serve(TurnLimits::default(), &tx, &cancel).await })
}
pub async fn run(h: &Harness, model: &str) -> Result<(), Error> {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
    let mut screen = Screen::open()?;
    let mut view = View::default();
    view.push(
        "Enter a message. /help for commands. Approvals require y/n; other questions take JSON.",
    );
    let (tx, mut rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let mut driver = Some(drive(h.host.clone(), tx.clone(), cancel.clone()));
    let mut compact: Option<JoinHandle<()>> = None;
    let result=async {
        let mut tick=tokio::time::interval(Duration::from_millis(40));
        loop {
            tick.tick().await;
            for _ in 0..256 {match rx.try_recv(){Ok(e)=>view.event(e),Err(_)=>break}}
            if driver.as_ref().is_some_and(|t|t.is_finished()) {
                match driver.take().unwrap().await {Ok(Err(e))=>view.push(format!("Execution ended: {e}")),Err(e)=>view.push(format!("Driver failed: {e}")),_=>{}}
                view.ask=None;view.stream=None;
            }
            if h.host.status() == SessionStatus::Idle { view.ask=None; }
            if compact.as_ref().is_some_and(|t|t.is_finished()){if let Err(e)=compact.take().unwrap().await{view.push(format!("Compact failed: {e}"));}}
            for _ in 0..32 {
                if !event::poll(Duration::ZERO)?{break;}
                match event::read()? {
                    Event::Paste(text)=>view.input.push_str(&clean(&text).replace('\n'," ")),
                    Event::Key(key) if key.kind!=KeyEventKind::Release=>match key.code {
                        KeyCode::Char('q') if key.modifiers.contains(KeyModifiers::CONTROL)=>return Ok(()),
                        KeyCode::Esc | KeyCode::Char('c') if key.code==KeyCode::Esc || key.modifiers.contains(KeyModifiers::CONTROL)=>{h.host.interrupt();view.ask=None;view.push("Cancellation requested.");},
                        KeyCode::PageUp=>view.scroll=view.scroll.saturating_add(10),
                        KeyCode::PageDown=>view.scroll=view.scroll.saturating_sub(10),
                        KeyCode::Backspace=>{view.input.pop();},
                        KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL|KeyModifiers::ALT)=>view.input.push(c),
                        KeyCode::Enter=>{
                            let text=std::mem::take(&mut view.input);let text=text.trim();if text.is_empty(){continue;}
                            if text=="/quit" {return Ok(());}
                            if text=="/help" {view.push("Enter: send or steer. /queue TEXT: follow-up. /compact: summarize idle session. /quit: close. Esc/Ctrl-C: cancel. PgUp/PgDn: scroll.");continue;}
                            if let Some((id,payload))=&view.ask {
                                match reply(payload,text){Ok(payload)=>{let input=In::Reply{id:id.clone(),payload};match h.host.submit(input){Ok(())=>{view.push(format!("Reply: {text}"));view.ask=None;},Err(e)=>view.push(e.to_string())}},Err(e)=>{view.push(e);view.input=text.into();}}
                                continue;
                            }
                            if text=="/compact" {
                                if compact.is_some(){view.push("Compaction already running.");continue;}
                                let host=h.host.clone();let tx=tx.clone();let token=cancel.clone();
                                compact=Some(tokio::spawn(async move{let result=host.compact(CompactionRequest::new(CompactionTrigger::Manual),&token).await;let message=match result{Ok(r)=>format!("Context {:?}: {} -> {} estimated tokens. {}{}",r.action,r.tokens_before,r.tokens_after,r.reason,r.stop_reason.map(|s|format!("; {s}")).unwrap_or_default()),Err(e)=>format!("Compact failed: {e}")};let _=tx.send(Out::Notice{level:Level::Info,message});}));continue;
                            }
                            if text.starts_with('/') && !text.starts_with("/queue "){view.push("Unknown command. /help");continue;}
                            let input=if let Some(text)=text.strip_prefix("/queue "){In::follow_up(text)}else{In::user_text(text)};
                            match h.host.submit(input){Ok(())=>{view.push(format!("You: {text}"));view.scroll=0;if driver.is_none(){driver=Some(drive(h.host.clone(),tx.clone(),cancel.clone()));}},Err(e)=>view.push(e.to_string())}
                        },
                        _=>{}
                    },
                    _=>{}
                }
            }
            draw(&mut screen,&view,h,model)?;
        }
    }.await;
    cancel.cancel();
    h.host.interrupt();
    if let Some(task) = compact {
        let _ = task.await;
    }
    if let Some(task) = driver {
        let _ = task.await;
    }
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn approval_requires_explicit_answer_and_final_message_replaces_chunks() {
        assert!(reply(&json!({"kind":"permission"}), "anything").is_err());
        assert_eq!(
            reply(&json!({"kind":"permission"}), "n").unwrap()["behavior"],
            "deny"
        );
        let mut v = View::default();
        v.event(Out::Chunk { text: "hel".into() });
        v.event(Out::Chunk { text: "lo".into() });
        v.event(Out::Message {
            text: "hello".into(),
        });
        assert_eq!(v.log, vec!["AI: hello"]);
    }
}

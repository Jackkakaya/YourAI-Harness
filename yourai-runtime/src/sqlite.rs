//! SQLite adapter. Only records live here; no model, tools, or execution scheduler.
use crate::error;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use yourai_core::prelude::*;

pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
fn sql(e: impl std::fmt::Display) -> YourAiError {
    error("sqlite", e)
}
#[derive(Clone)]
pub struct SqliteStore {
    connection: Arc<Mutex<Connection>>,
    gate: Arc<tokio::sync::Mutex<()>>,
}
impl SqliteStore {
    pub fn open(path: &Path) -> Result<Self, YourAiError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(sql)?;
        }
        let c = Connection::open(path).map_err(sql)?;
        c.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(sql)?;
        c.execute_batch(
            "PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;",
        )
        .map_err(sql)?;
        let version: i64 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(sql)?;
        if version > 2 {
            return Err(sql("database schema is newer than this application"));
        }
        if version < 1 {
            c.execute_batch(SCHEMA).map_err(sql)?;
        }
        if version < 2 {
            c.execute_batch("BEGIN IMMEDIATE; ALTER TABLE messages ADD COLUMN tool_output_pruned_at INTEGER; PRAGMA user_version=2; COMMIT;").map_err(sql)?;
        }
        Ok(Self {
            connection: Arc::new(Mutex::new(c)),
            gate: Arc::new(tokio::sync::Mutex::new(())),
        })
    }
    pub(crate) fn with<T>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T, YourAiError>,
    ) -> Result<T, YourAiError> {
        f(&mut *self
            .connection
            .lock()
            .map_err(|_| sql("connection poisoned"))?)
    }
    pub(crate) async fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T, YourAiError> + Send + 'static,
    ) -> Result<T, YourAiError> {
        let guard = self.gate.clone().lock_owned().await;
        let db = self.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            db.with(f)
        })
        .await
        .map_err(sql)?
    }
    pub fn path(root: &Path) -> PathBuf {
        root.join("sessions.sqlite3")
    }
}
const SCHEMA: &str = "
BEGIN IMMEDIATE;
CREATE TABLE IF NOT EXISTS sessions (
 session_id TEXT PRIMARY KEY, parent_session_id TEXT, title TEXT, provider TEXT, model TEXT,
 created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS messages (
 message_id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
 seq INTEGER NOT NULL CHECK(seq>0), kind TEXT NOT NULL CHECK(kind IN ('message','summary')),
 role TEXT NOT NULL CHECK(role IN ('user','assistant','tool')), content_json TEXT NOT NULL, format_version INTEGER NOT NULL DEFAULT 1,
 tool_call_id TEXT, token_count INTEGER, status TEXT NOT NULL CHECK(status IN ('active','compacted','archived')),
 created_at INTEGER NOT NULL, UNIQUE(session_id,seq)
);
CREATE UNIQUE INDEX IF NOT EXISTS one_active_summary ON messages(session_id) WHERE kind='summary' AND status='active';
CREATE UNIQUE INDEX IF NOT EXISTS one_tool_result ON messages(session_id,tool_call_id) WHERE tool_call_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS active_messages ON messages(session_id,status,seq);
CREATE TABLE IF NOT EXISTS usage_events (
 usage_id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
 provider TEXT, model TEXT, source TEXT NOT NULL,
 input_tokens INTEGER, output_tokens INTEGER, total_tokens INTEGER,
 cache_read_tokens INTEGER, cache_creation_tokens INTEGER, reasoning_tokens INTEGER,
 usage_json TEXT, created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS session_usage ON usage_events(session_id,created_at,usage_id);
PRAGMA user_version=1;
COMMIT;";
fn message_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<StoredMessage> {
    let raw: String = r.get(2)?;
    let version: i64 = r.get(5)?;
    if version != 1 {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let value: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let runtime_context = value
        .get("yourai_runtime_context")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let message = serde_json::from_value(value).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let status: String = r.get(4)?;
    Ok(StoredMessage {
        id: r.get(0)?,
        seq: r.get(1)?,
        message,
        summary: r.get::<_, String>(3)? == "summary",
        runtime_context,
        tool_output_pruned_at: r.get(6)?,
        model_response: false,
        request_observation: None,
        status: match status.as_str() {
            "active" => MessageStatus::Active,
            "compacted" => MessageStatus::Compacted,
            _ => MessageStatus::Archived,
        },
    })
}
const MESSAGE_COLUMNS: &str =
    "message_id,seq,content_json,kind,status,format_version,tool_output_pruned_at";
fn meta_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SessionMeta> {
    Ok(SessionMeta {
        id: SessionId(r.get(0)?),
        title: r.get(1)?,
        created_at: r.get(2)?,
        updated_at: r.get(3)?,
        model: r.get(4)?,
        parent_session_id: r.get::<_, Option<String>>(5)?.map(SessionId),
        provider: r.get(6)?,
    })
}
fn stored_json(m: &StoredMessage) -> Result<String, YourAiError> {
    if !m.runtime_context {
        return serde_json::to_string(&m.message).map_err(sql);
    }
    let mut value = serde_json::to_value(&m.message).map_err(sql)?;
    if m.runtime_context {
        value["yourai_runtime_context"] = true.into();
    }
    serde_json::to_string(&value).map_err(sql)
}
fn insert(
    c: &Connection,
    session: &SessionId,
    mut m: StoredMessage,
) -> Result<StoredMessage, YourAiError> {
    let raw = stored_json(&m)?;
    let previous: Option<(String, String, String, i64, String, Option<i64>)> = c
        .query_row(
            "SELECT session_id,content_json,kind,seq,status,tool_output_pruned_at FROM messages WHERE message_id=?1",
            [&m.id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )
        .optional()
        .map_err(sql)?;
    let kind = if m.summary { "summary" } else { "message" };
    if let Some((sid, content, previous_kind, seq, status, pruned)) = previous {
        if sid != session.0 || content != raw || kind != previous_kind {
            return Err(sql("message identity conflict"));
        }
        m.seq = seq;
        m.tool_output_pruned_at = pruned;
        m.status = match status.as_str() {
            "active" => MessageStatus::Active,
            "compacted" => MessageStatus::Compacted,
            _ => MessageStatus::Archived,
        };
        return Ok(m);
    }
    m.seq = c
        .query_row(
            "SELECT COALESCE(MAX(seq),0)+1 FROM messages WHERE session_id=?1",
            [&session.0],
            |r| r.get(0),
        )
        .map_err(sql)?;
    let results = m.message.content.tool_responses();
    if results.len() > 1 {
        return Err(sql("one tool response per message is required"));
    }
    let tool_id = results.first().map(|r| r.call_id.as_str());
    let role = format!("{:?}", m.message.role).to_lowercase();
    c.execute("INSERT INTO messages(message_id,session_id,seq,kind,role,content_json,tool_call_id,token_count,status,created_at,format_version) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'active',?9,?10)", params![m.id,session.0,m.seq,kind,role,raw,tool_id,raw.len().div_ceil(4) as i64,now(),1]).map_err(sql)?;
    c.execute(
        "UPDATE sessions SET updated_at=?2 WHERE session_id=?1",
        params![session.0, now()],
    )
    .map_err(sql)?;
    Ok(m)
}
impl SessionManager for SqliteStore {
    fn create_session<'a>(&'a self) -> BoxFuture<'a, Result<SessionMeta, YourAiError>> {
        Box::pin(async {
            let id = SessionId::new();
            let sid = id.clone();
            self.run(move |c| {
                c.execute(
                    "INSERT INTO sessions(session_id,created_at,updated_at) VALUES(?1,?2,?2)",
                    params![sid.0, now()],
                )
                .map_err(sql)?;
                Ok(())
            })
            .await?;
            self.load_session(&id).await
        })
    }
    fn load_session<'a>(
        &'a self,
        id: &'a SessionId,
    ) -> BoxFuture<'a, Result<SessionMeta, YourAiError>> {
        let id = id.clone();
        Box::pin(self.run(move |c| c.query_row("SELECT session_id,title,created_at,updated_at,model,parent_session_id,provider FROM sessions WHERE session_id=?1",[id.0],meta_row).map_err(sql)))
    }
    fn save_session<'a>(&'a self, m: &'a SessionMeta) -> BoxFuture<'a, Result<(), YourAiError>> {
        let m = m.clone();
        Box::pin(self.run(move |c| { let n=c.execute("UPDATE sessions SET title=?2,model=?3,updated_at=?4,parent_session_id=?5,provider=?6 WHERE session_id=?1",params![m.id.0,m.title,m.model,now(),m.parent_session_id.map(|id|id.0),m.provider]).map_err(sql)?; if n==0 {return Err(sql("unknown session"));} Ok(()) }))
    }
    fn list_sessions<'a>(&'a self) -> BoxFuture<'a, Result<Vec<SessionMeta>, YourAiError>> {
        Box::pin(self.run(|c| { let mut q=c.prepare("SELECT session_id,title,created_at,updated_at,model,parent_session_id,provider FROM sessions ORDER BY updated_at DESC,session_id").map_err(sql)?; let rows=q.query_map([],meta_row).map_err(sql)?.collect::<Result<Vec<_>,_>>().map_err(sql)?; Ok(rows) }))
    }
    fn delete_session<'a>(&'a self, id: &'a SessionId) -> BoxFuture<'a, Result<(), YourAiError>> {
        let id = id.clone();
        Box::pin(self.run(move |c| {
            c.execute("DELETE FROM sessions WHERE session_id=?1", [id.0])
                .map_err(sql)?;
            Ok(())
        }))
    }
    fn fork_session<'a>(
        &'a self,
        id: &'a SessionId,
    ) -> BoxFuture<'a, Result<SessionId, YourAiError>> {
        let id = id.clone();
        Box::pin(self.run(move |c| {
            let tx=c.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
            let child=SessionId::new();
            let n=tx.execute("INSERT INTO sessions(session_id,parent_session_id,title,model,provider,created_at,updated_at) SELECT ?2,session_id,title,model,provider,?3,?3 FROM sessions WHERE session_id=?1",params![id.0,child.0,now()]).map_err(sql)?;
            if n==0 { return Err(sql("unknown parent session")); }
            tx.execute("INSERT INTO messages SELECT lower(hex(randomblob(16))),?2,seq,kind,role,content_json,format_version,tool_call_id,token_count,status,created_at,tool_output_pruned_at FROM messages WHERE session_id=?1",params![id.0,child.0]).map_err(sql)?;
            tx.commit().map_err(sql)?; Ok(child)
        }))
    }
    fn read_messages<'a>(
        &'a self,
        id: &'a SessionId,
        query: MessageQuery,
    ) -> BoxFuture<'a, Result<MessagePage, YourAiError>> {
        let id = id.clone();
        Box::pin(self.run(move |c| {
            if query.limit==0 || query.limit>10000 {return Err(sql("page limit must be 1..=10000"));}
            let mut q=c.prepare(&format!("SELECT {MESSAGE_COLUMNS} FROM messages WHERE session_id=?1 AND seq>?2 AND (?3=0 OR status='active') ORDER BY seq LIMIT ?4")).map_err(sql)?;
            let mut messages=q.query_map(params![id.0,query.after,query.active_only,query.limit as i64+1],message_row).map_err(sql)?.collect::<Result<Vec<_>,_>>().map_err(sql)?;
            let next=if messages.len()>query.limit {messages.pop();messages.last().map(|m|m.seq)}else{None};
            Ok(MessagePage{messages,next})
        }))
    }
    fn append_messages<'a>(
        &'a self,
        id: &'a SessionId,
        messages: Vec<StoredMessage>,
    ) -> BoxFuture<'a, Result<Vec<StoredMessage>, YourAiError>> {
        let id = id.clone();
        Box::pin(self.run(move |c| {
            let tx = c
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql)?;
            let mut result = vec![];
            for m in messages {
                if m.summary || m.status != MessageStatus::Active {
                    return Err(sql("append accepts active ordinary messages only"));
                }
                result.push(insert(&tx, &id, m)?);
            }
            tx.commit().map_err(sql)?;
            Ok(result)
        }))
    }
    fn save_context<'a>(
        &'a self,
        id: &'a SessionId,
        change: ContextChange,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        let id = id.clone();
        Box::pin(self.run(move |c| {
            let tx = c.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
            let is_retry = if let Some(summary) = &change.compaction {
                tx.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE message_id=?1 AND session_id=?2)",params![summary.summary.id,id.0],|r|r.get::<_,bool>(0)).map_err(sql)?
            } else { false };
            for source in &change.pruned {
                let n = tx.execute("UPDATE messages SET tool_output_pruned_at=COALESCE(tool_output_pruned_at,?3) WHERE session_id=?1 AND message_id=?2 AND (status='active' OR (?4 AND status='compacted')) AND role='tool'", params![id.0,source,now(),is_retry]).map_err(sql)?;
                if n != 1 { return Err(sql("stale pruning source")); }
            }
            tx.execute("UPDATE sessions SET updated_at=?2 WHERE session_id=?1",params![id.0,now()]).map_err(sql)?;
            let Some(change) = change.compaction else {
                tx.commit().map_err(sql)?;
                return Ok(());
            };
            if !change.summary.summary || change.summary.status != MessageStatus::Active || change.sources.is_empty() { return Err(sql("invalid compaction candidate")); }
            let unique: std::collections::HashSet<_> = change.sources.iter().collect();
            if unique.len() != change.sources.len() { return Err(sql("duplicate compaction source")); }
            // A completed retry is accepted only for the same summary identity/content and compacted sources.
            let existing: Option<(String, String, String)> = tx
                .query_row(
                    "SELECT session_id,content_json,kind FROM messages WHERE message_id=?1",
                    [&change.summary.id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()
                .map_err(sql)?;
            let retry = existing.is_some();
            if let Some((sid, raw, kind)) = existing {
                if sid != id.0
                    || kind != "summary"
                    || raw != stored_json(&change.summary)?
                {
                    return Err(sql("summary identity conflict"));
                }
            }
            for source in &change.sources {
                let status: Option<String> = tx
                    .query_row(
                        "SELECT status FROM messages WHERE session_id=?1 AND message_id=?2",
                        params![id.0, source],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(sql)?;
                let expected = if retry { "compacted" } else { "active" };
                if status.as_deref() != Some(expected) {
                    return Err(sql("stale compaction source"));
                }
            }
            if !retry {
                for source in &change.sources {
                    tx.execute(
                        "UPDATE messages SET status='compacted' WHERE message_id=?1",
                        [source],
                    )
                    .map_err(sql)?;
                }
                insert(&tx, &id, change.summary)?;
            }
            tx.commit().map_err(sql)?;
            Ok(())
        }))
    }
}

/// Explicit one-way import. Original JSON files remain untouched. All direct child
/// sessions are imported in one transaction; live sessions and ID collisions are rejected.
pub fn import_json_sessions(root: &Path) -> Result<usize, YourAiError> {
    use fs2::FileExt;
    use serde::Deserialize;
    #[derive(Deserialize)]
    struct Meta {
        id: String,
        title: String,
        created: i64,
        updated: i64,
        model: Option<String>,
    }
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct History {
        messages: Vec<ChatMessage>,
        summary: Option<String>,
        boundary: usize,
        usage: Vec<Usage>,
    }
    let mut sessions = vec![];
    let mut guards = vec![];
    for entry in std::fs::read_dir(root).map_err(sql)? {
        let dir = entry.map_err(sql)?.path();
        if !dir.is_dir() || !dir.join("meta.json").exists() {
            continue;
        }
        for name in ["host.lock", "history.lock"] {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(dir.join(name))
                .map_err(sql)?;
            file.try_lock_exclusive()
                .map_err(|e| sql(format!("close the session before importing: {e}")))?;
            guards.push(file);
        }
        let meta: Meta = crate::storage::read_json(&dir.join("meta.json"))?;
        if dir.file_name().and_then(|s| s.to_str()) != Some(meta.id.as_str()) {
            return Err(sql("legacy metadata identity does not match its directory"));
        }
        let history = if dir.join("history.json").exists() {
            crate::storage::read_json::<History>(&dir.join("history.json"))?
        } else {
            History::default()
        };
        if history.boundary > history.messages.len()
            || (history.boundary > 0 && history.summary.is_none())
        {
            return Err(sql("invalid legacy compaction boundary"));
        }
        sessions.push((meta, history));
    }
    if sessions.is_empty() {
        return Ok(0);
    }
    let count = sessions.len();
    let store = SqliteStore::open(&SqliteStore::path(root))?;
    store.with(move |c| {
        let tx=c.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql)?;
        for (meta,history) in sessions {
            let id=SessionId(meta.id);
            tx.execute("INSERT INTO sessions(session_id,title,model,created_at,updated_at) VALUES(?1,?2,?3,?4,?5)",params![id.0,meta.title,meta.model,meta.created.checked_mul(1000).ok_or_else(||sql("timestamp overflow"))?,meta.updated.checked_mul(1000).ok_or_else(||sql("timestamp overflow"))?]).map_err(sql)?;
            for (index,message) in history.messages.into_iter().enumerate() {
                // Instructions are reloaded from host configuration, never imported as chat facts.
                if message.role==ChatRole::System {continue;}
                let row=insert(&tx,&id,StoredMessage::new(message))?;
                if index<history.boundary {tx.execute("UPDATE messages SET status='compacted' WHERE message_id=?1",[row.id]).map_err(sql)?;}
            }
            if let Some(summary)=history.summary {
                let mut row=StoredMessage::new(ChatMessage::user(format!("[Conversation summary; historical context]\n{summary}")));row.summary=true;insert(&tx,&id,row)?;
            }
            for (index,u) in history.usage.into_iter().enumerate() {
                let cv=|v:u64|i64::try_from(v).map_err(sql);
                tx.execute("INSERT INTO usage_events(usage_id,session_id,model,source,input_tokens,output_tokens,total_tokens,usage_json,created_at) VALUES(?1,?2,?3,'legacy',?4,?5,?6,?7,?8)",params![format!("legacy:{}:{index}",id.0),id.0,meta.model,cv(u.input_tokens)?,cv(u.output_tokens)?,cv(u.total_tokens)?,serde_json::to_string(&u).map_err(sql)?,meta.updated*1000]).map_err(sql)?;
            }
            // Do not import usage.json as well: it aggregates the same calls.
            tx.execute("UPDATE sessions SET updated_at=?2 WHERE session_id=?1",params![id.0,meta.updated*1000]).map_err(sql)?;
        }
        tx.commit().map_err(sql)?;Ok(())
    })?;
    drop(guards);
    Ok(count)
}

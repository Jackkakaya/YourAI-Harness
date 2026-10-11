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
    owner: Option<(SessionId, String)>,
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
        if version > 6 {
            return Err(sql("database schema is newer than this application"));
        }
        if version < 1 {
            c.execute_batch(SCHEMA).map_err(sql)?;
        }
        if version < 2 {
            c.execute_batch("BEGIN IMMEDIATE; ALTER TABLE messages ADD COLUMN tool_output_pruned_at INTEGER; PRAGMA user_version=2; COMMIT;").map_err(sql)?;
        }
        if version < 3 {
            c.execute_batch("BEGIN IMMEDIATE; ALTER TABLE sessions ADD COLUMN system_prompt TEXT; PRAGMA user_version=3; COMMIT;").map_err(sql)?;
        }
        if version < 4 {
            c.execute_batch("BEGIN IMMEDIATE;
                CREATE TABLE IF NOT EXISTS model_requests (
                    request_id TEXT PRIMARY KEY, run_id TEXT NOT NULL,
                    session_id TEXT, source TEXT NOT NULL, model TEXT NOT NULL,
                    started_at INTEGER NOT NULL, duration_ms INTEGER,
                    turn_id TEXT, attempt INTEGER NOT NULL,
                    outcome TEXT NOT NULL, http_status INTEGER
                );
                CREATE INDEX IF NOT EXISTS model_requests_session ON model_requests(session_id, started_at);
                PRAGMA user_version=4; COMMIT;").map_err(sql)?;
        }
        if version < 5 {
            c.execute_batch(
                "BEGIN IMMEDIATE;
                CREATE TABLE IF NOT EXISTS tasks (
                    session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
                    task_id TEXT NOT NULL CHECK(length(task_id)>0),
                    subject TEXT NOT NULL, description TEXT, owner TEXT,
                    completed INTEGER NOT NULL CHECK(completed IN (0,1)),
                    seq INTEGER NOT NULL CHECK(seq>=0),
                    PRIMARY KEY(session_id,task_id)
                );
                CREATE INDEX IF NOT EXISTS session_tasks ON tasks(session_id,seq,task_id);
                PRAGMA user_version=5; COMMIT;",
            )
            .map_err(sql)?;
        }
        if version < 6 {
            c.execute_batch("BEGIN IMMEDIATE;
                CREATE TABLE IF NOT EXISTS session_state (
                    session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
                    state_key TEXT NOT NULL, revision INTEGER NOT NULL CHECK(revision>0), value_json TEXT NOT NULL,
                    PRIMARY KEY(session_id,state_key)
                );
                CREATE TABLE IF NOT EXISTS session_owners (
                    session_id TEXT PRIMARY KEY REFERENCES sessions(session_id) ON DELETE CASCADE,
                    owner TEXT NOT NULL, expires_at INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS tool_outputs (
                    session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
                    output_id TEXT NOT NULL, offset INTEGER NOT NULL CHECK(offset>=0),
                    content BLOB NOT NULL, created_at INTEGER NOT NULL,
                    PRIMARY KEY(session_id,output_id,offset)
                );
                CREATE INDEX IF NOT EXISTS tool_outputs_expiry ON tool_outputs(created_at);
                PRAGMA user_version=6; COMMIT;").map_err(sql)?;
        }
        Ok(Self {
            owner: None,
            connection: Arc::new(Mutex::new(c)),
            gate: Arc::new(tokio::sync::Mutex::new(())),
        })
    }
    pub(crate) fn with<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, YourAiError>,
    ) -> Result<T, YourAiError> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| sql("connection poisoned"))?;
        // Ownership validation and every read/write occur in the same transaction.
        // No local file lock participates in correctness.
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        if let Some((id, owner)) = &self.owner {
            let valid: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM session_owners WHERE session_id=?1 AND owner=?2 AND expires_at>?3)", params![id.as_str(),owner,now()], |r|r.get(0)).map_err(sql)?;
            if !valid {
                return Err(sql("session ownership expired or replaced"));
            }
        }
        let value = f(&tx)?;
        tx.commit().map_err(sql)?;
        Ok(value)
    }
    pub(crate) async fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, YourAiError> + Send + 'static,
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
    if !matches!(version, 1 | 2) {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let value: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let runtime_context = value
        .get("yourai_runtime_context")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let context_key = value
        .get("yourai_context_key")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        // Existing sessions predate context keys; preserve their instruction rows too.
        .or_else(|| {
            if runtime_context {
                value
                    .get("content")
                    .and_then(|content| {
                        serde_json::from_value::<MessageContent>(content.clone()).ok()
                    })
                    .and_then(|content| {
                        content.first_text().and_then(|text| {
                            text.strip_prefix("[Instructions: ")
                                .and_then(|text| text.split_once("]\n"))
                                .map(|(path, _)| format!("instructions:{path}"))
                        })
                    })
            } else {
                None
            }
        });
    let api_content = value
        .get("yourai_api_content")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e))
        })?;
    let recall = value
        .get("yourai_recall")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e))
        })?
        .unwrap_or_default();
    let input = value
        .get("yourai_input")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e))
        })?;
    let message = serde_json::from_value(value).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let status: String = r.get(4)?;
    Ok(StoredMessage {
        input,
        id: r.get(0)?,
        seq: r.get(1)?,
        message,
        summary: r.get::<_, String>(3)? == "summary",
        runtime_context,
        context_key,
        api_content,
        recall,
        tool_output_pruned_at: r.get(6)?,
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
fn task_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Task> {
    Ok(Task {
        id: r.get(0)?,
        subject: r.get(1)?,
        description: r.get(2)?,
        owner: r.get(3)?,
        completed: r.get(4)?,
        seq: r.get(5)?,
    })
}
fn meta_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SessionMeta> {
    Ok(SessionMeta {
        id: SessionId::from(r.get::<_, String>(0)?),
        title: r.get(1)?,
        created_at: r.get(2)?,
        updated_at: r.get(3)?,
        model: r.get(4)?,
        parent_session_id: r.get::<_, Option<String>>(5)?.map(SessionId::from),
        provider: r.get(6)?,
        system_prompt: r.get(7)?,
    })
}
fn stored_json(m: &StoredMessage) -> Result<String, YourAiError> {
    let mut value = serde_json::to_value(&m.message).map_err(sql)?;
    if let Some(input) = &m.input {
        if m.message.role != ChatRole::User || m.runtime_context || m.summary {
            return Err(sql("only admitted user messages may carry input identity"));
        }
        if !matches!(input, In::UserText { id: Some(id), .. } if id == &m.id) {
            return Err(sql("input identity differs from message identity"));
        }
        value["yourai_input"] = serde_json::to_value(input).map_err(sql)?;
    }
    if m.runtime_context {
        value["yourai_runtime_context"] = true.into();
    }
    if let Some(key) = &m.context_key {
        if !m.runtime_context || m.summary || m.message.role != ChatRole::User {
            return Err(sql("context keys require an ordinary runtime context"));
        }
        value["yourai_context_key"] = key.clone().into();
    }
    if let Some(content) = &m.api_content {
        if m.message.role != ChatRole::User || m.runtime_context || m.summary {
            return Err(sql("only real user messages may carry recall"));
        }
        value["yourai_api_content"] = serde_json::to_value(content).map_err(sql)?;
        value["yourai_recall"] = serde_json::to_value(&m.recall).map_err(sql)?;
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
        if sid != session.as_str() || content != raw || kind != previous_kind {
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
            [session.as_str()],
            |r| r.get(0),
        )
        .map_err(sql)?;
    let results = m.message.content.tool_responses();
    if results.len() > 1 {
        return Err(sql("one tool response per message is required"));
    }
    let tool_id = results.first().map(|r| r.call_id.as_str());
    let role = format!("{:?}", m.message.role).to_lowercase();
    c.execute("INSERT INTO messages(message_id,session_id,seq,kind,role,content_json,tool_call_id,token_count,status,created_at,format_version) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'active',?9,?10)", params![m.id,session.as_str(),m.seq,kind,role,raw,tool_id,raw.len().div_ceil(4) as i64,now(),if m.api_content.is_some() {2} else {1}]).map_err(sql)?;
    c.execute(
        "UPDATE sessions SET updated_at=?2 WHERE session_id=?1",
        params![session.as_str(), now()],
    )
    .map_err(sql)?;
    Ok(m)
}
impl SessionStore for SqliteStore {
    fn claim_session(
        &self,
        id: &SessionId,
        owner: &str,
        ttl_ms: u64,
    ) -> Result<Arc<dyn SessionStore>, YourAiError> {
        let expires = now()
            .checked_add(i64::try_from(ttl_ms).map_err(sql)?)
            .ok_or_else(|| sql("lease expiry overflow"))?;
        if ttl_ms == 0 {
            return Err(sql("lease duration must be positive"));
        }
        let mut db = self.clone();
        db.owner = None;
        db.with(|c| {
            let changed = c.execute("INSERT INTO session_owners(session_id,owner,expires_at) VALUES(?1,?2,?3) ON CONFLICT(session_id) DO UPDATE SET owner=excluded.owner,expires_at=excluded.expires_at WHERE session_owners.expires_at<=?4",params![id.as_str(),owner,expires,now()]).map_err(sql)?;
            if changed != 1 { return Err(sql("session is already owned")); }
            Ok(())
        })?;
        let mut owned = self.clone();
        owned.owner = Some((id.clone(), owner.to_owned()));
        Ok(Arc::new(owned))
    }
    fn renew_session(&self, ttl_ms: u64) -> Result<(), YourAiError> {
        if ttl_ms == 0 {
            return Err(sql("lease duration must be positive"));
        }
        let (id, owner) = self
            .owner
            .as_ref()
            .ok_or_else(|| sql("no session ownership"))?;
        let expires = now()
            .checked_add(i64::try_from(ttl_ms).map_err(sql)?)
            .ok_or_else(|| sql("lease expiry overflow"))?;
        self.with(|c| {
            c.execute(
                "UPDATE session_owners SET expires_at=?3 WHERE session_id=?1 AND owner=?2",
                params![id.as_str(), owner, expires],
            )
            .map_err(sql)?;
            Ok(())
        })
    }
    fn release_session(&self) -> Result<(), YourAiError> {
        let (id, owner) = self
            .owner
            .as_ref()
            .ok_or_else(|| sql("no session ownership"))?;
        let mut db = self.clone();
        db.owner = None;
        db.with(|c| {
            c.execute(
                "DELETE FROM session_owners WHERE session_id=?1 AND owner=?2",
                params![id.as_str(), owner],
            )
            .map_err(sql)?;
            Ok(())
        })
    }
    fn read_state(&self, id: &SessionId, key: &str) -> Result<Option<SessionState>, YourAiError> {
        self.with(|c| {
            let row: Option<(i64,String)> = c.query_row("SELECT revision,value_json FROM session_state WHERE session_id=?1 AND state_key=?2",params![id.as_str(),key],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(sql)?;
            row.map(|(revision,raw)|Ok(SessionState{revision,value:serde_json::from_str(&raw).map_err(sql)?})).transpose()
        })
    }
    fn save_state(
        &self,
        id: &SessionId,
        key: &str,
        expected: Option<i64>,
        value: serde_json::Value,
    ) -> Result<i64, YourAiError> {
        if key.is_empty() || key.len() > 128 {
            return Err(sql("invalid state key"));
        }
        let revision = expected
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| sql("state revision overflow"))?;
        let raw = serde_json::to_string(&value).map_err(sql)?;
        self.with(|c| {
            let n=match expected {
                Some(old)=>c.execute("UPDATE session_state SET revision=?3,value_json=?4 WHERE session_id=?1 AND state_key=?2 AND revision=?5",params![id.as_str(),key,revision,raw,old]),
                None=>c.execute("INSERT INTO session_state(session_id,state_key,revision,value_json) VALUES(?1,?2,?3,?4) ON CONFLICT(session_id,state_key) DO NOTHING",params![id.as_str(),key,revision,raw]),
            }.map_err(sql)?;
            if n!=1 { return Err(sql("session state changed; retry from current revision")); }
            Ok(revision)
        })
    }
    fn append_output<'a>(
        &'a self,
        id: &'a SessionId,
        output: &'a str,
        offset: u64,
        bytes: Vec<u8>,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        let id = id.clone();
        let output = output.to_owned();
        Box::pin(self.run(move |c| {
            if bytes.is_empty() {return Ok(());}
            let offset=i64::try_from(offset).map_err(sql)?;
            let existing: Option<Vec<u8>>=c.query_row("SELECT content FROM tool_outputs WHERE session_id=?1 AND output_id=?2 AND offset=?3",params![id.as_str(),output,offset],|r|r.get(0)).optional().map_err(sql)?;
            if let Some(previous)=existing {return if previous==bytes {Ok(())} else {Err(sql("output chunk identity conflict"))};}
            let end: i64=c.query_row("SELECT COALESCE(MAX(offset+length(content)),0) FROM tool_outputs WHERE session_id=?1 AND output_id=?2",params![id.as_str(),output],|r|r.get(0)).map_err(sql)?;
            if end!=offset {return Err(sql("output chunk offset conflict"));}
            c.execute("INSERT INTO tool_outputs(session_id,output_id,offset,content,created_at) VALUES(?1,?2,?3,?4,?5)",params![id.as_str(),output,offset,bytes,now()]).map_err(sql)?;
            Ok(())
        }))
    }
    fn read_output<'a>(
        &'a self,
        id: &'a SessionId,
        output: &'a str,
        offset: u64,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<u8>, YourAiError>> {
        let id = id.clone();
        let output = output.to_owned();
        Box::pin(self.run(move |c| {
            if limit==0 || limit>1024*1024 {return Err(sql("output read limit must be 1..=1048576 bytes"));}
            let offset=i64::try_from(offset).map_err(sql)?;
            let end=offset.checked_add(limit as i64).ok_or_else(||sql("output range overflow"))?;
            let exists: bool=c.query_row("SELECT EXISTS(SELECT 1 FROM tool_outputs WHERE session_id=?1 AND output_id=?2)",params![id.as_str(),output],|r|r.get(0)).map_err(sql)?;
            if !exists {return Err(sql("unknown or expired tool output"));}
            let mut stmt=c.prepare("SELECT offset,content FROM tool_outputs WHERE session_id=?1 AND output_id=?2 AND offset<?3 AND offset+length(content)>?4 ORDER BY offset").map_err(sql)?;
            let rows=stmt.query_map(params![id.as_str(),output,end,offset],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,Vec<u8>>(1)?))).map_err(sql)?;
            let mut result=Vec::new();
            for row in rows {let (start,chunk)=row.map_err(sql)?;let from=(offset-start).max(0) as usize;let to=((end-start).max(0) as usize).min(chunk.len());result.extend_from_slice(&chunk[from..to]);}
            Ok(result)
        }))
    }
    fn cleanup_outputs<'a>(
        &'a self,
        id: &'a SessionId,
        older_than_ms: i64,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        let id = id.clone();
        Box::pin(self.run(move |c| {
            c.execute("DELETE FROM tool_outputs WHERE session_id=?1 AND output_id IN (SELECT output_id FROM tool_outputs WHERE session_id=?1 GROUP BY output_id HAVING MAX(created_at)<?2)",params![id.as_str(),older_than_ms]).map_err(sql)?;
            Ok(())
        }))
    }

    fn read_tasks(&self, id: &SessionId) -> Result<Vec<Task>, YourAiError> {
        self.with(|c| {
            let mut stmt = c.prepare("SELECT task_id,subject,description,owner,completed,seq FROM tasks WHERE session_id=?1 ORDER BY seq,task_id").map_err(sql)?;
            let rows = stmt.query_map([id.as_str()], task_row).map_err(sql)?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(sql)
        })
    }
    fn save_tasks(&self, id: &SessionId, tasks: Vec<Task>) -> Result<Vec<Task>, YourAiError> {
        self.with(|c| {
            let tx = c;
            let mut saved = Vec::with_capacity(tasks.len());
            for mut task in tasks {
                let seq = i64::try_from(task.seq).map_err(sql)?;
                let existing = tx.query_row(
                    "SELECT task_id,subject,description,owner,completed,seq FROM tasks WHERE session_id=?1 AND task_id=?2",
                    params![id.as_str(), task.id], task_row,
                ).optional().map_err(sql)?;
                if let Some(previous) = existing {
                    if previous.subject != task.subject || previous.description != task.description
                        || previous.owner != task.owner || previous.seq != task.seq {
                        return Err(sql("task identity conflict"));
                    }
                    // Completion is monotonic, including a retry of an older create.
                    task.completed |= previous.completed;
                }
                tx.execute(
                    "INSERT INTO tasks(session_id,task_id,subject,description,owner,completed,seq) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(session_id,task_id) DO UPDATE SET completed=excluded.completed",
                    params![id.as_str(), task.id, task.subject, task.description, task.owner, task.completed, seq],
                ).map_err(sql)?;
                saved.push(task);
            }

            Ok(saved)
        })
    }

    fn initialize_system<'a>(
        &'a self,
        id: &'a SessionId,
        system: &'a str,
    ) -> BoxFuture<'a, Result<String, YourAiError>> {
        let id = id.clone();
        let system = system.to_owned();
        Box::pin(self.run(move |c| {
            let tx = c;
            tx.execute("UPDATE sessions SET system_prompt=?2 WHERE session_id=?1 AND system_prompt IS NULL", params![id.as_str(),system]).map_err(sql)?;
            let saved = tx.query_row("SELECT system_prompt FROM sessions WHERE session_id=?1", [id.as_str()], |r| r.get(0)).map_err(sql)?;
             Ok(saved)
        }))
    }

    fn create_session<'a>(
        &'a self,
        id: SessionId,
        system: &'a str,
    ) -> BoxFuture<'a, Result<SessionMeta, YourAiError>> {
        Box::pin(async move {
            let system = system.to_owned();
            let sid = id.clone();
            self.run(move |c| {
                c.execute(
                    "INSERT INTO sessions(session_id,created_at,updated_at,system_prompt) VALUES(?1,?2,?2,?3)",
                    params![sid.as_str(), now(), system],
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
        Box::pin(self.run(move |c| c.query_row("SELECT session_id,title,created_at,updated_at,model,parent_session_id,provider,system_prompt FROM sessions WHERE session_id=?1",[id.as_str()],meta_row).map_err(sql)))
    }
    fn save_session<'a>(&'a self, m: &'a SessionMeta) -> BoxFuture<'a, Result<(), YourAiError>> {
        let m = m.clone();
        Box::pin(self.run(move |c| { let n=c.execute("UPDATE sessions SET title=?2,model=?3,updated_at=?4,parent_session_id=?5,provider=?6 WHERE session_id=?1",params![m.id.as_str(),m.title,m.model,now(),m.parent_session_id.as_ref().map(|id|id.as_str()),m.provider]).map_err(sql)?; if n==0 {return Err(sql("unknown session"));} Ok(()) }))
    }
    fn list_sessions<'a>(&'a self) -> BoxFuture<'a, Result<Vec<SessionMeta>, YourAiError>> {
        Box::pin(self.run(|c| { let mut q=c.prepare("SELECT session_id,title,created_at,updated_at,model,parent_session_id,provider,system_prompt FROM sessions ORDER BY updated_at DESC,session_id").map_err(sql)?; let rows=q.query_map([],meta_row).map_err(sql)?.collect::<Result<Vec<_>,_>>().map_err(sql)?; Ok(rows) }))
    }
    fn delete_session<'a>(&'a self, id: &'a SessionId) -> BoxFuture<'a, Result<(), YourAiError>> {
        let id = id.clone();
        Box::pin(self.run(move |c| {
            let active: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM session_owners WHERE session_id=?1 AND expires_at>?2)", params![id.as_str(), now()], |r|r.get(0)).map_err(sql)?;
            if active { return Err(sql("session is running; close it before deletion")); }
            c.execute("DELETE FROM sessions WHERE session_id=?1", [id.as_str()]).map_err(sql)?;
            Ok(())
        }))
    }
    fn fork_session<'a>(
        &'a self,
        id: &'a SessionId,
    ) -> BoxFuture<'a, Result<SessionId, YourAiError>> {
        let id = id.clone();
        Box::pin(self.run(move |c| {
            let tx = c;
            let child=SessionId::new();
            let n=tx.execute("INSERT INTO sessions(session_id,parent_session_id,title,model,provider,created_at,updated_at,system_prompt) SELECT ?2,session_id,title,model,provider,?3,?3,system_prompt FROM sessions WHERE session_id=?1",params![id.as_str(),child.as_str(),now()]).map_err(sql)?;
            if n==0 { return Err(sql("unknown parent session")); }
            tx.execute("INSERT INTO messages SELECT lower(hex(randomblob(16))),?2,seq,kind,role,content_json,format_version,tool_call_id,token_count,status,created_at,tool_output_pruned_at FROM messages WHERE session_id=?1",params![id.as_str(),child.as_str()]).map_err(sql)?;
            // Copied messages retain their output IDs; copy their session-local
            // original bytes atomically so retrieval also works in the fork.
            tx.execute("INSERT INTO tool_outputs(session_id,output_id,offset,content,created_at) SELECT ?2,output_id,offset,content,created_at FROM tool_outputs WHERE session_id=?1",params![id.as_str(),child.as_str()]).map_err(sql)?;
            // Forked admissions use the same new identity as their copied message.
            tx.execute("UPDATE messages SET content_json=json_set(content_json,'$.yourai_input.UserText.id',message_id) WHERE session_id=?1 AND json_type(content_json,'$.yourai_input.UserText')='object'",[child.as_str()]).map_err(sql)?;
             Ok(child)
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
            let mut messages=q.query_map(params![id.as_str(),query.after,query.active_only,query.limit as i64+1],message_row).map_err(sql)?.collect::<Result<Vec<_>,_>>().map_err(sql)?;
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
            let tx = c;
            let mut result = vec![];
            for m in messages {
                if m.summary || m.status != MessageStatus::Active {
                    return Err(sql("append accepts active ordinary messages only"));
                }
                result.push(insert(tx, &id, m)?);
            }

            Ok(result)
        }))
    }
    fn save_summary<'a>(
        &'a self,
        id: &'a SessionId,
        sources: Vec<String>,
        summary: StoredMessage,
        progress: std::sync::Arc<std::sync::atomic::AtomicU8>,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        let id = id.clone();
        let db = self.clone();
        Box::pin(async move {
            let guard = db.gate.clone().lock_owned().await;
            tokio::task::spawn_blocking(move || {
                let _guard = guard;
                let result = db.with(|c| {
                    progress.store(
                        CommitStatus::Started as u8,
                        std::sync::atomic::Ordering::Release,
                    );
                    let tx = c;
                    tx.execute(
                        "UPDATE sessions SET updated_at=?2 WHERE session_id=?1",
                        params![id.as_str(), now()],
                    )
                    .map_err(sql)?;
                    if !summary.summary
                        || summary.status != MessageStatus::Active
                        || sources.is_empty()
                    {
                        return Err(sql("invalid compaction candidate"));
                    }
                    let unique: std::collections::HashSet<_> = sources.iter().collect();
                    if unique.len() != sources.len() {
                        return Err(sql("duplicate compaction source"));
                    }
                    // A completed retry is accepted only for the same summary identity/content and compacted sources.
                    let existing: Option<(String, String, String)> = tx
                        .query_row(
                            "SELECT session_id,content_json,kind FROM messages WHERE message_id=?1",
                            [&summary.id],
                            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                        )
                        .optional()
                        .map_err(sql)?;
                    let retry = existing.is_some();
                    if let Some((sid, raw, kind)) = existing {
                        if sid != id.as_str() || kind != "summary" || raw != stored_json(&summary)?
                        {
                            return Err(sql("summary identity conflict"));
                        }
                    }
                    for source in &sources {
                        let status: Option<String> = tx
                            .query_row(
                                "SELECT status FROM messages WHERE session_id=?1 AND message_id=?2",
                                params![id.as_str(), source],
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
                        for source in &sources {
                            tx.execute(
                                "UPDATE messages SET status='compacted' WHERE message_id=?1",
                                [source],
                            )
                            .map_err(sql)?;
                        }
                        insert(tx, &id, summary)?;
                    }

                    Ok(())
                });
                if result.is_ok() {
                    progress.store(
                        CommitStatus::Committed as u8,
                        std::sync::atomic::Ordering::Release,
                    );
                }
                result
            })
            .await
            .map_err(sql)?
        })
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
        // Honor locks held by old binaries without creating new state files.
        for name in ["host.lock", "history.lock"] {
            let path = dir.join(name);
            if !path.exists() {
                continue;
            }
            let file = std::fs::File::open(path).map_err(sql)?;
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
        let tx = c;
        for (meta,history) in sessions {
            let id=SessionId::from(meta.id);
            tx.execute("INSERT INTO sessions(session_id,title,model,created_at,updated_at) VALUES(?1,?2,?3,?4,?5)",params![id.as_str(),meta.title,meta.model,meta.created.checked_mul(1000).ok_or_else(||sql("timestamp overflow"))?,meta.updated.checked_mul(1000).ok_or_else(||sql("timestamp overflow"))?]).map_err(sql)?;
            for (index,message) in history.messages.into_iter().enumerate() {
                // Instructions are reloaded from host configuration, never imported as chat facts.
                if message.role==ChatRole::System {continue;}
                let row=insert(tx,&id,StoredMessage::new(message))?;
                if index<history.boundary {tx.execute("UPDATE messages SET status='compacted' WHERE message_id=?1",[row.id]).map_err(sql)?;}
            }
            if let Some(summary)=history.summary {
                let mut row=StoredMessage::new(ChatMessage::user(format!("[Conversation summary; historical context]\n{summary}")));row.summary=true;insert(tx,&id,row)?;
            }
            for (index,u) in history.usage.into_iter().enumerate() {
                let cv=|v:u64|i64::try_from(v).map_err(sql);
                tx.execute("INSERT INTO usage_events(usage_id,session_id,model,source,input_tokens,output_tokens,total_tokens,usage_json,created_at) VALUES(?1,?2,?3,'legacy',?4,?5,?6,?7,?8)",params![format!("legacy:{}:{index}",id.as_str()),id.as_str(),meta.model,cv(u.input_tokens)?,cv(u.output_tokens)?,cv(u.total_tokens)?,serde_json::to_string(&u).map_err(sql)?,meta.updated*1000]).map_err(sql)?;
            }
            // Do not import usage.json as well: it aggregates the same calls.
            tx.execute("UPDATE sessions SET updated_at=?2 WHERE session_id=?1",params![id.as_str(),meta.updated*1000]).map_err(sql)?;
        }
        Ok(())
    })?;
    drop(guards);
    Ok(count)
}

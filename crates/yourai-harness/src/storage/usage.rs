use crate::error;
use std::path::PathBuf;
use yourai_core::prelude::*;
/// Usage shares the session database; unknown provider counters remain NULL.
pub struct LocalUsage(pub crate::SqliteStore);
impl LocalUsage {
    pub fn open(path: PathBuf) -> Result<Self, YourAiError> {
        Ok(Self(crate::SqliteStore::open(&path)?))
    }
    async fn stats(&self, id: Option<&SessionId>) -> Result<UsageStats, YourAiError> {
        let id = id.cloned();
        self.0.run(move |c| c.query_row("SELECT COALESCE(SUM(input_tokens),0),COALESCE(SUM(output_tokens),0),COALESCE(SUM(COALESCE(total_tokens,input_tokens+output_tokens)),0),COUNT(*) FROM usage_events WHERE (?1 IS NULL OR session_id=?1)", [id.as_ref().map(|id|id.0.as_str())], |r| Ok(UsageStats{total_input_tokens:r.get(0)?,total_output_tokens:r.get(1)?,total_tokens:r.get(2)?,request_count:r.get(3)?})).map_err(|e|error("usage",e))).await
    }
}
impl UsageTracker for LocalUsage {
    fn record_event<'a>(
        &'a self,
        id: &'a SessionId,
        event: &'a UsageEvent,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(async move {
            let raw = serde_json::to_string(&event.usage).map_err(|e| error("usage", e))?;
            let id = id.clone();
            let event = event.clone();
            self.0.run(move |c| {
                use rusqlite::{params,OptionalExtension};
                let tx=c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(|e|error("usage",e))?;
                let previous:Option<(String,Option<String>,String,String)>=tx.query_row("SELECT session_id,model,source,usage_json FROM usage_events WHERE usage_id=?1",[&event.id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional().map_err(|e|error("usage",e))?;
                if let Some((sid,model,source,content))=previous {
                    if sid!=id.0 || model!=event.model || source!=event.source || content!=raw {return Err(error("usage","usage identity conflict"));}
                    return Ok(());
                }
                let u=&event.usage;
                let read=u.prompt_tokens_details.as_ref().and_then(|d|d.cached_tokens);
                let creation=u.prompt_tokens_details.as_ref().and_then(|d|d.cache_creation_tokens);
                let reasoning=u.completion_tokens_details.as_ref().and_then(|d|d.reasoning_tokens);
                tx.execute("INSERT INTO usage_events(usage_id,session_id,model,source,input_tokens,output_tokens,total_tokens,cache_read_tokens,cache_creation_tokens,reasoning_tokens,usage_json,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",params![event.id,id.0,event.model,event.source,u.prompt_tokens,u.completion_tokens,u.total_tokens,read,creation,reasoning,raw,crate::storage::sqlite::now()]).map_err(|e|error("usage",e))?;tx.commit().map_err(|e|error("usage",e))?;Ok(())
            }).await
        })
    }

    fn total<'a>(&'a self) -> BoxFuture<'a, Result<UsageStats, YourAiError>> {
        Box::pin(async { self.stats(None).await })
    }
    fn session_usage<'a>(
        &'a self,
        id: &'a SessionId,
    ) -> BoxFuture<'a, Result<UsageStats, YourAiError>> {
        Box::pin(async move { self.stats(Some(id)).await })
    }
    fn reset_session<'a>(&'a self, id: &'a SessionId) -> BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(async move {
            let id = id.clone();
            self.0
                .run(move |c| {
                    c.execute("DELETE FROM usage_events WHERE session_id=?1", [&id.0])
                        .map_err(|e| error("usage", e))?;
                    Ok(())
                })
                .await
        })
    }
}

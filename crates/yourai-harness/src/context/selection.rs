//! Safe history selection, independent of model calls and persistence.
use super::*;

/// A tool call and all its parallel results form one indivisible group.
pub(super) fn groups(records: &[StoredMessage]) -> Vec<std::ops::Range<usize>> {
    let mut open = HashSet::new();
    let mut result = vec![];
    let mut start = 0;
    for (i, m) in records.iter().enumerate() {
        open.extend(
            m.message
                .content
                .tool_calls()
                .iter()
                .map(|c| c.call_id.clone()),
        );
        for r in m.message.content.tool_responses() {
            open.remove(&r.call_id);
        }
        if open.is_empty() {
            result.push(start..i + 1);
            start = i + 1;
        }
    }
    result
}

/// Retain complete batches within the token budget and at most the configured
/// number of turns. Unfinished batches and the latest message remain protected.
pub(super) fn tail_start(
    context: &MemoryContext,
    records: &[StoredMessage],
    groups: &[std::ops::Range<usize>],
    budget: u64,
    execution: &ContextExecution,
) -> Result<usize, YourAiError> {
    let mut total = 0u64;
    let mut start = records.len().saturating_sub(1);
    let turn_start = records
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, m)| !m.summary && !m.runtime_context && m.message.role == ChatRole::User)
        .take(context.services.policy.keep_recent_turns)
        .last()
        .map(|(i, _)| i)
        .unwrap_or(records.len());
    for group in groups.iter().rev() {
        if group.end <= turn_start {
            break;
        }
        let size = context.estimate_raw(
            &ChatRequest::new(context.project_messages(&records[group.clone()], true)?),
            execution,
        )?;
        if total.saturating_add(size) > budget {
            break;
        }
        total += size;
        start = start.min(group.start);
    }
    Ok(start)
}

//! Delivery policy for paid responses, shared by main, compaction and hook calls.
use yourai_core::prelude::*;

/// Accounting failure must never turn a successful model call into a model retry.
/// Retries reuse the event identity, including an uncertain commit. Persistent
/// failure is reported explicitly; this is bounded delivery, not a durable outbox.
pub(crate) async fn record_response(
    tracker: Option<&dyn UsageTracker>,
    session: &SessionId,
    model: &str,
    source: &str,
    usage: GenaiUsage,
) -> Option<String> {
    let tracker = tracker?;
    let event = UsageEvent::new(Some(model.into()), source, usage);
    let mut failure = None;
    for _ in 0..3 {
        match tracker.record_event(session, &event).await {
            Ok(()) => return None,
            Err(error) => failure = Some(error),
        }
    }
    let warning = format!(
        "Usage accounting incomplete (event {}): {}",
        event.id,
        failure.unwrap()
    );
    // Also report outside the execution channel: a compaction or hook may have
    // no output sink, or its caller may fail after the paid response.
    eprintln!("{warning}");
    Some(warning)
}

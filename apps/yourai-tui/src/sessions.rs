//! Session list filtering and display helpers shared by the launcher
//! and the `/sessions` overlay. Pure functions only — no I/O — so they
//! can be unit-tested without a database.
use std::cmp::Reverse;
use yourai_core::prelude::{SessionId, SessionMeta};

/// A row in the sessions list, derived from `SessionMeta` plus a precomputed
/// display label. Filtering and rendering work off this type so the launcher
/// (which has no View) and the overlay share the same code path.
#[derive(Clone, Debug)]
pub struct SessionRow {
    pub id: SessionId,
    pub title: String,
    pub model: String,
    pub updated_at: i64,
    pub is_current: bool,
}

/// Build the display rows from raw session metadata, hiding subagent child
/// sessions (which carry a `parent_session_id`) and sorting newest-first.
/// `current` marks the active session so the overlay can highlight it.
pub fn rows_from(meta: Vec<SessionMeta>, current: Option<&SessionId>) -> Vec<SessionRow> {
    let mut filtered: Vec<_> = meta
        .into_iter()
        .filter(|m| m.parent_session_id.is_none())
        .collect();
    filtered.sort_by_key(|row| Reverse(row.updated_at));
    filtered
        .into_iter()
        .map(|m| SessionRow {
            is_current: current == Some(&m.id),
            title: m
                .title
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| "Untitled session".into()),
            model: m.model.unwrap_or_default(),
            id: m.id,
            updated_at: m.updated_at,
        })
        .collect()
}

/// Case-insensitive substring match across title, session id (first 8 chars)
/// and model. Returns the indices into `rows` that match (preserving order).
pub fn filter_sessions(rows: &[SessionRow], query: &str) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return (0..rows.len()).collect();
    }
    rows.iter()
        .enumerate()
        .filter(|(_, r)| {
            r.title.to_lowercase().contains(&q)
                || r.id.0.to_lowercase().contains(&q)
                || r.model.to_lowercase().contains(&q)
        })
        .map(|(i, _)| i)
        .collect()
}

/// One display row for the sessions list: title elided to `title_w` display
/// columns (width-padded so CJK titles align), short id, model and relative
/// time. Shared by the launcher and the `/sessions` overlay so both render
/// identical bodies.
pub fn row_body(row: &SessionRow, title_w: usize, now: i64) -> String {
    let title = crate::text::elide(&row.title, title_w);
    let pad = title_w.saturating_sub(unicode_width::UnicodeWidthStr::width(title.as_str()));
    let id8: String = row.id.0.chars().take(8).collect();
    let model = if row.model.is_empty() {
        "—".to_string()
    } else {
        crate::text::elide(&row.model, 16)
    };
    format!(
        "{}{} {} · {:<16} · {}",
        title,
        " ".repeat(pad),
        id8,
        model,
        relative_time(row.updated_at, now)
    )
}

/// Relative time label: `2h ago`, `3d ago`, or `YYYY-MM-DD` beyond 30 days.
/// `now` is unix seconds; pass `SystemTime::now()` from the caller.
pub fn relative_time(updated_at: i64, now: i64) -> String {
    let delta = now.saturating_sub(updated_at).max(0);
    if delta < 60 {
        return "just now".into();
    }
    let mins = delta / 60;
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = hours / 24;
    if days < 30 {
        return format!("{days}d ago");
    }
    // Beyond 30 days, fall back to a calendar date. Use UTC to stay independent
    // of local timezone rules — the day boundary is approximate, which is fine
    // for "this session is months old" context.
    let secs = updated_at.max(0) as u64;
    let (year, month, day) = utc_ymd(secs);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Convert unix seconds to (year, month, day) in UTC, using a simple
/// proleptic Gregorian algorithm. Good enough for display labels.
fn utc_ymd(secs: u64) -> (i32, u32, u32) {
    let days = (secs / 86_400) as i64;
    // 1970-01-01 is day 0. Use the classic Howard Hinnant algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    let year = (y + if m <= 2 { 1 } else { 0 }) as i32;
    (year, m, d)
}

#[cfg(test)]
mod tests {
    #[allow(clippy::wildcard_imports)]
    use super::*;

    fn meta(id: &str, title: Option<&str>, model: Option<&str>, updated: i64) -> SessionMeta {
        SessionMeta {
            system_prompt: None,
            id: SessionId(id.into()),
            title: title.map(str::to_owned),
            parent_session_id: None,
            provider: None,
            created_at: updated - 60,
            updated_at: updated,
            model: model.map(str::to_owned),
        }
    }

    #[test]
    fn rows_hide_subagent_children_and_sort_newest_first() {
        let mut child = meta("child", Some("subagent"), None, 2000);
        child.parent_session_id = Some(SessionId("parent".into()));
        let metas = vec![
            meta("old", Some("Old session"), Some("kimi"), 1000),
            child,
            meta("new", Some("New session"), Some("glm"), 3000),
        ];
        let rows = rows_from(metas, None);
        assert_eq!(rows.len(), 2); // child filtered out
        assert_eq!(rows[0].id.0, "new"); // newest first
        assert_eq!(rows[1].id.0, "old");
        assert_eq!(rows[0].title, "New session");
    }

    #[test]
    fn untitled_sessions_get_placeholder() {
        let rows = rows_from(vec![meta("a", None, None, 0)], None);
        assert_eq!(rows[0].title, "Untitled session");
    }

    #[test]
    fn filter_matches_title_id_short_prefix_and_model() {
        let rows = rows_from(
            vec![
                meta("d3f40178", Some("Fix parser"), Some("kimi-k3"), 0),
                meta("9a1b2c3d", Some("Fix TUI sidebar"), Some("glm-4.6"), 0),
                meta("5e6f7a8b", Some("Untitled"), Some("kimi-k3"), 0),
            ],
            None,
        );
        // Empty query returns all.
        assert_eq!(filter_sessions(&rows, "").len(), 3);
        // "fix" matches both titled sessions (case-insensitive).
        assert_eq!(filter_sessions(&rows, "fix").len(), 2);
        // "d3f40178" matches by full id; "d3f4" also matches by short prefix.
        assert_eq!(filter_sessions(&rows, "d3f4").len(), 1);
        // "kimi" matches both kimi models.
        assert_eq!(filter_sessions(&rows, "kimi").len(), 2);
        // "glm" matches the glm session only.
        assert_eq!(filter_sessions(&rows, "glm").len(), 1);
    }

    #[test]
    fn row_body_elides_cjk_titles_by_display_width_and_aligns_columns() {
        let row = SessionRow {
            id: SessionId("d3f40178abcdef".into()),
            title: "修复中文解析器的边界问题".into(),
            model: "kimi-k3".into(),
            updated_at: 0,
            is_current: false,
        };
        // title_w = 10 columns: four CJK glyphs (8 cols) + ellipsis, padded
        // one column so the id column starts exactly at title_w.
        let body = row_body(&row, 10, 100);
        assert!(body.contains("修复中文…"), "got: {body}");
        let title_end = body.find(" d3f40178").unwrap();
        let title_cols = unicode_width::UnicodeWidthStr::width(&body[..title_end]);
        assert_eq!(title_cols, 10, "title column is display-width aligned");
        assert!(body.contains("kimi-k3"));
        // An empty model renders the placeholder, not an empty column.
        let bare = SessionRow {
            model: String::new(),
            ..row
        };
        assert!(row_body(&bare, 10, 100).contains("—"));
    }

    #[test]
    fn relative_time_transitions_through_buckets() {
        // Use a modern anchor so the 30+ day branch lands in the 21st century.
        let now = 1_700_000_000i64; // 2023-11-14 UTC
        assert_eq!(relative_time(now - 30, now), "just now");
        assert_eq!(relative_time(now - 120, now), "2m ago");
        assert_eq!(relative_time(now - 3 * 3600, now), "3h ago");
        assert_eq!(relative_time(now - 2 * 86_400, now), "2d ago");
        // 60 days ago falls back to YYYY-MM-DD.
        let label = relative_time(now - 60 * 86_400, now);
        assert_eq!(label.len(), 10);
        assert!(label.starts_with("2023-"), "got {label}");
    }

    #[test]
    fn utc_ymd_known_anchors() {
        assert_eq!(utc_ymd(0), (1970, 1, 1)); // epoch
        assert_eq!(utc_ymd(86_400), (1970, 1, 2)); // +1 day
        assert_eq!(utc_ymd(1_700_000_000), (2023, 11, 14)); // common anchor
    }
}

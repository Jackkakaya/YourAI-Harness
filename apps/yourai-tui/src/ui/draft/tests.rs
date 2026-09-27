use super::*;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}
fn image() -> clipboard::ClipboardImage {
    clipboard::ClipboardImage {
        mime: "image/png".into(),
        data: "aGVsbG8=".into(),
    }
}
fn pending_image(
    draft: &mut Draft,
) -> tokio::sync::oneshot::Sender<std::io::Result<Option<clipboard::ClipboardImage>>> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    draft.current.image = Some(tokio::spawn(async move { rx.await.unwrap() }));
    tx
}
async fn settle(draft: &mut Draft) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while draft.scan.is_some() || draft.current.image.is_some() {
            draft.poll().await;
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn clear_and_submit_invalidate_even_completed_image_tasks() {
    for submitted in [false, true] {
        let mut draft = Draft::default();
        draft.stage_image(image());
        let release = pending_image(&mut draft);
        release.send(Ok(Some(image()))).ok().unwrap();
        // The completion exists but has not been applied to the draft.
        tokio::task::yield_now().await;
        if submitted {
            draft.submitted();
        } else {
            assert!(draft.intercept(key(KeyCode::Esc)));
        }
        draft.poll().await;
        assert!(draft.is_empty());
    }
}

#[tokio::test]
async fn moving_a_draft_preserves_both_staged_and_inflight_images() {
    let mut old = Draft::default();
    old.insert("new draft");
    old.stage_image(image());
    let release = pending_image(&mut old);
    let mut moved = std::mem::take(&mut old);
    release.send(Ok(Some(image()))).ok().unwrap();
    settle(&mut moved).await;
    assert!(old.is_empty());
    assert_eq!(moved.text(), "new draft");
    assert_eq!(moved.attachment_counts(), (2, 0));
}

#[tokio::test]
async fn dropping_a_draft_cancels_its_clipboard_work() {
    let mut draft = Draft::default();
    let release = pending_image(&mut draft);
    drop(draft);
    tokio::task::yield_now().await;
    assert!(release.is_closed());
}

#[tokio::test]
async fn keyboard_and_paste_share_completion_and_attachment_lifetime() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.rs"), "fn main() {}").unwrap();
    let mut draft = Draft::default();
    draft.set_cwd(dir.path());
    draft.key(key(KeyCode::Char('@')));
    assert!(!draft.intercept(key(KeyCode::Char('n'))));
    draft.key(key(KeyCode::Char('n')));
    draft.insert("otes");
    settle(&mut draft).await;
    assert_eq!(draft.mention().query, "notes");
    assert!(draft.intercept(key(KeyCode::Enter)));
    assert_eq!(draft.text(), "@notes.rs ");
    assert_eq!(draft.attachment_counts(), (0, 1));
    // Erasing the marker releases its attachment, including the badge.
    draft.set_text("just text");
    assert!(draft.attachments().is_empty());
    draft.insert(" @notes.rs");
    assert!(
        draft.attachments().is_empty(),
        "a deleted attachment must not resurrect from text"
    );
}

#[tokio::test]
async fn changing_scan_root_cannot_apply_results_from_the_old_root() {
    let old = tempfile::tempdir().unwrap();
    let new = tempfile::tempdir().unwrap();
    std::fs::write(old.path().join("old.rs"), "old").unwrap();
    std::fs::write(new.path().join("new.rs"), "new").unwrap();
    let mut draft = Draft::default();
    draft.set_cwd(old.path());
    draft.insert("@");
    draft.set_cwd(new.path());
    settle(&mut draft).await;
    assert_eq!(draft.mention().entries.len(), 1);
    assert_eq!(draft.mention().entries[0].path, new.path().join("new.rs"));
}

#[test]
fn explicit_images_survive_text_edits_but_references_require_whole_markers() {
    let mut draft = Draft::default();
    draft.stage_image(image());
    draft.set_text("see @src/main.rs");
    draft.current.attachments.push(PendingAttachment {
        marker: Some("@src".into()),
        attachment: UserAttachment::file("/abs/src", None),
    });
    draft.set_text("see @src/main.rs");
    assert_eq!(draft.attachments().len(), 1);
    for text in ["see @src now", "see @src"] {
        assert!(marker_present(text, "@src"));
    }
    for text in ["see foo@src", "see @src/main.rs", "@src.rs"] {
        assert!(!marker_present(text, "@src"));
    }
}

#[test]
fn history_restores_complete_inputs_and_the_unsubmitted_draft() {
    let mut draft = Draft::default();
    draft.insert("sent");
    draft.stage_image(image());
    draft.submitted();
    draft.insert("scratch");
    draft.stage_image(clipboard::ClipboardImage {
        mime: "image/png".into(),
        data: "different".into(),
    });
    draft.key(key(KeyCode::Left));
    let cursor = draft.editor().cursor;
    draft.key(key(KeyCode::Up));
    assert_eq!(draft.text(), "sent");
    assert!(matches!(&draft.attachments()[0].data, AttachmentData::Base64(s) if s == "aGVsbG8="));
    draft.clear_attachments();
    draft.set_text("edited recalled input");
    draft.key(key(KeyCode::Down));
    assert_eq!(draft.text(), "scratch");
    assert_eq!(draft.editor().cursor, cursor);
    assert!(matches!(&draft.attachments()[0].data, AttachmentData::Base64(s) if s == "different"));
    draft.key(key(KeyCode::Up));
    assert_eq!(
        draft.attachment_counts(),
        (1, 0),
        "editing a recalled copy must not mutate stored history"
    );
    draft.submitted();
    assert_eq!(
        draft.text(),
        "scratch",
        "sending a historical input preserves the saved draft"
    );
}

#[tokio::test]
async fn history_preserves_inflight_clipboard_work_on_its_original_draft() {
    let mut draft = Draft::default();
    draft.insert("old");
    draft.submitted();
    draft.insert("new");
    let release = pending_image(&mut draft);
    draft.key(key(KeyCode::Up));
    release.send(Ok(Some(image()))).ok().unwrap();
    tokio::task::yield_now().await;
    draft.poll().await;
    assert_eq!(draft.text(), "old");
    assert!(draft.attachments().is_empty());
    draft.key(key(KeyCode::Down));
    settle(&mut draft).await;
    assert_eq!(draft.text(), "new");
    assert_eq!(draft.attachment_counts(), (1, 0));
}

#[tokio::test]
async fn dropping_while_browsing_cancels_the_saved_drafts_image_read() {
    let mut draft = Draft::default();
    draft.remember("old");
    let release = pending_image(&mut draft);
    draft.key(key(KeyCode::Up));
    drop(draft);
    tokio::task::yield_now().await;
    assert!(release.is_closed());
}

#[test]
fn history_navigation_preserves_multiline_cursor_movement() {
    let mut draft = Draft::default();
    draft.remember("old");
    draft.insert("line1\nline2");
    draft.key(key(KeyCode::Up));
    assert_eq!(draft.text(), "line1\nline2");
    assert_eq!(draft.editor().cursor, 5);
    draft.key(key(KeyCode::Up));
    assert_eq!(draft.text(), "old");
    draft.key(key(KeyCode::Down));
    assert_eq!(draft.text(), "line1\nline2");
    assert_eq!(draft.editor().cursor, 5);
}

#[tokio::test]
async fn history_recalls_file_references_without_opening_completion() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("file.rs"), "content").unwrap();
    let mut draft = Draft::default();
    draft.set_cwd(dir.path());
    draft.insert("@file");
    settle(&mut draft).await;
    assert!(draft.intercept(key(KeyCode::Enter)));
    let original = serde_json::to_value(draft.attachments()).unwrap();
    draft.submitted();
    draft.key(key(KeyCode::Up));
    assert_eq!(serde_json::to_value(draft.attachments()).unwrap(), original);
    assert!(!draft.mention().active);
    draft.key(key(KeyCode::Down));
    assert!(draft.is_empty());
}

#[test]
fn equal_text_with_different_attachments_remains_distinct_history() {
    let mut draft = Draft::default();
    for data in ["first", "second"] {
        draft.insert("same question");
        draft.stage_image(clipboard::ClipboardImage {
            mime: "image/png".into(),
            data: data.into(),
        });
        draft.submitted();
    }
    for data in ["second", "first"] {
        draft.key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert_eq!(draft.text(), "same question");
        assert!(matches!(&draft.attachments()[0].data, AttachmentData::Base64(s) if s == data));
    }
}

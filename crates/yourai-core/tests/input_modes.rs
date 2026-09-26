use serde_json::json;
use yourai_core::protocol::{AttachmentData, FileRef, In, InputMode, UserAttachment};

#[test]
fn input_mode_defaults_for_old_wire_messages_and_survives_round_trip() {
    let old: In = serde_json::from_value(json!({"UserText": {"text": "hello"}})).unwrap();
    assert!(matches!(
        old,
        In::UserText {
            mode: InputMode::Steer,
            ..
        }
    ));

    let wire = serde_json::to_value(In::follow_up("later")).unwrap();
    assert_eq!(
        wire,
        json!({"UserText": {"text": "later", "mode": "follow_up"}})
    );
    let restored: In = serde_json::from_value(wire).unwrap();
    assert!(
        matches!(restored, In::UserText { text, mode: InputMode::FollowUp, .. } if text == "later")
    );
}

#[test]
fn attachments_round_trip_and_default_empty_for_old_wire() {
    // Historical wire messages carry no `attachments` field — they must
    // deserialize to an empty vec (backward compatibility).
    let old: In =
        serde_json::from_value(json!({"UserText": {"text": "hi", "mode": "steer"}})).unwrap();
    assert!(matches!(
        old,
        In::UserText { attachments, .. } if attachments.is_empty()
    ));

    // A message with an inline base64 attachment round-trips through the wire
    // format — the untagged AttachmentData::Base64 serializes as a plain
    // string, so the wire shape is byte-identical to the pre-FileRef format.
    let msg = In::user_text_with_attachments(
        "what is this?",
        vec![UserAttachment::base64(
            "image/png",
            "iVBORw0KGgo=",
            Some("clip.png".into()),
        )],
    );
    let wire = serde_json::to_value(&msg).unwrap();
    assert_eq!(
        wire,
        json!({
            "UserText": {
                "text": "what is this?",
                "mode": "steer",
                "attachments": [{
                    "content_type": "image/png",
                    "data": "iVBORw0KGgo=",
                    "name": "clip.png"
                }]
            }
        })
    );
    let restored: In = serde_json::from_value(wire).unwrap();
    assert!(
        matches!(
            &restored,
            In::UserText { text, attachments, .. }
            if text == "what is this?"
                && attachments.len() == 1
                && matches!(&attachments[0].data, AttachmentData::Base64(d) if d == "iVBORw0KGgo=")
        ),
        "old wire shape must deserialize back to Base64"
    );

    // Empty attachments are omitted from the wire (skip_serializing_if),
    // keeping the format identical to pre-attachment messages.
    let wire2 = serde_json::to_value(In::user_text("plain")).unwrap();
    assert_eq!(wire2, json!({"UserText": {"text": "plain", "mode": "steer"}}));
}

#[test]
fn file_ref_attachments_round_trip_and_parse_from_wire() {
    // File references serialize as an object; `lines` is omitted when absent
    // and empty `content_type` is skipped (File form derives MIME at resolve
    // time).
    let msg = In::user_text_with_attachments(
        "review this",
        vec![UserAttachment::file("src/main.rs", None)],
    );
    let wire = serde_json::to_value(&msg).unwrap();
    assert_eq!(
        wire,
        json!({
            "UserText": {
                "text": "review this",
                "mode": "steer",
                "attachments": [{ "data": { "path": "src/main.rs" }, "name": null }]
            }
        })
    );

    // Line windows round-trip: 1-based inclusive [start, end] as a JSON pair.
    let ranged = serde_json::to_value(UserAttachment::file("README.md", Some((10, 20)))).unwrap();
    assert_eq!(
        ranged,
        json!({"data": {"path": "README.md", "lines": [10, 20]}, "name": null})
    );

    // Wire → struct: explicit lines, absent lines, and absent content_type.
    let restored: UserAttachment = serde_json::from_value(json!({
        "content_type": "",
        "data": { "path": "a/b.txt", "lines": [3, 9] },
        "name": null
    }))
    .unwrap();
    assert!(matches!(
        &restored,
        UserAttachment {
            content_type,
            data: AttachmentData::File(FileRef { path, lines: Some((3, 9)) }),
            ..
        }
        if content_type.is_empty() && path == "a/b.txt"
    ));
    let restored: UserAttachment =
        serde_json::from_value(json!({"data": {"path": "notes"}, "name": null})).unwrap();
    assert!(matches!(
        &restored,
        UserAttachment { data: AttachmentData::File(FileRef { lines: None, .. }), .. }
    ));

    // Mixed forms in one message: old base64 string and new file object
    // coexist (untagged dispatch per element).
    let mixed: In = serde_json::from_value(json!({
        "UserText": {
            "text": "both",
            "mode": "steer",
            "attachments": [
                { "content_type": "image/png", "data": "iVBORw0KGgo=", "name": "a.png" },
                { "data": { "path": "src/lib.rs" }, "name": null }
            ]
        }
    }))
    .unwrap();
    assert!(matches!(
        &mixed,
        In::UserText { attachments, .. }
        if attachments.len() == 2
            && matches!(&attachments[0].data, AttachmentData::Base64(_))
            && matches!(&attachments[1].data, AttachmentData::File(_))
    ));
}

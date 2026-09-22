use serde_json::json;
use yourai_core::protocol::{In, InputMode, UserAttachment};

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

    // A message with an attachment round-trips through the wire format.
    let msg = In::user_text_with_attachments(
        "what is this?",
        vec![UserAttachment {
            content_type: "image/png".into(),
            data: "iVBORw0KGgo=".into(),
            name: Some("clip.png".into()),
        }],
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
    assert!(matches!(
        restored,
        In::UserText { text, attachments, .. }
        if text == "what is this?" && attachments.len() == 1
    ));

    // Empty attachments are omitted from the wire (skip_serializing_if),
    // keeping the format identical to pre-attachment messages.
    let wire2 = serde_json::to_value(In::user_text("plain")).unwrap();
    assert_eq!(
        wire2,
        json!({"UserText": {"text": "plain", "mode": "steer"}})
    );
}

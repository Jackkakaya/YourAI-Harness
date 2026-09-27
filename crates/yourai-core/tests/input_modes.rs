use serde_json::json;
use yourai_core::protocol::{In, InputMode};

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

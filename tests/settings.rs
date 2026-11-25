use universal_dv_transfer::config::Settings;

#[test]
fn legacy_capture_control_does_not_disable_tape_recording() {
    let settings: Settings = serde_json::from_value(serde_json::json!({
        "capture": { "control_deck": false },
        "playlist": ["output-pal.dv"]
    }))
    .unwrap();
    assert!(!settings.capture.control_deck);
    assert!(settings.record.control_deck);
    assert_eq!(
        settings.playlist,
        [std::path::PathBuf::from("output-pal.dv")]
    );
    assert!(Settings::default().record.control_deck);
}

#[test]
fn explicit_manual_tape_output_survives_settings_roundtrip() {
    let settings: Settings = serde_json::from_value(serde_json::json!({
        "capture": { "control_deck": true },
        "record": { "control_deck": false }
    }))
    .unwrap();
    let saved = serde_json::to_vec(&settings).unwrap();
    let restored: Settings = serde_json::from_slice(&saved).unwrap();
    assert!(restored.capture.control_deck);
    assert!(!restored.record.control_deck);
}

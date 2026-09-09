use openwave_core::model::*;
use serde_json::json;

#[test]
fn legacy_binding_is_one_identity_even_with_commas() {
    let data = json!({"music": {"match_app_name":"  Player, Classical  ", "level":"0.375", "custom":{"keep":true}}, "voice": {"match_app_names":[" One ","One","", "Two"],"match_app_name":"ignored"}});
    let records = normalize_sources(data).unwrap();
    assert_eq!(
        records.keys().map(SourceId::as_str).collect::<Vec<_>>(),
        ["music", "voice"]
    );
    assert_eq!(records["music"].match_app_names, ["Player, Classical"]);
    assert_eq!(records["voice"].match_app_names, ["One", "Two"]);
    assert_eq!(records["music"].level, 0.375);
    let saved = serde_json::to_value(&records).unwrap();
    assert_eq!(saved["music"]["custom"], json!({"keep":true}));
    assert!(saved["music"].get("match_app_name").is_none());
    assert_eq!(normalize_sources(saved).unwrap(), records);
}

#[test]
fn group_loading_keeps_only_first_open_in_source_order() {
    let records = normalize_sources(
        json!({"z": {"group":" Room "}, "a": {"group":"Room"}, "b": {"group":"Elsewhere"}}),
    )
    .unwrap();
    assert!(!records["z"].muted);
    assert!(records["a"].muted);
    assert!(!records["b"].muted);
    let all_muted = normalize_sources(
        json!({"a":{"group":"Room","muted":true},"b":{"group":"Room","muted":true}}),
    )
    .unwrap();
    assert!(all_muted.values().all(|source| source.muted));
}

#[test]
fn stable_ids_and_sink_names_reject_ambiguous_topology() {
    assert!(normalize_sources(json!({"bad.id":{}})).is_err());
    assert!(normalize_sources(json!({"valid":{"id":"another"}})).is_err());
    assert!(
        normalize_mixes(json!({"one":{"sink":"openwave_shared"},"two":{"sink":"openwave_shared"}}))
            .is_err()
    );
    assert!(normalize_mixes(json!({"one":{"sink":"openwave_capture_one"}})).is_err());
    assert!(serde_json::from_value::<SourceId>(json!("../other")).is_err());
}

#[test]
fn explicit_empty_definitions_do_not_become_default_routes() {
    assert!(normalize_mixes(json!({})).unwrap().is_empty());
    let defaults = default_mixes();
    assert_eq!(
        defaults
            .values()
            .map(|mix| mix.sink.as_str())
            .collect::<Vec<_>>(),
        [
            "openwave_personal_mix",
            "openwave_chat_mix",
            "openwave_record_mix"
        ]
    );
}

#[test]
fn matrix_migration_preserves_small_existing_send_and_extensions() {
    let state = MatrixState::from_value(json!({"music.personal":{"volume":"0.005","muted":false,"annotation":"keep"},"output":"legacy_headphones","outputs":{"personal":"chosen_headphones"},"custom":[1,2]})).unwrap();
    assert_eq!(state.cells["music.personal"].volume, 0.005);
    assert_eq!(
        state.output(&MixId::new("personal").unwrap()),
        "chosen_headphones"
    );
    assert_eq!(send_level(0.005).unwrap(), 0.0);
    let saved = state.to_value().unwrap();
    assert_eq!(saved["music.personal"]["annotation"], "keep");
    assert_eq!(saved["custom"], json!([1, 2]));
    assert_eq!(saved["volumes"], json!({}));
    assert_eq!(MatrixState::from_value(saved).unwrap(), state);
}

#[test]
fn legacy_output_is_validated_after_canonical_override() {
    assert!(MatrixState::from_value(json!({"output":""})).is_err());
    assert!(
        MatrixState::from_value(json!({"output":"headphones","outputs":{"personal":""}})).is_err()
    );
    let state =
        MatrixState::from_value(json!({"output":"","outputs":{"personal":"chosen_headphones"}}))
            .unwrap();
    assert_eq!(
        state.output(&MixId::new("personal").unwrap()),
        "chosen_headphones"
    );
    assert_eq!(
        MatrixState::from_value(state.to_value().unwrap()).unwrap(),
        state
    );
    let legacy = MatrixState::from_value(json!({"output":"legacy_headphones"})).unwrap();
    assert_eq!(
        legacy.output(&MixId::new("personal").unwrap()),
        "legacy_headphones"
    );
    assert_eq!(
        MatrixState::from_value(legacy.to_value().unwrap()).unwrap(),
        legacy
    );
}

#[test]
fn legacy_numeric_coercion_does_not_weaken_new_command_validation() {
    assert_eq!(
        normalize_sources(json!({"one":{"level":true}})).unwrap()["one"].level,
        1.0
    );
    assert!(command_level(f64::NAN).is_err());
    assert!(command_level(1.1).is_err());
    assert!(normalize_sources(json!({"one":{"muted":"false"}})).is_err());
    assert!(MatrixState::from_value(json!({"one.two":{"volume":"NaN"}})).is_err());
}

#[test]
fn malformed_preferences_fail_instead_of_becoming_truthy() {
    assert!(Preferences::from_value(json!({"maximized":"false"})).is_err());
    assert!(Preferences::from_value(json!({"gain_locked":1})).is_err());
    assert!(Preferences::from_value(json!({"offered_capture_nodes":[1]})).is_err());
    let prefs =
        Preferences::from_value(json!({"width":1,"height":100,"extension":{"keep":true}})).unwrap();
    assert_eq!((prefs.width, prefs.height), (820, 480));
    assert_eq!(
        serde_json::to_value(prefs).unwrap()["extension"],
        json!({"keep":true})
    );
}

#[test]
fn public_snapshot_omits_private_bindings_and_keeps_silent_cells() {
    let sources = normalize_sources(json!({"music":{"name":"Music","match_app_names":["private application"],"private_serial":"secret"}})).unwrap();
    let snapshot = AppSnapshot {
        desired: std::sync::Arc::new(DesiredState {
            sources,
            mixes: default_mixes(),
            ..DesiredState::default()
        }),
        ..AppSnapshot::default()
    };
    let encoded = snapshot.action_snapshot().unwrap();
    let value: serde_json::Value = serde_json::from_str(&encoded).unwrap();
    assert!(!encoded.contains("private application"));
    assert!(!encoded.contains("secret"));
    assert_eq!(value["sources"][0]["id"], "music");
    assert_eq!(
        value["cells"]["music.personal"],
        json!({"volume":0.0,"muted":false})
    );
    assert_eq!(value["outputs"]["personal"], "auto");
    assert_eq!(value["outputs"]["chat"], "none");
    assert_eq!(value["volumes"], json!({}));
}

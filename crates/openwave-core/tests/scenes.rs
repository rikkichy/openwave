use indexmap::IndexMap;
use openwave_core::model::UnitId;
use openwave_core::profiles::ProfileId;
use openwave_core::scenes::{
    HardwareCandidate, HardwarePatch, LevelPatch, Scene, SceneId, SourcePatch, decode_store,
    encode_store, hardware_key, pick_hardware_entry, resolve_hardware,
};
use serde_json::{Value, json};

fn unit(profile: ProfileId, address: u8) -> UnitId {
    UnitId {
        profile,
        bus: 1,
        address,
        incarnation: 1,
    }
}

fn hardware(value: Value) -> IndexMap<String, HardwarePatch> {
    serde_json::from_value(value).unwrap()
}

#[test]
fn stored_ids_are_preserved_and_new_names_replace_by_ascii_slug() {
    let original = "Legacy ID.with/slash";
    let id: SceneId = serde_json::from_value(json!(original)).unwrap();
    assert_eq!(id.as_str(), original);
    assert_eq!(serde_json::to_value(&id).unwrap(), json!(original));
    assert!(serde_json::from_value::<SceneId>(json!("")).is_err());
    assert!(serde_json::from_value::<SceneId>(json!(7)).is_err());
    assert_eq!(SceneId::from_name("  On Air! ").as_str(), "on-air");
    assert_eq!(SceneId::from_name("***").as_str(), "scene");
    assert_eq!(
        SceneId::from_name("Crème / KELVIN 42").as_str(),
        "cr-me-kelvin-42"
    );
    let mut store = IndexMap::new();
    store.insert(
        SceneId::from_name("On Air!"),
        Scene::from_value(json!({"name": "On Air!", "sources": {"mic": {"level": 0.7}}})).unwrap(),
    );
    store.insert(
        SceneId::from_name("on air"),
        Scene::from_value(json!({"name": "on air", "sources": {}})).unwrap(),
    );
    assert_eq!(
        encode_store(&store).unwrap(),
        json!({"scenes": {"on-air": {"name": "on air", "sources": {}}}})
    );
}

#[test]
fn absent_and_empty_sections_and_null_outputs_round_trip_distinctly() {
    let value = json!({"scenes": {
        "absent": {"name": ""},
        "empty": {"name": "Empty", "sources": {}, "cells": {}, "outputs": {}, "volumes": {}, "hardware": {}},
        "outputs": {"name": "Outputs", "outputs": {"personal": null, "chat": "", "record": "none"}}
    }});
    let store = decode_store(value.clone()).unwrap();
    assert_eq!(encode_store(&store).unwrap(), value);
    let absent = &store[&SceneId::new("absent").unwrap()];
    let empty = &store[&SceneId::new("empty").unwrap()];
    assert_eq!(absent.sources, None);
    assert_eq!(empty.sources, Some(IndexMap::new()));
    assert_eq!(
        decode_store(json!({"scenes": {}})).unwrap(),
        IndexMap::new()
    );
}

#[test]
fn source_and_scene_order_survives_for_recall_precedence() {
    let input = r#"{"scenes":{"z":{"name":"First","sources":{"backup":{"muted":false},"primary":{"muted":false}}},"a":{"name":"Second"}}}"#;
    let store = decode_store(serde_json::from_str(input).unwrap()).unwrap();
    assert_eq!(
        store.keys().map(SceneId::as_str).collect::<Vec<_>>(),
        ["z", "a"]
    );
    let round_trip = decode_store(encode_store(&store).unwrap()).unwrap();
    let source_order = round_trip[&SceneId::new("z").unwrap()]
        .sources
        .as_ref()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_eq!(source_order, ["backup", "primary"]);
}

#[test]
fn scene_sections_keep_legacy_bounds_without_imposing_live_topology() {
    let value = json!({
        "name": "Boundaries",
        "sources": {"legacy id": {"level": 0, "muted": false}, "missing": {"level": 1}},
        "cells": {"legacy.id.personal": {"volume": 0}, "missing.chat": {"volume": 1, "muted": true}},
        "volumes": {"personal": {"volume": 1}},
        "hardware": {"unknown-model:serial": {"gain_raw": 65535, "monitor_mix": 0, "hp_volume_db": -128, "mute": false, "low_impedance": true}}
    });
    let scene = Scene::from_value(value.clone()).unwrap();
    assert_eq!(
        Scene::from_value(serde_json::to_value(&scene).unwrap()).unwrap(),
        scene
    );
    assert!(
        HardwarePatch::from_value(json!({"gain_raw": 0, "monitor_mix": 65535, "hp_volume_db": 0}))
            .is_ok()
    );
}

#[test]
fn strict_deserialization_rejects_invalid_sections_keys_and_types() {
    let invalid = [
        json!(null),
        json!([]),
        json!({}),
        json!({"name": 42}),
        json!({"name": "Bad", "mixes": {}}),
        json!({"name": "Bad", "sources": null}),
        json!({"name": "Bad", "hardware": []}),
        json!({"name": "Bad", "sources": {"": {}}}),
        json!({"name": "Bad", "sources": {"mic": {"muted": "false"}}}),
        json!({"name": "Bad", "sources": {"mic": {"level": true}}}),
        json!({"name": "Bad", "sources": {"mic": {"level": "0.5"}}}),
        json!({"name": "Bad", "sources": {"mic": {"level": -0.01}}}),
        json!({"name": "Bad", "sources": {"mic": {"level": null}}}),
        json!({"name": "Bad", "sources": {"mic": {"volume": 0.5}}}),
        json!({"name": "Bad", "cells": {"mic.main": []}}),
        json!({"name": "Bad", "cells": {"mic": {}}}),
        json!({"name": "Bad", "cells": {".main": {}}}),
        json!({"name": "Bad", "cells": {"mic.": {}}}),
        json!({"name": "Bad", "cells": {"mic.main": {"volume": 1.01}}}),
        json!({"name": "Bad", "volumes": {"main": {"muted": 0}}}),
        json!({"name": "Bad", "outputs": {"main": 1}}),
        json!({"name": "Bad", "hardware": {"wave_xlr:abc": {"phantom": false}}}),
        json!({"name": "Bad", "hardware": {"wave_xlr:abc": {"gain_raw": 1.0}}}),
        json!({"name": "Bad", "hardware": {"wave_xlr:abc": {"gain_raw": 65536}}}),
        json!({"name": "Bad", "hardware": {"wave_xlr:abc": {"monitor_mix": -1}}}),
        json!({"name": "Bad", "hardware": {"wave_xlr:abc": {"hp_volume_db": -129}}}),
        json!({"name": "Bad", "hardware": {"wave_xlr:abc": {"hp_volume_db": 0.1}}}),
        json!({"name": "Bad", "hardware": {"wave_xlr:abc": {"mute": null}}}),
        json!({"name": "Bad", "hardware": {"wave_xlr:abc": {"low_impedance": 1}}}),
    ];
    for value in invalid {
        assert!(
            Scene::from_value(value.clone()).is_err(),
            "accepted {value}"
        );
        assert!(
            serde_json::from_value::<Scene>(value.clone()).is_err(),
            "serde accepted {value}"
        );
        assert!(decode_store(json!({"scenes": {"invalid": value}})).is_err());
    }
    for value in [
        json!({}),
        json!({"scenes": []}),
        json!({"scenes": {}, "extra": true}),
        json!({"scenes": {"": {"name": "Bad"}}}),
    ] {
        assert!(decode_store(value).is_err());
    }
    assert!(serde_json::from_value::<SourcePatch>(json!({"level": 2})).is_err());
    assert!(serde_json::from_value::<LevelPatch>(json!({"volume": null})).is_err());
    assert!(serde_json::from_value::<HardwarePatch>(json!({"phantom": true})).is_err());
}

#[test]
fn programmatic_nonfinite_edits_cannot_be_persisted_as_json_null() {
    for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let mut scene = Scene::from_value(json!({"name": "Finite", "sources": {"mic": {}}, "cells": {"mic.main": {}}, "hardware": {"wave3:a": {}}})).unwrap();
        scene.sources.as_mut().unwrap()["mic"].level = Some(invalid);
        assert!(scene.validate().is_err());
        let mut store = IndexMap::new();
        store.insert(SceneId::new("finite").unwrap(), scene.clone());
        assert!(encode_store(&store).is_err());
        scene.sources.as_mut().unwrap()["mic"].level = None;
        scene.cells.as_mut().unwrap()["mic.main"].volume = Some(invalid);
        assert!(scene.validate().is_err());
        scene.cells.as_mut().unwrap()["mic.main"].volume = None;
        scene.hardware.as_mut().unwrap()["wave3:a"].hp_volume_db = Some(invalid);
        assert!(scene.validate().is_err());
    }
}

#[test]
fn exact_serial_including_empty_patch_wins_over_legacy() {
    let a = unit(ProfileId::WaveXlr, 1);
    let b = unit(ProfileId::WaveXlr, 2);
    let candidates = [
        HardwareCandidate {
            unit: a,
            serial: "one",
        },
        HardwareCandidate {
            unit: b,
            serial: "two",
        },
    ];
    let entries = hardware(
        json!({"wave_xlr": {"gain_raw": 100}, "wave_xlr:one": {"gain_raw": 200}, "wave_xlr:two": {}}),
    );
    let resolution = resolve_hardware(&entries, &candidates);
    assert_eq!(
        resolution
            .selected
            .iter()
            .map(|entry| (entry.unit, entry.patch.gain_raw))
            .collect::<Vec<_>>(),
        [(a, Some(200)), (b, None)]
    );
    assert_eq!(
        resolution
            .skipped
            .iter()
            .map(|issue| issue.target.as_str())
            .collect::<Vec<_>>(),
        ["hardware wave_xlr"]
    );
}

#[test]
fn missing_and_duplicate_serials_never_borrow_another_target() {
    let a = unit(ProfileId::WaveXlr, 1);
    let b = unit(ProfileId::WaveXlr, 2);
    let entries = hardware(json!({"wave_xlr:old": {"gain_raw": 500}}));
    for serial in ["new", "", "old-suffix"] {
        let candidates = [HardwareCandidate { unit: a, serial }];
        assert!(
            pick_hardware_entry(&entries, a, &candidates)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            resolve_hardware(&entries, &candidates).skipped[0].target,
            "hardware wave_xlr:old"
        );
    }
    let duplicated = [
        HardwareCandidate {
            unit: a,
            serial: "old",
        },
        HardwareCandidate {
            unit: b,
            serial: "old",
        },
    ];
    assert!(pick_hardware_entry(&entries, a, &duplicated).is_err());
    assert!(resolve_hardware(&entries, &duplicated).selected.is_empty());
    assert_eq!(
        resolve_hardware(&entries, &duplicated).skipped[0].target,
        "hardware wave_xlr:old"
    );
    let replacement = UnitId {
        incarnation: 2,
        ..a
    };
    assert!(
        pick_hardware_entry(
            &entries,
            a,
            &[HardwareCandidate {
                unit: replacement,
                serial: "old"
            }]
        )
        .is_err()
    );
    assert!(pick_hardware_entry(&entries, a, &[]).is_err());
}

#[test]
fn legacy_resolution_counts_serialless_units_and_never_crosses_profiles() {
    let a = unit(ProfileId::WaveXlr, 1);
    let b = unit(ProfileId::WaveXlr, 2);
    let other = unit(ProfileId::Wave3, 3);
    let entries = hardware(json!({"wave_xlr": {"gain_raw": 250}}));
    for serial in ["one", ""] {
        let one = [HardwareCandidate { unit: a, serial }];
        assert_eq!(
            pick_hardware_entry(&entries, a, &one)
                .unwrap()
                .unwrap()
                .1
                .gain_raw,
            Some(250)
        );
        let two = [
            one[0],
            HardwareCandidate {
                unit: b,
                serial: "",
            },
        ];
        assert!(resolve_hardware(&entries, &two).selected.is_empty());
        let different_model = [
            one[0],
            HardwareCandidate {
                unit: other,
                serial,
            },
        ];
        assert_eq!(
            resolve_hardware(&entries, &different_model).selected[0].unit,
            a
        );
        assert!(
            pick_hardware_entry(&entries, other, &different_model)
                .unwrap()
                .is_none()
        );
    }
    assert!(resolve_hardware(&entries, &[]).selected.is_empty());
    assert_eq!(
        hardware_key(ProfileId::WaveXlrMk2, "a:b"),
        "wave_xlr_mk2:a:b"
    );
}

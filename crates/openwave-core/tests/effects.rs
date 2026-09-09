use openwave_core::{
    effects::{FxSettings, capture_channels, render_fx_config, toggle_fx},
    model::{Source, SourceId, SourceKind},
};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

fn source(settings: Value) -> Source {
    let mut source = Source::new("Microphone".into(), SourceKind::Device);
    source.id = SourceId::new("mic_1").unwrap();
    source.node_name = "alsa_input.mic".into();
    source.fx = Some(FxSettings::from_value(settings).unwrap());
    source
}

fn args(source: &Source, channels: u32) -> Value {
    let config: Value = serde_json::from_str(
        &render_fx_config(source, channels, "owned-filter")
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    config["context.modules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|module| module["name"] == "libpipewire-module-filter-chain")
        .unwrap()["args"]
        .clone()
}

#[test]
fn hostile_labels_remain_data_and_rename_preserves_node_identity() {
    let hostile = "Mic\"\n} ] context.modules = [{ name = \"evil\" }] #\\";
    let mut record = source(json!({"gate":true}));
    record.name = hostile.into();
    record.node_name = hostile.into();
    let config: Value =
        serde_json::from_str(&render_fx_config(&record, 2, hostile).unwrap().unwrap()).unwrap();
    assert!(
        !config["context.modules"]
            .as_array()
            .unwrap()
            .iter()
            .any(|module| module["name"] == "evil")
    );
    let before = args(&record, 2);
    assert_eq!(before["capture.props"]["target.object"], hostile);
    assert_eq!(
        before["node.description"],
        format!("OpenWave FX: {hostile}")
    );
    let owned = config["context.modules"]
        .as_array()
        .unwrap()
        .last()
        .unwrap();
    assert_eq!(owned["args"]["capture.props"]["openwave.owner"], hostile);
    assert_eq!(owned["args"]["playback.props"]["openwave.owner"], hostile);
    record.name = "Renamed microphone".into();
    let after = args(&record, 2);
    for props in ["capture.props", "playback.props"] {
        assert_eq!(before[props]["node.name"], after[props]["node.name"]);
        assert_eq!(before[props]["media.name"], after[props]["media.name"]);
    }
}

#[test]
fn invalid_settings_fail_both_direct_and_serde_decoding() {
    for bad in [
        json!({"mono":"false"}),
        json!({"gate_thresh":true}),
        json!({"delay_ms":"20"}),
        json!({"lowcut":90}),
        json!({"unknown":1}),
        Value::Null,
        json!([]),
        json!("gate"),
    ] {
        assert!(FxSettings::from_value(bad.clone()).is_err());
        assert!(serde_json::from_value::<FxSettings>(bad).is_err());
    }
    assert!(serde_json::from_str::<FxSettings>("{\"eq_low\":1e999}").is_err());
    for nonfinite in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let mut record = source(json!({}));
        record.fx.as_mut().unwrap().eq_low = nonfinite;
        assert!(render_fx_config(&record, 2, "owner").is_err());
    }
}

#[test]
fn plugin_bounds_are_applied_before_rendering() {
    let record = source(
        json!({"gate":true,"gate_thresh":-1000,"comp":true,"comp_thresh":-1000,"comp_ratio":1000,"eq_low":-1000,"eq_mid":1000,"eq_high":1000,"delay_ms":1000}),
    );
    let graph = args(&record, 2);
    let nodes: HashMap<_, _> = graph["filter.graph"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| (node["name"].as_str().unwrap(), node))
        .collect();
    assert_eq!(nodes["gate_0"]["control"]["Threshold (dB)"], -70.0);
    assert_eq!(nodes["comp_0"]["control"]["Threshold level (dB)"], -30.0);
    assert_eq!(nodes["comp_0"]["control"]["Ratio (1:n)"], 10.0);
    assert_eq!(nodes["eq_low_0"]["control"]["Gain"], -12.0);
    assert_eq!(nodes["eq_mid_0"]["control"]["Gain"], 12.0);
    assert_eq!(nodes["eq_high_0"]["control"]["Gain"], 12.0);
    assert_eq!(nodes["delay_0"]["control"]["Delay (s)"], 0.5);
}

#[test]
fn neutral_or_disabled_controls_bypass_without_mutating_source() {
    for settings in [
        json!({}),
        json!({"gate_thresh":-25,"comp_thresh":-5}),
        json!({"comp":true,"comp_ratio":1}),
        json!({"delay_ms":-10}),
    ] {
        let record = source(settings);
        let original = record.clone();
        assert!(!record.fx.as_ref().unwrap().active());
        assert_eq!(render_fx_config(&record, 2, "owner").unwrap(), None);
        assert_eq!(record, original);
    }
}

#[test]
fn stereo_strips_do_not_cross_or_lose_channels() {
    let record = source(
        json!({"lowcut":80,"gate":true,"comp":true,"eq_low":1,"eq_mid":2,"eq_high":3,"delay_ms":10}),
    );
    let args = args(&record, 2);
    let graph = &args["filter.graph"];
    let nodes: HashMap<_, _> = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| (node["name"].as_str().unwrap(), node))
        .collect();
    let links: HashMap<_, _> = graph["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|link| {
            (
                link["output"].as_str().unwrap(),
                link["input"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(args["capture.props"]["audio.position"], json!(["FL", "FR"]));
    assert_eq!(
        args["playback.props"]["audio.position"],
        json!(["FL", "FR"])
    );
    assert_eq!(graph["inputs"].as_array().unwrap().len(), 2);
    assert_eq!(graph["outputs"].as_array().unwrap().len(), 2);
    let mut visited = HashSet::new();
    for channel in 0..2 {
        let mut port = graph["inputs"][channel].as_str().unwrap();
        let end = graph["outputs"][channel].as_str().unwrap();
        let mut labels = Vec::new();
        loop {
            let (name, input) = port.split_once(':').unwrap();
            assert!(visited.insert(name), "cycle or shared stereo node");
            let node = nodes[name];
            let ladspa = node["type"] == "ladspa";
            assert_eq!(input, if ladspa { "Input" } else { "In" });
            labels.push(node["label"].as_str().unwrap());
            let output = format!("{name}:{}", if ladspa { "Output" } else { "Out" });
            if output == end {
                break;
            }
            port = links[output.as_str()];
        }
        assert_eq!(
            labels,
            [
                "bq_highpass",
                "gate",
                "sc4m",
                "bq_lowshelf",
                "bq_peaking",
                "bq_highshelf",
                "delay"
            ]
        );
    }
    assert_eq!(nodes["gate_0"]["plugin"], "gate_1410");
    assert_eq!(
        nodes["gate_0"]["control"]["Output select (-1 = key listen, 0 = gate, 1 = bypass)"],
        0.0
    );
    assert_eq!(nodes["comp_0"]["plugin"], "sc4m_1916");
}

#[test]
fn only_explicit_mono_averages_stereo_before_processing() {
    let record = source(json!({"mono":true,"comp":true}));
    let result = args(&record, 2);
    let graph = &result["filter.graph"];
    assert_eq!(graph["inputs"], json!(["downmix:In 1", "downmix:In 2"]));
    assert_eq!(
        graph["nodes"][0]["control"],
        json!({"Gain 1":0.5,"Gain 2":0.5})
    );
    assert_eq!(graph["outputs"], json!(["comp_0:Output"]));
    assert!(
        graph["links"]
            .as_array()
            .unwrap()
            .contains(&json!({"output":"downmix:Out","input":"comp_0:Input"}))
    );
    assert_eq!(result["capture.props"]["audio.channels"], 2);
    assert_eq!(result["playback.props"]["audio.position"], json!(["MONO"]));
    let passthrough = args(&source(json!({"mono":true})), 2);
    assert_eq!(
        passthrough["filter.graph"]["outputs"],
        json!(["thru_0:Out"])
    );
}

#[test]
fn observed_channels_override_persisted_metadata_and_targets_fail_closed() {
    let mut record = source(json!({"eq_high":2}));
    record.extra.insert("channels".into(), json!(6));
    let mono = args(&record, 1);
    assert_eq!(mono["capture.props"]["audio.position"], json!(["MONO"]));
    assert_eq!(mono["playback.props"]["audio.position"], json!(["MONO"]));
    assert_eq!(mono["filter.graph"]["inputs"], json!(["eq_high_0:In"]));
    assert_eq!(capture_channels(None).unwrap(), 2);
    for channels in [0, 3, 6] {
        assert!(render_fx_config(&record, channels, "owner").is_err());
    }
    for target in ["0", "-1", "", "raw\0mic", "openwave_fx_mic"] {
        record.node_name = target.into();
        assert!(render_fx_config(&record, 2, "owner").is_err());
    }
}

#[test]
fn effect_toggles_are_atomic_and_device_only() {
    let mut record = source(json!({"lowcut":120}));
    let settings = toggle_fx(&record, "lowcut").unwrap();
    assert_eq!(settings.lowcut, 0);
    let mut settings = settings;
    settings.toggle("lowcut").unwrap();
    assert_eq!(settings.lowcut, 80);
    let before = settings.clone();
    assert!(settings.toggle("gate_thresh").is_err());
    assert_eq!(settings, before);
    record.kind = SourceKind::App;
    assert!(toggle_fx(&record, "gate").is_err());
}

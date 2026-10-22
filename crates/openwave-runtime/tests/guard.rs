use openwave_runtime::guard::{
    SinkEvent, SinkInput, device_key, is_guarded_sink, node_serial, parse_short_sinks,
    parse_sink_inputs, sink_events,
};
use serde_json::json;
use std::collections::HashMap;

const SINK: &str = "alsa_output.usb-Elgato_Systems_Elgato_Wave_XLR_A01DA401224UGC-00.analog-stereo";
const SOURCE: &str =
    "alsa_input.usb-Elgato_Systems_Elgato_Wave_XLR_A01DA401224UGC-00.mono-fallback";

#[test]
fn source_and_sink_of_one_unit_share_a_key_and_serial() {
    assert_eq!(device_key(SINK), device_key(SOURCE));
    assert_eq!(
        device_key(SINK),
        Some("usb-Elgato_Systems_Elgato_Wave_XLR_A01DA401224UGC-00")
    );
    assert_eq!(node_serial(SOURCE), Some("A01DA401224UGC"));
    assert_eq!(device_key("openwave-hold"), None);
}

#[test]
fn only_the_wave_xlr_sink_is_guarded() {
    assert!(is_guarded_sink(SINK));
    assert!(!is_guarded_sink(SOURCE));
    assert!(!is_guarded_sink(
        "alsa_output.usb-Elgato_Systems_Elgato_XLR_Dock_X-00.analog-stereo"
    ));
    assert!(!is_guarded_sink(
        "alsa_output.usb-Elgato_Systems_Elgato_Wave_3_X-00.analog-stereo"
    ));
    assert!(!is_guarded_sink(
        "alsa_output.usb-Elgato_Elgato_Wave_XLR_Dock_MK.2_X-00.analog-stereo"
    ));
    assert!(!is_guarded_sink("alsa_output.pci-0000_01_00.1.hdmi-stereo"));
    assert!(!is_guarded_sink("openwave-hold"));
}

#[test]
fn monitor_batches_report_initial_removed_and_returning_sinks() {
    let mut seen = HashMap::new();
    let sink = json!({"id": 57, "type": "PipeWire:Interface:Node",
        "info": {"props": {"node.name": SINK, "media.class": "Audio/Sink"}}});
    let source = json!({"id": 58, "type": "PipeWire:Interface:Node",
        "info": {"props": {"node.name": SOURCE, "media.class": "Audio/Source"}}});

    let first = sink_events(&json!([sink, source]), &mut seen, true);
    assert_eq!(
        first,
        vec![SinkEvent {
            name: SINK.into(),
            removed: false,
            initial: true
        }]
    );
    assert!(
        sink_events(&json!([sink]), &mut seen, false).is_empty(),
        "updates are not additions"
    );

    let gone = sink_events(
        &json!([{"id": 57, "info": null}, {"id": 99, "info": null}]),
        &mut seen,
        false,
    );
    assert_eq!(
        gone,
        vec![SinkEvent {
            name: SINK.into(),
            removed: true,
            initial: false
        }]
    );

    let back = sink_events(&json!([sink]), &mut seen, false);
    assert_eq!(
        back,
        vec![SinkEvent {
            name: SINK.into(),
            removed: false,
            initial: false
        }]
    );
}

#[test]
fn pactl_output_maps_streams_to_sinks() {
    let sinks = parse_short_sinks(&format!(
        "56\t{SINK}\tPipeWire\ts24le 2ch 48000Hz\tRUNNING\n70\topenwave-hold\tPipeWire\tfloat32le 2ch 48000Hz\tIDLE\n"
    ));
    assert_eq!(sinks.get(SINK), Some(&56));
    assert_eq!(sinks.get("openwave-hold"), Some(&70));

    let inputs = parse_sink_inputs(
        r#"[{"index":101,"sink":56,"properties":{"object.id":"212","application.name":"Firefox"}},
            {"index":102,"sink":70,"properties":{}},
            {"index":103,"sink":56}]"#,
    );
    assert_eq!(
        inputs,
        vec![
            SinkInput {
                index: 101,
                sink: 56,
                object_id: Some(212)
            },
            SinkInput {
                index: 102,
                sink: 70,
                object_id: None
            },
            SinkInput {
                index: 103,
                sink: 56,
                object_id: None
            },
        ]
    );
    assert!(parse_sink_inputs("not json").is_empty());
}

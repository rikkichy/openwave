use openwave_runtime::audio::{PinAction, PinStatus, PinWatch, aggregate_status, capture_nodes};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
fn at(start: Instant, seconds: u64) -> Instant {
    start + Duration::from_secs(seconds)
}

#[test]
fn zero_only_restarts_and_mute_cannot_rearm_silence_allowance() {
    let now = Instant::now();
    let mut watch = PinWatch::new(now);
    watch.bytes(at(now, 30), false);
    assert_eq!(
        watch.step(at(now, 30), true, true, Some(false)),
        PinAction::Restart
    );
    watch.restarted(at(now, 31));
    assert_eq!(
        watch.step(at(now, 31), true, true, Some(false)),
        PinAction::Keep
    );
    watch.bytes(at(now, 61), false);
    assert_eq!(
        watch.step(at(now, 61), true, true, Some(false)),
        PinAction::Keep
    );
    assert_eq!(watch.status(), PinStatus::Silent);
    watch.bytes(at(now, 62), false);
    assert_eq!(
        watch.step(at(now, 62), true, true, Some(true)),
        PinAction::Keep
    );
    watch.bytes(at(now, 100), false);
    assert_eq!(
        watch.step(at(now, 100), true, true, Some(false)),
        PinAction::Keep
    );
    assert_eq!(watch.status(), PinStatus::Silent);
    watch.bytes(at(now, 101), true);
    watch.bytes(at(now, 131), false);
    assert_eq!(
        watch.step(at(now, 131), true, true, Some(false)),
        PinAction::Restart
    );
}
#[test]
fn unknown_graph_or_mute_never_authorizes_recycle() {
    let now = Instant::now();
    let mut watch = PinWatch::new(now);
    assert_eq!(
        watch.step(at(now, 10), false, false, Some(false)),
        PinAction::Keep
    );
    assert_eq!(watch.step(at(now, 20), false, true, None), PinAction::Keep);
    assert_eq!(watch.status(), PinStatus::Unknown);
    assert_eq!(
        watch.step(at(now, 20), false, true, Some(false)),
        PinAction::Restart
    );
}
#[test]
fn zero_bytes_are_flow_but_unstarted_capture_is_not_healthy() {
    let now = Instant::now();
    let mut watch = PinWatch::new(now);
    assert_eq!(watch.step(now, true, true, Some(false)), PinAction::Keep);
    assert_eq!(watch.status(), PinStatus::Starting);
    watch.bytes(at(now, 2), false);
    assert_eq!(
        watch.step(at(now, 2), true, true, Some(false)),
        PinAction::Keep
    );
    assert_eq!(watch.status(), PinStatus::Healthy);
    assert_eq!(
        watch.step(at(now, 5), true, true, Some(false)),
        PinAction::Restart
    );
    assert_eq!(watch.status(), PinStatus::Wedged);
}
#[test]
fn restarting_one_pin_does_not_reset_other_pin_or_hide_its_fault() {
    let now = Instant::now();
    let mut first = PinWatch::new(now);
    let mut second = PinWatch::new(now);
    first.bytes(at(now, 30), false);
    assert_eq!(
        first.step(at(now, 30), true, true, Some(false)),
        PinAction::Restart
    );
    first.restarted(at(now, 31));
    second.bytes(at(now, 31), false);
    assert_eq!(
        second.step(at(now, 31), true, true, Some(false)),
        PinAction::Restart
    );
    assert_eq!(
        aggregate_status([first.status(), second.status()]),
        PinStatus::Silent
    );
    assert_eq!(
        aggregate_status([PinStatus::Healthy, PinStatus::Wedged]),
        PinStatus::Wedged
    );
    assert_eq!(aggregate_status([]), PinStatus::Absent);
}
fn node(id: u32, name: &str, serial: u32) -> Value {
    json!({"id":id,"type":"PipeWire:Interface:Node","info":{"state":"running","props":{"node.name":name,"media.class":"Audio/Source","object.serial":serial,"device.id":50}}})
}
fn graph() -> Value {
    json!([
        {"id":0,"type":"PipeWire:Interface:Core","info":{"cookie":123}},
        node(1,"alsa_input.usb-Elgato_Systems_Elgato_Wave_XLR_ABC-00.mono",101),
        node(2,"alsa_input.usb-Elgato_Systems_Elgato_XLR_Dock_DEF-00.mono",102),
        node(3,"alsa_input.usb-Elgato_Systems_Game_Capture_HD60-00.mono",103),
        {"id":50,"type":"PipeWire:Interface:Device","info":{"params":{"Route":[{"direction":"Input","props":{"mute":false}}]}}}
    ])
}
#[test]
fn exact_wave_stems_and_generation_identity_drive_discovery() {
    let mut value = graph();
    let original = capture_nodes(&value).unwrap();
    assert_eq!(
        original
            .iter()
            .map(|node| node.identity.object_serial.as_str())
            .collect::<Vec<_>>(),
        ["101", "102"]
    );
    value[1]["info"]["props"]["object.serial"] = json!(201);
    let replaced = capture_nodes(&value).unwrap();
    assert_ne!(original[0].identity, replaced[0].identity);
    assert_eq!(original[1].identity, replaced[1].identity);
    value[0]["info"]["cookie"] = json!(456);
    assert_ne!(
        replaced[1].identity,
        capture_nodes(&value).unwrap()[1].identity
    );
}
#[test]
fn incomplete_graph_and_ambiguous_mute_fail_closed() {
    let mut value = graph();
    value[4]["info"]["params"]["Route"][0]["props"]["mute"] = json!("false");
    assert!(
        capture_nodes(&value)
            .unwrap()
            .iter()
            .all(|node| node.muted.is_none())
    );
    value[0]["info"]["cookie"] = Value::Null;
    assert!(capture_nodes(&value).is_err());
    let mut duplicate = graph();
    let copy = duplicate[1].clone();
    duplicate.as_array_mut().unwrap().push(copy);
    assert!(capture_nodes(&duplicate).is_err());
    let mut missing_serial = graph();
    missing_serial[1]["info"]["props"]
        .as_object_mut()
        .unwrap()
        .remove("object.serial");
    assert!(
        capture_nodes(&missing_serial).is_err(),
        "node ID is not pw-cat serial authority"
    );
}

#[test]
fn daemon_informational_paths_create_no_runtime_or_audio_state() {
    let home = tempfile::tempdir().unwrap();
    for flag in ["--help", "--version"] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_openwave-daemon"))
            .arg(flag)
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path().join("config"))
            .env("XDG_DATA_HOME", home.path().join("data"))
            .env("XDG_STATE_HOME", home.path().join("state"))
            .env("XDG_RUNTIME_DIR", home.path().join("runtime"))
            .env("PATH", "")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);
    }
}

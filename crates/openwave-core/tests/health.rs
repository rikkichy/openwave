use openwave_core::{
    health::*,
    model::{NodeIdentity, Observation, OperationError},
};
use serde_json::{Value, json};
use std::time::Duration;

fn t(seconds: u64) -> Duration {
    Duration::from_secs(seconds)
}
fn id(serial: &str) -> NodeIdentity {
    NodeIdentity {
        server_cookie: 42,
        object_serial: serial.into(),
    }
}
fn unknown<T>() -> Observation<T> {
    Observation::Unknown(OperationError::unavailable("collector unavailable"))
}
fn capture(serial: &str, count: u64, age: u64) -> Observation<CaptureSample> {
    Observation::Known(CaptureSample {
        identity: id(serial),
        running: true,
        muted: Observation::Known(false),
        xruns: Observation::Known(count),
        byte_age: Observation::Known(t(age)),
    })
}
fn sink(serial: &str, pointer: u64, state: PlaybackState) -> Observation<SinkSample> {
    Observation::Known(SinkSample {
        identity: id(serial),
        running: true,
        muted: Observation::Known(false),
        playback: Observation::Known(PlaybackStatus {
            hw_ptr: pointer,
            state,
        }),
    })
}

#[test]
fn sustained_xruns_require_two_windows_and_one_announcement() {
    let mut watch = GlitchWatch::default();
    assert!(!watch.observe("mic", &id("one"), 61994));
    assert_eq!(watch.last_delta("mic"), None);
    assert!(watch.observe("mic", &id("one"), 62044));
    assert!(!watch.glitching("mic"));
    assert!(watch.observe("mic", &id("one"), 62094));
    assert!(watch.glitching("mic") && watch.just_confirmed("mic"));
    watch.observe("mic", &id("one"), 62144);
    assert!(watch.glitching("mic") && !watch.just_confirmed("mic"));
    watch.observe("mic", &id("one"), 62167);
    assert!(!watch.glitching("mic"));
    watch.observe("mic", &id("one"), 62217);
    watch.observe("mic", &id("one"), 62218);
    assert!(!watch.glitching("mic"));
    watch.observe("mic", &id("two"), 90000);
    assert_eq!(watch.last_delta("mic"), None);
    watch.observe("mic", &id("two"), 0);
    assert_eq!(watch.last_delta("mic"), None);
    watch.pause("mic");
    watch.observe("mic", &id("two"), 200);
    assert!(!watch.glitching("mic"));
}

#[test]
fn no_data_and_xruns_spend_one_capped_cooled_budget() {
    let mut watch = CaptureWatch::default();
    assert!(
        watch
            .observe("mic", &capture("one", 0, 8), t(0))
            .recovery_due
    );
    watch.record_attempt("mic", t(0));
    assert!(!watch.observe("mic", &capture("one", 0, 0), t(10)).healthy);
    watch.observe("mic", &capture("one", 230, 0), t(20));
    let fault = watch.observe("mic", &capture("one", 460, 0), t(30));
    assert!(fault.glitching && !fault.recovery_due);
    assert!(
        watch
            .observe("mic", &capture("one", 690, 0), t(60))
            .recovery_due
    );
    watch.record_attempt("mic", t(60));
    assert!(
        !watch
            .observe("mic", &capture("one", 690, 100), t(300))
            .recovery_due
    );
    assert!(
        watch
            .observe("other", &capture("other", 0, 100), t(300))
            .recovery_due
    );
}

#[test]
fn unknown_muted_absent_and_recreated_capture_never_refill() {
    let mut watch = CaptureWatch::default();
    watch.record_attempt("mic", t(0));
    watch.record_attempt("mic", t(60));
    watch.observe("mic", &capture("one", 0, 0), t(100));
    watch.observe("mic", &capture("one", 0, 0), t(110));
    watch.observe("mic", &unknown(), t(400));
    watch.observe("mic", &capture("one", 0, 0), t(500));
    watch.observe("mic", &capture("one", 0, 0), t(510));
    let mut muted = capture("one", 0, 0);
    if let Observation::Known(sample) = &mut muted {
        sample.muted = Observation::Known(true);
    }
    watch.observe("mic", &muted, t(800));
    watch.observe("mic", &capture("one", 0, 0), t(900));
    watch.observe("mic", &capture("one", 0, 0), t(910));
    watch.observe("mic", &capture("two", 0, 0), t(1210));
    assert_eq!(watch.budget.spent("mic"), 2);
    assert!(
        !watch
            .observe("mic", &capture("two", 0, 0), t(1500))
            .recovery_due
    );
    assert!(
        !watch
            .observe("mic", &capture("two", 0, 9), t(1799))
            .recovery_due
    );
    watch.observe("mic", &capture("two", 0, 0), t(1800));
    watch.observe("mic", &capture("two", 0, 0), t(2100));
    assert!(
        watch
            .observe("mic", &capture("two", 0, 9), t(2101))
            .recovery_due
    );
}

#[test]
fn capture_counter_reset_and_missing_bytes_break_healthy_interval() {
    let mut watch = CaptureWatch::default();
    watch.record_attempt("mic", t(0));
    watch.record_attempt("mic", t(60));
    watch.observe("mic", &capture("one", 100, 0), t(100));
    watch.observe("mic", &capture("one", 100, 0), t(110));
    watch.observe("mic", &capture("one", 0, 0), t(410));
    assert_eq!(watch.budget.spent("mic"), 2);
    watch.observe("mic", &capture("one", 1, 0), t(500));
    let mut missing_bytes = capture("one", 1, 0);
    if let Observation::Known(sample) = &mut missing_bytes {
        sample.byte_age = unknown();
    }
    watch.observe("mic", &missing_bytes, t(800));
    watch.observe("mic", &capture("one", 1, 0), t(900));
    assert_eq!(watch.budget.spent("mic"), 2);
    watch.observe("mic", &capture("one", 1, 0), t(1200));
    assert_eq!(watch.budget.spent("mic"), 0);
}

#[test]
fn sink_stall_uses_running_pointer_and_xrun_not_audio_amplitude() {
    let mut watch = SinkStallWatch::default();
    assert!(!watch.observe("sink", &sink("one", 1000, PlaybackState::Running)));
    assert!(watch.observe("sink", &sink("one", 1000, PlaybackState::Running)));
    assert!(watch.just_stalled("sink") && watch.should_recover("sink", t(10)));
    watch.observe("sink", &sink("one", 1000, PlaybackState::Running));
    assert!(!watch.just_stalled("sink"));
    watch.record_attempt("sink", t(10));
    assert!(!watch.observe("sink", &sink("one", 0, PlaybackState::Running)));
    watch.observe("sink", &sink("one", 0, PlaybackState::Running));
    assert!(!watch.should_recover("sink", t(69)));
    assert!(watch.should_recover("sink", t(70)));
    watch.record_attempt("sink", t(70));
    assert!(watch.observe("sink", &sink("one", 0, PlaybackState::Xrun)));
    assert!(!watch.should_recover("sink", t(300)));
    assert!(watch.observe("other", &sink("other", 0, PlaybackState::Xrun)));
    assert!(watch.should_recover("other", t(300)));
    assert!(!watch.observe("sink", &unknown()));
    assert!(!watch.observe("sink", &sink("one", 1000, PlaybackState::Other)));
    let mut muted = sink("one", 1000, PlaybackState::Xrun);
    if let Observation::Known(sample) = &mut muted {
        sample.muted = Observation::Known(true);
    }
    assert!(!watch.observe("sink", &muted));
}

#[test]
fn only_six_advances_refill_output_not_reset_recreation_or_unknown() {
    let mut watch = SinkStallWatch::default();
    watch.record_attempt("sink", t(0));
    watch.record_attempt("sink", t(60));
    for pointer in 0..=5 {
        watch.observe("sink", &sink("one", pointer, PlaybackState::Running));
    }
    assert_eq!(watch.spent("sink"), 2);
    watch.observe("sink", &sink("one", 0, PlaybackState::Running));
    assert_eq!(watch.spent("sink"), 2);
    for pointer in 1..=5 {
        watch.observe("sink", &sink("one", pointer, PlaybackState::Running));
    }
    watch.observe("sink", &sink("two", 100, PlaybackState::Running));
    assert_eq!(watch.spent("sink"), 2);
    for pointer in 101..=105 {
        watch.observe("sink", &sink("two", pointer, PlaybackState::Running));
    }
    watch.observe("sink", &unknown());
    for pointer in 106..=111 {
        watch.observe("sink", &sink("two", pointer, PlaybackState::Running));
    }
    assert_eq!(watch.spent("sink"), 2);
    watch.observe("sink", &sink("two", 112, PlaybackState::Running));
    assert_eq!(watch.spent("sink"), 0);
    watch.observe("sink", &sink("two", 112, PlaybackState::Running));
    assert!(watch.should_recover("sink", t(1000)));
}

#[test]
fn failed_collection_and_valid_omission_pause_both_budgets() {
    let mut watch = HealthWatch::default();
    watch.capture.record_attempt("mic", t(0));
    watch.capture.record_attempt("mic", t(60));
    watch.sink.record_attempt("sink", t(0));
    watch.sink.record_attempt("sink", t(60));
    let frame = |pointer| {
        Observation::Known(HealthSamples {
            captures: [("mic".into(), capture("one", 0, 0))].into_iter().collect(),
            sinks: [("sink".into(), sink("one", pointer, PlaybackState::Running))]
                .into_iter()
                .collect(),
        })
    };
    watch.observe(&frame(0), t(100));
    watch.observe(&frame(1), t(110));
    assert!(watch.observe(&unknown(), t(500)).error.is_some());
    watch.observe(&frame(2), t(600));
    watch.observe(&frame(3), t(610));
    watch.observe(&Observation::Known(HealthSamples::default()), t(1000));
    watch.observe(&frame(4), t(1100));
    assert_eq!(watch.capture.budget.spent("mic"), 2);
    assert_eq!(watch.sink.spent("sink"), 2);
}

#[test]
fn capture_stall_boundary_absence_and_cooldown_are_conservative() {
    let mut watch = StallWatch::default();
    assert!(!watch.should_recover("mic", false, Some(t(999)), t(100)));
    assert!(!watch.should_recover("mic", true, None, t(100)));
    assert!(!watch.should_recover("mic", true, Some(t(7)), t(100)));
    assert!(watch.should_recover("mic", true, Some(t(8)), t(100)));
    watch.record_attempt("mic", t(100));
    assert!(!watch.can_recover("mic", t(99)));
    assert!(!watch.can_recover("mic", t(159)));
    assert!(watch.can_recover("mic", t(160)));
}

#[test]
fn collector_parsers_keep_last_iteration_and_reject_unknown_values() {
    let text = "S ID QUANT RATE WAIT BUSY W/Q B/Q ERR FORMAT NAME\nC 73 0 0 --- --- --- --- 0 dock\nR 73 0 0 0us 0us ??? ??? 30367 S24LE 1 48000 + dock\nR 199 0 0 0us 0us 0 0 5 F32P 1 0 + fx\nR 200 0 0 0us 0us 0 0 9 fx\n";
    let counts = parse_pw_top(text);
    assert_eq!(counts[&73].count, 30367);
    assert_eq!(counts[&199].count, 5);
    assert_eq!(counts[&200].count, 9);
    assert_eq!(counts.len(), 3);
    assert_eq!(
        parse_playback_status("state: RUNNING\nhw_ptr : 49000\n").unwrap(),
        PlaybackStatus {
            state: PlaybackState::Running,
            hw_ptr: 49000
        }
    );
    assert!(parse_playback_status("state: RUNNING\nhw_ptr: unknown").is_err());
    assert!(parse_mutes(&json!([{"name":"mic", "mute":null}])).is_err());
    assert!(
        parse_mutes(&json!([{"name":"mic", "mute":false}, {"name":"mic", "mute":true}])).is_err()
    );
    assert_eq!(
        parse_mutes(&json!([{"name":"mic", "mute":false}])).unwrap()["mic"],
        false
    );
}

fn graph_node(id: u32, name: &str, class: &str) -> Value {
    json!({"id":id,"type":"PipeWire:Interface:Node","info":{"state":"running", "props":{"node.name":name,"media.class":class,"object.serial":id+100,"api.alsa.pcm.card":0,"api.alsa.pcm.device":1}}})
}
fn graph_link(source: u32, target: u32) -> Value {
    json!({"type":"PipeWire:Interface:Link","info":{"output-node-id":source,"input-node-id":target}})
}
#[test]
fn output_watch_follows_actual_links_and_rejects_malformed_graph() {
    let mut route = graph_node(5, "openwave_loop_output_personal", "Stream/Output/Audio");
    route["info"]["props"]["target.object"] = json!("alsa_output.unrelated");
    let mut graph = json!([
        {"id":0,"type":"PipeWire:Interface:Core","info":{"cookie":42}},
        graph_node(1,"alsa_input.usb-Elgato_Systems_Elgato_XLR_Dock_SERIAL-00.mono-fallback","Audio/Source"),
        graph_node(2,"alsa_output.linked","Audio/Sink"),
        graph_node(3,"alsa_output.unrelated","Audio/Sink"),
        graph_node(4,"openwave_loop_output_personal_cap","Stream/Input/Audio"),
        route, graph_node(6,"unrelated_player","Stream/Output/Audio"),
        graph_link(5,2), graph_link(4,3), graph_link(6,3)
    ]);
    let parsed = parse_health_graph(&graph).unwrap();
    assert_eq!(
        parsed.sinks.keys().collect::<Vec<_>>(),
        [&"alsa_output.linked".to_string()]
    );
    assert_eq!(
        parsed.sinks["alsa_output.linked"].playback,
        AlsaPlayback {
            card: 0,
            device: 1,
            subdevice: 0
        }
    );
    assert_eq!(parsed.captures.values().next().unwrap().identity, id("101"));
    graph[2]["info"]["props"] = json!([]);
    assert!(parse_health_graph(&graph).is_err());
    graph[2] = graph_node(2, "alsa_output.linked", "Audio/Sink");
    graph[0]["info"]["cookie"] = Value::Null;
    assert!(parse_health_graph(&graph).is_err());
}

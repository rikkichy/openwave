use indexmap::IndexMap;
use openwave_core::{
    model::*,
    profiles::ProfileId,
    protocol::{ConfigBuffer, DeviceInfo, DeviceState},
    scenes::SceneId,
};
use openwave_runtime::{
    controller::{
        AppCommand, Backend, BackendCommand, BackendEvent, EditTiming, RuntimeEvent, RuntimeHandle,
        ShutdownError,
    },
    mixer::MixerObservation,
    uninstall::{UninstallPlan, UninstallResult},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    future::Future,
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    task::{Context, Poll, Wake, Waker},
    thread,
    time::{Duration, Instant},
};

// This backend belongs only to the fixture; controller ownership and all stores
// are real, and no production audio, device, service or setup worker is started.
struct FixtureBackend {
    root: PathBuf,
    incoming: mpsc::Receiver<BackendEvent>,
    outgoing: mpsc::Sender<BackendCommand>,
}
impl Backend for FixtureBackend {
    fn identity(&self) -> &Path {
        &self.root
    }
    fn dispatch(&mut self, command: BackendCommand) -> Result<()> {
        self.outgoing
            .send(command)
            .map_err(|_| OperationError::unavailable("fixture receiver closed"))
    }
    fn next_event(&mut self) -> Option<BackendEvent> {
        self.incoming.try_recv().ok()
    }
    fn shutdown(&mut self) -> std::result::Result<(), ShutdownError> {
        Ok(())
    }
    fn prepare_removal(&mut self, _: &UninstallPlan, _: bool) -> Result<()> {
        Err(OperationError::unavailable("fixture forbids removal"))
    }
    fn remove(&mut self, _: &UninstallPlan, _: bool) -> Result<UninstallResult> {
        Err(OperationError::unavailable("fixture forbids removal"))
    }
}
struct ThreadWake(thread::Thread);
impl Wake for ThreadWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}
fn block_on<T>(future: impl Future<Output = T>) -> T {
    let waker = Waker::from(Arc::new(ThreadWake(thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => thread::park(),
        }
    }
}
struct Fixture {
    root: tempfile::TempDir,
    handle: RuntimeHandle,
    incoming: mpsc::Sender<BackendEvent>,
    outgoing: mpsc::Receiver<BackendCommand>,
    completed: mpsc::Receiver<(CommandId, CommandOutcome)>,
    outcomes: HashMap<CommandId, CommandOutcome>,
    pump: Option<thread::JoinHandle<()>>,
}
impl Fixture {
    fn new(scene: Value, corrupt: Option<(&str, &str)>) -> Self {
        let root = tempfile::tempdir().unwrap();
        let sources = json!({
            "a":{"kind":"device","name":"Mic A","node_name":"mic_a","level":0.7,"muted":true,"group":"Mics","channels":1,"custom":"retain"},
            "b":{"kind":"device","name":"Mic B","node_name":"mic_b","level":0.8,"muted":false,"group":"Mics"}
        });
        std::fs::write(
            root.path().join("sources.json"),
            serde_json::to_vec(&sources).unwrap(),
        )
        .unwrap();
        std::fs::write(root.path().join("mixes.json"), serde_json::to_vec(&json!({
            "a.personal":{"volume":0.8,"muted":true,"custom":42},"marker":"retain",
            "outputs":{"personal":"none"},"volumes":{"personal":{"volume":0.6,"muted":false,"custom":43}}
        })).unwrap()).unwrap();
        std::fs::write(
            root.path().join("scenes.json"),
            serde_json::to_vec(&json!({"scenes":{"recall":scene}})).unwrap(),
        )
        .unwrap();
        if let Some((file, contents)) = corrupt {
            std::fs::write(root.path().join(file), contents).unwrap();
        }
        let (incoming, receiver) = mpsc::channel();
        let (sender, outgoing) = mpsc::channel();
        let path = root.path().to_owned();
        let (handle, events) = RuntimeHandle::start_with(path.clone(), move || {
            Ok(Box::new(FixtureBackend {
                root: path,
                incoming: receiver,
                outgoing: sender,
            }))
        })
        .unwrap();
        let (completion_send, completed) = mpsc::channel();
        let pump = thread::spawn(move || {
            while let Ok(event) = block_on(events.recv()) {
                if let RuntimeEvent::CommandFinished { id, result } = event {
                    if completion_send.send((id, result)).is_err() {
                        break;
                    }
                }
            }
        });
        let fixture = Self {
            root,
            handle,
            incoming,
            outgoing,
            completed,
            outcomes: HashMap::new(),
            pump: Some(pump),
        };
        fixture.wait_snapshot(|snapshot| snapshot.revision >= 1);
        fixture
    }
    fn wait_snapshot(&self, predicate: impl Fn(&AppSnapshot) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !predicate(&self.handle.snapshot()) {
            assert!(Instant::now() < deadline, "snapshot deadline");
            thread::sleep(Duration::from_millis(2));
        }
    }
    fn connect(&self, units: &[UnitSnapshot], captures: bool) {
        for unit in units {
            self.incoming
                .send(BackendEvent::Unit(unit.clone()))
                .unwrap();
        }
        if captures {
            let captures = units
                .iter()
                .enumerate()
                .map(|(index, unit)| CaptureSnapshot {
                    identity: NodeIdentity {
                        server_cookie: 1,
                        object_serial: format!("capture-{index}"),
                    },
                    node_id: 10 + index as u32,
                    node_name: format!("mic_{}", if index == 0 { "a" } else { "b" }),
                    name: format!("Microphone {index}"),
                    muted: Observation::Unknown(OperationError::unavailable(
                        "fixture mute not observed",
                    )),
                    channels: Some(1),
                    properties: json!({"device.serial":unit.info.serial})
                        .as_object()
                        .unwrap()
                        .clone(),
                })
                .collect();
            self.incoming
                .send(BackendEvent::Graph(MixerObservation {
                    observation: Observation::Known(()),
                    captures,
                    streams: vec![],
                    outputs: vec![],
                    default_sink: None,
                    meter_targets: vec![],
                    mix_identities: IndexMap::new(),
                    silent_sources: Default::default(),
                    errors: vec![],
                    revision: self.handle.snapshot().revision,
                }))
                .unwrap();
        }
        self.incoming
            .send(BackendEvent::Status {
                service: "connected".into(),
                setup_required: false,
            })
            .unwrap();
        self.wait_snapshot(|snapshot| {
            snapshot.service_status == "connected" && snapshot.units.len() == units.len()
        });
    }
    fn submit(&self, command: AppCommand) -> CommandId {
        self.handle.submit(command).unwrap()
    }
    fn outcome(&mut self, id: CommandId) -> CommandOutcome {
        if let Some(outcome) = self.outcomes.remove(&id) {
            return outcome;
        }
        loop {
            let (received, outcome) = self
                .completed
                .recv_timeout(Duration::from_secs(5))
                .expect("command completion deadline");
            if received == id {
                return outcome;
            }
            assert!(
                self.outcomes.insert(received, outcome).is_none(),
                "duplicate completion"
            );
        }
    }
    fn apply(&self) -> CommandId {
        self.submit(AppCommand::ApplyScene {
            scene: SceneId::new("recall").unwrap(),
        })
    }
    fn job(&self) -> (u64, UnitId, Vec<DeviceSetting>) {
        loop {
            if let BackendCommand::Device {
                job,
                unit,
                settings,
            } = self
                .outgoing
                .recv_timeout(Duration::from_secs(5))
                .expect("device admission deadline")
            {
                return (job, unit, settings);
            }
        }
    }
    fn finish_job(&self, job: &(u64, UnitId, Vec<DeviceSetting>), failed: bool) {
        let result = if failed {
            Err(OperationError::unavailable("fixture USB failure"))
        } else {
            let mut state = device_state(job.1.profile);
            for setting in &job.2 {
                match setting {
                    DeviceSetting::GainRaw(value) => state.gain_raw = *value,
                    DeviceSetting::Mute(value) => state.muted = *value,
                    DeviceSetting::HeadphoneDb(value) => state.hp_volume_db = *value,
                    DeviceSetting::LowImpedance(value) => state.low_impedance = Some(*value),
                    DeviceSetting::MonitorMix(value) => state.monitor_mix = Some(*value),
                    DeviceSetting::Phantom(_) => panic!("scene submitted phantom"),
                }
            }
            Ok(state)
        };
        self.incoming
            .send(BackendEvent::DeviceFinished {
                job: job.0,
                unit: job.1,
                result,
            })
            .unwrap();
    }
    fn read(&self, file: &str) -> Value {
        serde_json::from_slice(&std::fs::read(self.root.path().join(file)).unwrap()).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.handle.submit(AppCommand::Shutdown);
        let _ = self.handle.wait_stopped();
        if let Some(pump) = self.pump.take() {
            let _ = pump.join();
        }
    }
}
fn device_state(profile: ProfileId) -> DeviceState {
    ConfigBuffer::decode(profile, &vec![0; profile.profile().config_len])
        .unwrap()
        .state()
}
fn unit(profile: ProfileId, address: u8, serial: &str, muted: bool) -> UnitSnapshot {
    let mut state = device_state(profile);
    state.muted = muted;
    state.gain_raw = 1280;
    state.hp_volume_db = -12.0;
    UnitSnapshot {
        id: UnitId {
            profile,
            bus: 1,
            address,
            incarnation: 1,
        },
        info: DeviceInfo {
            api: "1.0".into(),
            firmware: "1.2.3".into(),
            serial: serial.into(),
        },
        state: Observation::Known(state),
        desired_mute: None,
        input_peak: 0.0,
        output_peak: 0.0,
        errors: vec![],
    }
}
fn scene_result(outcome: CommandOutcome) -> (Vec<OperationIssue>, Vec<OperationIssue>) {
    match outcome {
        CommandOutcome::SceneFinished {
            skipped, failed, ..
        } => (skipped, failed),
        other => panic!("expected scene result, got {other:?}"),
    }
}

#[test]
fn scene_waits_for_every_unit_and_reports_eventual_failure_without_early_mute_batches() {
    let mut f = Fixture::new(
        json!({"name":"Recall","sources":{"a":{"muted":false},"b":{"muted":false}},"hardware":{
            "wave_xlr:A":{"gain_raw":2560,"mute":false,"hp_volume_db":-20.0,"low_impedance":true},
            "wave3:B":{"gain_raw":1280,"mute":true,"hp_volume_db":-10.0,"monitor_mix":6400}
        }}),
        None,
    );
    let a = unit(ProfileId::WaveXlr, 1, "A", true);
    let b = unit(ProfileId::Wave3, 2, "B", false);
    f.connect(&[a.clone(), b.clone()], true);
    let id = f.apply();
    let first = f.job();
    let second = f.job();
    assert_eq!(first.1, a.id);
    assert_eq!(second.1, b.id);
    assert_eq!(
        first.2,
        vec![
            DeviceSetting::GainRaw(2560),
            DeviceSetting::Mute(true),
            DeviceSetting::HeadphoneDb(-20.0),
            DeviceSetting::LowImpedance(true)
        ]
    );
    assert_eq!(
        second.2,
        vec![
            DeviceSetting::GainRaw(1280),
            DeviceSetting::Mute(false),
            DeviceSetting::HeadphoneDb(-10.0),
            DeviceSetting::MonitorMix(6400)
        ]
    );
    f.finish_job(&second, true);
    // A status publication is a barrier after the failed completion; the other
    // admitted job is deliberately still running.
    f.incoming
        .send(BackendEvent::Status {
            service: "one-result".into(),
            setup_required: false,
        })
        .unwrap();
    f.wait_snapshot(|s| s.service_status == "one-result");
    assert!(f.handle.snapshot().scene_outcome.is_none());
    assert!(f.completed.try_recv().is_err());
    f.finish_job(&first, false);
    let (skipped, failed) = scene_result(f.outcome(id));
    assert!(skipped.iter().any(|issue| issue.target == "source a"));
    assert_eq!(failed.len(), 1);
    assert!(failed[0].message.contains("fixture USB failure"));
    let snapshot = f.handle.snapshot();
    assert!(snapshot.desired.sources[&SourceId::new("a").unwrap()].muted);
    assert!(!snapshot.desired.sources[&SourceId::new("b").unwrap()].muted);
    assert!(
        !f.outgoing
            .try_iter()
            .any(|command| matches!(command, BackendCommand::Device { .. })),
        "one ordered batch per explicit unit"
    );
}

#[test]
fn explicit_source_mute_overrides_hardware_and_hardware_only_open_mutes_group_peer() {
    let mut f = Fixture::new(
        json!({"name":"Recall","sources":{"b":{"muted":true}},"hardware":{
            "wave_xlr:A":{"mute":false},"wave3:B":{"mute":false}
        }}),
        None,
    );
    let a = unit(ProfileId::WaveXlr, 1, "A", true);
    let b = unit(ProfileId::Wave3, 2, "B", false);
    f.connect(&[a.clone(), b.clone()], true);
    let id = f.apply();
    let first = f.job();
    assert_eq!(first.1, b.id);
    assert_eq!(first.2, vec![DeviceSetting::Mute(true)]);
    f.finish_job(&first, false);
    let second = f.job();
    assert_eq!(second.1, a.id);
    assert_eq!(second.2, vec![DeviceSetting::Mute(false)]);
    f.finish_job(&second, false);
    let (_, failed) = scene_result(f.outcome(id));
    assert!(failed.is_empty());
    let persisted = f.read("sources.json");
    assert_eq!(persisted["a"]["muted"], false);
    assert_eq!(persisted["b"]["muted"], true);
    assert_eq!(persisted["a"]["custom"], "retain");
}

#[test]
fn exact_matching_selected_gain_lock_and_profile_limits_do_not_substitute_or_write_phantom() {
    let mut f = Fixture::new(
        json!({"name":"Recall","hardware":{
            "wave_xlr":{"gain_raw":256},"wave_xlr:A":{"gain_raw":512,"low_impedance":true},
            "wave_xlr:B":{"gain_raw":768},"wave_xlr:missing":{"mute":true},
            "wave3:C":{"gain_raw":65535,"low_impedance":true,"monitor_mix":65535,"hp_volume_db":-15.0}
        }}),
        None,
    );
    let a = unit(ProfileId::WaveXlr, 1, "A", true);
    let b = unit(ProfileId::WaveXlr, 2, "B", false);
    let c = unit(ProfileId::Wave3, 3, "C", false);
    f.connect(&[a.clone(), b.clone(), c.clone()], false);
    let lock = f.submit(AppCommand::SetGainLock { locked: true });
    assert!(matches!(f.outcome(lock), CommandOutcome::Applied { .. }));
    let id = f.apply();
    let jobs = [f.job(), f.job(), f.job()];
    assert_eq!(jobs[0].1, a.id);
    assert_eq!(jobs[0].2, vec![DeviceSetting::LowImpedance(true)]);
    assert_eq!(jobs[1].1, b.id);
    assert_eq!(jobs[1].2, vec![DeviceSetting::GainRaw(768)]);
    assert_eq!(jobs[2].1, c.id);
    assert_eq!(jobs[2].2, vec![DeviceSetting::HeadphoneDb(-15.0)]);
    for job in &jobs {
        f.finish_job(job, false);
    }
    let (skipped, failed) = scene_result(f.outcome(id));
    assert!(failed.is_empty());
    assert!(
        skipped
            .iter()
            .any(|issue| issue.target == "hardware wave_xlr")
    );
    assert!(
        skipped
            .iter()
            .any(|issue| issue.target == "hardware wave_xlr:missing")
    );
    assert_eq!(skipped.len(), 6);
}

#[test]
fn failed_source_save_preserves_old_mute_and_matrix_recall_preserves_extras_and_topology() {
    let mut f = Fixture::new(
        json!({"name":"Recall","sources":{"a":{"level":0.2,"muted":false}},
        "cells":{"a.personal":{"volume":0.1,"muted":false},"missing.personal":{"volume":0.9}},
        "outputs":{"personal":"openwave_chat_mix","chat":"missing_output"},"volumes":{"personal":{"volume":0.4}},
        "hardware":{"wave_xlr:A":{"gain_raw":2304,"mute":false}}}),
        None,
    );
    let a = unit(ProfileId::WaveXlr, 1, "A", true);
    f.connect(&[a], true);
    let original = std::fs::read(f.root.path().join("sources.json")).unwrap();
    std::fs::rename(
        f.root.path().join("sources.json"),
        f.root.path().join("sources.saved"),
    )
    .unwrap();
    std::os::unix::fs::symlink("sources.saved", f.root.path().join("sources.json")).unwrap();
    let id = f.apply();
    let job = f.job();
    assert_eq!(
        job.2,
        vec![DeviceSetting::GainRaw(2304), DeviceSetting::Mute(true)]
    );
    f.finish_job(&job, false);
    let (skipped, failed) = scene_result(f.outcome(id));
    assert!(
        failed
            .iter()
            .any(|issue| issue.target == "source persistence")
    );
    assert!(
        skipped
            .iter()
            .any(|issue| issue.target == "cell missing.personal")
    );
    assert!(
        skipped
            .iter()
            .any(|issue| issue.target == "output personal")
    );
    assert_eq!(
        std::fs::read(f.root.path().join("sources.saved")).unwrap(),
        original
    );
    let matrix = f.read("mixes.json");
    assert_eq!(
        matrix["a.personal"],
        json!({"volume":0.1,"muted":false,"custom":42})
    );
    assert_eq!(matrix["marker"], "retain");
    assert_eq!(matrix["outputs"]["personal"], "none");
    assert_eq!(matrix["outputs"]["chat"], "missing_output");
    assert_eq!(matrix["volumes"]["personal"]["custom"], 43);
    assert_eq!(f.handle.snapshot().desired.sources.len(), 2);
    assert_eq!(f.handle.snapshot().desired.mixes.len(), 3);
}

#[test]
fn corrupt_matrix_is_untouched_but_source_recall_is_reported_as_partial() {
    let corrupt = "{broken matrix";
    let mut f = Fixture::new(
        json!({"name":"Recall","sources":{"a":{"level":0.25}},"cells":{"a.personal":{"volume":0.4}}}),
        Some(("mixes.json", corrupt)),
    );
    let save = f.submit(AppCommand::SaveScene {
        name: "Must not save".into(),
    });
    assert!(matches!(f.outcome(save), CommandOutcome::Rejected(_)));
    let id = f.apply();
    let (_, failed) = scene_result(f.outcome(id));
    assert!(
        failed
            .iter()
            .any(|issue| issue.target == "matrix persistence")
    );
    assert_eq!(f.read("sources.json")["a"]["level"], 0.25);
    assert_eq!(
        std::fs::read_to_string(f.root.path().join("mixes.json")).unwrap(),
        corrupt
    );
    assert!(
        f.read("scenes.json")["scenes"]
            .get("must-not-save")
            .is_none()
    );
}

#[test]
fn save_uses_committed_levels_poll_cache_and_replaces_slug_without_private_fields() {
    let mut f = Fixture::new(json!({"name":"Recall"}), None);
    let a = unit(ProfileId::WaveXlr, 1, "DUPLICATE", true);
    let b = unit(ProfileId::WaveXlr, 2, "DUPLICATE", false);
    let c = unit(ProfileId::Wave3, 3, "C", false);
    f.connect(&[a, b, c.clone()], false);
    let save = f.submit(AppCommand::SaveScene {
        name: "  Saved Mix!  ".into(),
    });
    assert!(matches!(f.outcome(save), CommandOutcome::Applied { .. }));
    let saved = f.read("scenes.json")["scenes"]["saved-mix"].clone();
    assert_eq!(saved["name"], "Saved Mix!");
    assert_eq!(saved["sources"]["a"], json!({"level":0.7,"muted":true}));
    assert_eq!(
        saved["cells"]["a.personal"],
        json!({"volume":0.8,"muted":true})
    );
    assert_eq!(
        saved["volumes"]["personal"],
        json!({"volume":0.6,"muted":false})
    );
    assert_eq!(
        saved["hardware"],
        json!({"wave3:C":{"gain_raw":1280,"mute":false,"hp_volume_db":-12.0,"monitor_mix":0}})
    );
    assert!(saved.get("fx").is_none());
    assert!(saved["hardware"]["wave3:C"].get("phantom").is_none());
    // Replace the same slug after a committed edit; saved levels follow desired
    // state, without carrying unknown source/cell fields into the strict scene.
    let edit = f.submit(AppCommand::SetCell {
        source: SourceId::new("a").unwrap(),
        mix: MixId::new("personal").unwrap(),
        level: 0.1,
        muted: false,
        timing: EditTiming::Immediate,
    });
    assert!(matches!(f.outcome(edit), CommandOutcome::Applied { .. }));
    let save = f.submit(AppCommand::SaveScene {
        name: "Saved Mix?".into(),
    });
    assert!(matches!(f.outcome(save), CommandOutcome::Applied { .. }));
    let saved = f.read("scenes.json");
    assert_eq!(saved["scenes"].as_object().unwrap().len(), 2);
    assert_eq!(
        saved["scenes"]["saved-mix"]["cells"]["a.personal"]["volume"],
        0.1
    );
    let delete = f.submit(AppCommand::DeleteScene {
        scene: SceneId::new("saved-mix").unwrap(),
    });
    assert!(matches!(f.outcome(delete), CommandOutcome::Applied { .. }));
    assert!(f.read("scenes.json")["scenes"].get("saved-mix").is_none());
}

#[test]
fn duplicate_serial_and_ambiguous_legacy_entries_are_skipped_without_device_admission() {
    let mut f = Fixture::new(
        json!({"name":"Recall","hardware":{
            "wave_xlr":{"mute":true},"wave_xlr:DUPLICATE":{"gain_raw":512},"wave3:absent":{"mute":false}
        }}),
        None,
    );
    f.connect(
        &[
            unit(ProfileId::WaveXlr, 1, "DUPLICATE", true),
            unit(ProfileId::WaveXlr, 2, "DUPLICATE", false),
        ],
        false,
    );
    let id = f.apply();
    let (skipped, failed) = scene_result(f.outcome(id));
    assert!(failed.is_empty());
    assert_eq!(
        skipped
            .iter()
            .map(|issue| issue.target.as_str())
            .collect::<Vec<_>>(),
        vec![
            "hardware wave_xlr",
            "hardware wave_xlr:DUPLICATE",
            "hardware wave3:absent"
        ]
    );
    assert!(
        !f.outgoing
            .try_iter()
            .any(|command| matches!(command, BackendCommand::Device { .. }))
    );
}

#[test]
fn corrupt_scene_payload_is_rejected_before_any_partial_recall() {
    let invalid = r#"{"scenes":{"recall":{"name":"Invalid","sources":{"a":{"level":0.2}},"hardware":{"wave_xlr:A":{"phantom":true}}}}}"#;
    let mut f = Fixture::new(json!({"name":"unused"}), Some(("scenes.json", invalid)));
    f.connect(&[unit(ProfileId::WaveXlr, 1, "A", true)], false);
    let original = std::fs::read(f.root.path().join("sources.json")).unwrap();
    let id = f.apply();
    assert!(matches!(f.outcome(id), CommandOutcome::Rejected(_)));
    assert_eq!(
        std::fs::read(f.root.path().join("sources.json")).unwrap(),
        original
    );
    assert_eq!(
        std::fs::read_to_string(f.root.path().join("scenes.json")).unwrap(),
        invalid
    );
    assert!(
        !f.outgoing
            .try_iter()
            .any(|command| matches!(command, BackendCommand::Device { .. }))
    );
}

#[test]
fn recall_supersedes_only_its_pending_cell_and_device_fields() {
    let mut f = Fixture::new(
        json!({"name":"Recall","cells":{"a.personal":{"volume":0.2,"muted":false}},"hardware":{"wave_xlr:A":{"gain_raw":4096}}}),
        None,
    );
    let a = unit(ProfileId::WaveXlr, 1, "A", true);
    let b = unit(ProfileId::Wave3, 2, "B", false);
    f.connect(&[a.clone(), b.clone()], false);
    let source_a = SourceId::new("a").unwrap();
    let source_b = SourceId::new("b").unwrap();
    let mix = MixId::new("personal").unwrap();
    let old_cell = f.submit(AppCommand::SetCell {
        source: source_a.clone(),
        mix: mix.clone(),
        level: 0.9,
        muted: false,
        timing: EditTiming::Debounced,
    });
    let other_cell = f.submit(AppCommand::SetCell {
        source: source_b.clone(),
        mix: mix.clone(),
        level: 0.3,
        muted: true,
        timing: EditTiming::Debounced,
    });
    let old_gain = f.submit(AppCommand::SetDeviceSetting {
        unit: a.id,
        setting: DeviceSetting::GainRaw(512),
        timing: EditTiming::Debounced,
    });
    let other_hp = f.submit(AppCommand::SetDeviceSetting {
        unit: b.id,
        setting: DeviceSetting::HeadphoneDb(-18.0),
        timing: EditTiming::Debounced,
    });
    let scene = f.apply();
    assert!(matches!(f.outcome(old_cell), CommandOutcome::Cancelled));
    assert!(matches!(f.outcome(old_gain), CommandOutcome::Cancelled));
    let recalled = f.job();
    assert_eq!(recalled.1, a.id);
    assert_eq!(recalled.2, vec![DeviceSetting::GainRaw(4096)]);
    f.finish_job(&recalled, false);
    let (_, failed) = scene_result(f.outcome(scene));
    assert!(failed.is_empty());
    let unrelated = f.job();
    assert_eq!(unrelated.1, b.id);
    assert_eq!(unrelated.2, vec![DeviceSetting::HeadphoneDb(-18.0)]);
    f.finish_job(&unrelated, false);
    assert!(matches!(
        f.outcome(other_hp),
        CommandOutcome::Applied { .. }
    ));
    assert!(matches!(
        f.outcome(other_cell),
        CommandOutcome::Applied { .. }
    ));
    let matrix = f.read("mixes.json");
    assert_eq!(matrix["a.personal"]["volume"], 0.2);
    assert_eq!(matrix["a.personal"]["muted"], false);
    assert_eq!(matrix["b.personal"]["volume"], 0.3);
    assert_eq!(matrix["b.personal"]["muted"], true);
}

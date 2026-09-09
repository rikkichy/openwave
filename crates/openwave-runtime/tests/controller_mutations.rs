use indexmap::IndexMap;
use openwave_core::{
    model::*,
    profiles::ProfileId,
    protocol::{ConfigBuffer, DeviceInfo},
};
use openwave_runtime::{
    controller::*,
    mixer::MixerObservation,
    uninstall::{UninstallPlan, UninstallResult},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

struct FixtureBackend {
    identity: PathBuf,
    commands: mpsc::Sender<BackendCommand>,
    events: mpsc::Receiver<BackendEvent>,
    activation: Option<mpsc::Receiver<Result<()>>>,
    shutdown: Option<mpsc::Receiver<std::result::Result<(), ShutdownError>>>,
}
impl Backend for FixtureBackend {
    fn identity(&self) -> &Path {
        &self.identity
    }
    fn dispatch(&mut self, command: BackendCommand) -> Result<()> {
        let activating = matches!(command, BackendCommand::Activate);
        self.commands
            .send(command)
            .map_err(|_| OperationError::unavailable("fixture closed"))?;
        if activating && let Some(results) = &self.activation {
            return results
                .recv_timeout(Duration::from_secs(5))
                .expect("activation response deadline");
        }
        Ok(())
    }
    fn next_event(&mut self) -> Option<BackendEvent> {
        self.events.try_recv().ok()
    }
    fn shutdown(&mut self) -> std::result::Result<(), ShutdownError> {
        self.shutdown.as_ref().map_or(Ok(()), |results| {
            results
                .recv_timeout(Duration::from_secs(5))
                .expect("shutdown response deadline")
        })
    }
    fn prepare_removal(&mut self, _: &UninstallPlan, _: bool) -> Result<()> {
        Err(OperationError::unavailable("fixture forbids removal"))
    }
    fn remove(&mut self, _: &UninstallPlan, _: bool) -> Result<UninstallResult> {
        Err(OperationError::unavailable("fixture forbids removal"))
    }
}
struct Rig {
    root: tempfile::TempDir,
    handle: RuntimeHandle,
    incoming: mpsc::Sender<BackendEvent>,
    commands: mpsc::Receiver<BackendCommand>,
    completed: mpsc::Receiver<(CommandId, CommandOutcome)>,
    saved: HashMap<CommandId, CommandOutcome>,
    pump: Option<thread::JoinHandle<()>>,
    shutdown_finished: mpsc::Receiver<std::result::Result<(), ShutdownError>>,
}
impl Rig {
    fn new(sources: Value, matrix: Value, corrupt: Option<(&str, &str)>) -> Self {
        Self::controlled(sources, matrix, corrupt, None, None)
    }
    fn controlled(
        sources: Value,
        matrix: Value,
        corrupt: Option<(&str, &str)>,
        activation: Option<mpsc::Receiver<Result<()>>>,
        shutdown: Option<mpsc::Receiver<std::result::Result<(), ShutdownError>>>,
    ) -> Self {
        let root = tempfile::tempdir().unwrap();
        for (file, data) in [("sources.json", sources), ("mixes.json", matrix)] {
            std::fs::write(root.path().join(file), serde_json::to_vec(&data).unwrap()).unwrap();
        }
        if let Some((file, data)) = corrupt {
            std::fs::write(root.path().join(file), data).unwrap();
        }
        let (incoming, events) = mpsc::channel();
        let (send, commands) = mpsc::channel();
        let identity = root.path().to_owned();
        let (handle, notifications) = RuntimeHandle::start_with(identity.clone(), move || {
            Ok(Box::new(FixtureBackend {
                identity,
                commands: send,
                events,
                activation,
                shutdown,
            }))
        })
        .unwrap();
        let (send, completed) = mpsc::channel();
        let (shutdown_events, shutdown_finished) = mpsc::channel();
        let pump = thread::spawn(move || {
            let context = glib::MainContext::new();
            while let Ok(event) = context.block_on(notifications.recv()) {
                match event {
                    RuntimeEvent::CommandFinished { id, result } => {
                        if send.send((id, result)).is_err() {
                            break;
                        }
                    }
                    RuntimeEvent::ShutdownFinished(result) => {
                        let _ = shutdown_events.send(result);
                    }
                    _ => {}
                }
            }
        });
        let rig = Self {
            root,
            handle,
            incoming,
            commands,
            completed,
            saved: HashMap::new(),
            pump: Some(pump),
            shutdown_finished,
        };
        rig.wait(|s| s.revision > 0);
        rig
    }
    fn wait(&self, predicate: impl Fn(&AppSnapshot) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !predicate(&self.handle.snapshot()) {
            assert!(Instant::now() < deadline, "snapshot deadline");
            thread::sleep(Duration::from_millis(2));
        }
    }
    fn submit(&self, command: AppCommand) -> CommandId {
        self.handle.submit(command).unwrap()
    }
    fn result(&mut self, id: CommandId) -> CommandOutcome {
        if let Some(result) = self.saved.remove(&id) {
            return result;
        }
        loop {
            let (next, result) = self
                .completed
                .recv_timeout(Duration::from_secs(5))
                .expect("completion deadline");
            if next == id {
                return result;
            }
            assert!(
                self.saved.insert(next, result).is_none(),
                "duplicate completion"
            );
        }
    }
    fn apply(&mut self, command: AppCommand) {
        let id = self.submit(command);
        assert!(matches!(self.result(id), CommandOutcome::Applied { .. }));
    }
    fn barrier(&self, label: &str) {
        self.incoming
            .send(BackendEvent::Status {
                service: label.into(),
                setup_required: false,
            })
            .unwrap();
        self.wait(|s| s.service_status == label);
    }
    fn graph(&self, captures: Vec<CaptureSnapshot>, mixes: IndexMap<MixId, NodeIdentity>) {
        self.incoming
            .send(BackendEvent::Graph(MixerObservation {
                observation: Observation::Known(()),
                captures,
                streams: vec![],
                outputs: vec![],
                default_sink: None,
                meter_targets: vec![],
                mix_identities: mixes,
                errors: vec![],
                silent_sources: HashMap::new(),
                revision: self.handle.snapshot().revision,
            }))
            .unwrap();
    }
    fn device_job(&self) -> (u64, UnitId, Vec<DeviceSetting>) {
        loop {
            if let BackendCommand::Device {
                job,
                unit,
                settings,
            } = self
                .commands
                .recv_timeout(Duration::from_secs(5))
                .expect("device deadline")
            {
                return (job, unit, settings);
            }
        }
    }
    fn drain(&self) -> Vec<BackendCommand> {
        self.commands.try_iter().collect()
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        let _ = self.handle.submit(AppCommand::Shutdown);
        let _ = self.handle.wait_stopped();
        if let Some(pump) = self.pump.take() {
            let _ = pump.join();
        }
    }
}
fn sid(value: &str) -> SourceId {
    SourceId::new(value).unwrap()
}
fn mid(value: &str) -> MixId {
    MixId::new(value).unwrap()
}
fn unit(address: u8, serial: &str, muted: bool) -> UnitSnapshot {
    let profile = ProfileId::WaveXlr;
    let mut state = ConfigBuffer::decode(profile, &vec![0; profile.profile().config_len])
        .unwrap()
        .state();
    state.muted = muted;
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
fn capture(name: &str, serial: Option<&str>, muted: Option<bool>) -> CaptureSnapshot {
    CaptureSnapshot {
        identity: NodeIdentity {
            server_cookie: 1,
            object_serial: name.into(),
        },
        node_id: 10,
        node_name: name.into(),
        name: name.into(),
        muted: muted
            .map(Observation::Known)
            .unwrap_or_else(|| Observation::Unknown(OperationError::unavailable("not observed"))),
        channels: Some(1),
        properties: serial
            .map(|serial| json!({"device.serial":serial}))
            .unwrap_or(json!({}))
            .as_object()
            .unwrap()
            .clone(),
    }
}
fn grouped() -> Value {
    json!({"a":{"kind":"device","node_name":"mic_a","muted":true,"group":"Mics"},"b":{"kind":"device","node_name":"mic_b","muted":false,"group":"Mics"}})
}

#[test]
fn same_hardware_value_reconciles_stale_rows_and_peers_without_origin_echo() {
    let f = Rig::new(grouped(), json!({}), None);
    let a = unit(1, "A", false);
    let b = unit(2, "B", false);
    f.incoming.send(BackendEvent::Unit(a.clone())).unwrap();
    f.incoming.send(BackendEvent::Unit(b.clone())).unwrap();
    f.graph(
        vec![
            capture("mic_a", Some("A"), None),
            capture("mic_b", Some("B"), None),
        ],
        IndexMap::new(),
    );
    f.barrier("bound");
    f.drain();
    f.incoming.send(BackendEvent::Unit(a.clone())).unwrap();
    f.barrier("reconciled");
    let snapshot = f.handle.snapshot();
    assert!(!snapshot.desired.sources["a"].muted);
    assert!(snapshot.desired.sources["b"].muted);
    assert_eq!(
        snapshot
            .units
            .iter()
            .find(|u| u.id == b.id)
            .unwrap()
            .desired_mute,
        Some(true)
    );
    let writes: Vec<_> = f
        .drain()
        .into_iter()
        .filter_map(|command| match command {
            BackendCommand::Device { unit, settings, .. } => Some((unit, settings)),
            _ => None,
        })
        .collect();
    assert_eq!(writes, vec![(b.id, vec![DeviceSetting::Mute(true)])]);
    f.incoming.send(BackendEvent::Unit(a)).unwrap();
    f.barrier("same-again");
    assert!(
        !f.drain()
            .iter()
            .any(|command| matches!(command, BackendCommand::Device { .. }))
    );
}

#[test]
fn saved_nonvendor_mute_survives_first_observation_and_follows_later_edges() {
    let f = Rig::new(
        json!({"mic":{"kind":"device","node_name":"external_mic","muted":true}}),
        json!({"mic.personal":{"volume":0.7,"muted":false}}),
        None,
    );
    f.graph(
        vec![capture("external_mic", None, Some(false))],
        IndexMap::new(),
    );
    f.barrier("first");
    assert!(f.handle.snapshot().desired.sources["mic"].muted);
    assert_eq!(
        f.handle
            .snapshot()
            .desired
            .matrix
            .cell(&sid("mic"), &mid("personal"))
            .volume,
        0.7
    );
    let saved: Value =
        serde_json::from_slice(&std::fs::read(f.root.path().join("sources.json")).unwrap())
            .unwrap();
    assert_eq!(saved["mic"]["muted"], true);
    for command in f.drain() {
        match command {
            BackendCommand::Routing { desired, .. } => assert!(desired.sources["mic"].muted),
            BackendCommand::CaptureMute { muted, .. } => assert!(muted),
            _ => {}
        }
    }
    f.graph(
        vec![capture("external_mic", None, Some(false))],
        IndexMap::new(),
    );
    f.barrier("same-baseline");
    assert!(f.handle.snapshot().desired.sources["mic"].muted);
    f.graph(
        vec![capture("external_mic", None, Some(true))],
        IndexMap::new(),
    );
    f.barrier("external-muted");
    f.graph(
        vec![capture("external_mic", None, Some(false))],
        IndexMap::new(),
    );
    f.barrier("external-opened");
    assert!(!f.handle.snapshot().desired.sources["mic"].muted);
    assert!(
        !f.drain()
            .iter()
            .any(|c| matches!(c, BackendCommand::CaptureMute { .. }))
    );
}

#[test]
fn remote_send_supersedes_delayed_drag_and_never_opens_the_old_gain() {
    let mut f = Rig::new(
        json!({"music":{}}),
        json!({"music.personal":{"volume":0.8,"muted":true}}),
        None,
    );
    f.drain();
    let old = f.submit(AppCommand::SetCell {
        source: sid("music"),
        mix: mid("personal"),
        level: 0.1,
        muted: false,
        timing: EditTiming::Debounced,
    });
    let new = f.submit(AppCommand::SetCell {
        source: sid("music"),
        mix: mid("personal"),
        level: 0.2,
        muted: false,
        timing: EditTiming::Immediate,
    });
    assert!(matches!(f.result(old), CommandOutcome::Cancelled));
    assert!(matches!(f.result(new), CommandOutcome::Applied { .. }));
    thread::sleep(Duration::from_millis(220));
    f.barrier("after-debounce");
    assert_eq!(
        f.handle.snapshot().desired.matrix.cells["music.personal"].volume,
        0.2
    );
    for command in f.drain() {
        if let BackendCommand::Routing { desired, .. } = command {
            let cell = &desired.matrix.cells["music.personal"];
            assert!(cell.muted || cell.volume == 0.2);
        }
    }
    let persisted: Value =
        serde_json::from_slice(&std::fs::read(f.root.path().join("mixes.json")).unwrap()).unwrap();
    assert_eq!(persisted["music.personal"]["volume"], 0.2);
}

#[test]
fn selection_cancels_unsent_gain_but_cannot_retarget_admitted_work() {
    let mut f = Rig::new(json!({}), json!({}), None);
    let a = unit(1, "A", false);
    let b = unit(2, "B", false);
    f.incoming.send(BackendEvent::Unit(a.clone())).unwrap();
    f.incoming.send(BackendEvent::Unit(b.clone())).unwrap();
    f.barrier("units");
    f.drain();
    let pending = f.submit(AppCommand::SetDeviceSetting {
        unit: a.id,
        setting: DeviceSetting::GainRaw(1280),
        timing: EditTiming::Debounced,
    });
    f.apply(AppCommand::SelectUnit { unit: Some(b.id) });
    assert!(matches!(f.result(pending), CommandOutcome::Cancelled));
    let admitted = f.submit(AppCommand::SetDeviceSetting {
        unit: a.id,
        setting: DeviceSetting::GainRaw(2560),
        timing: EditTiming::Immediate,
    });
    let job = f.device_job();
    assert_eq!(job.1, a.id);
    assert_eq!(job.2, vec![DeviceSetting::GainRaw(2560)]);
    f.apply(AppCommand::SelectUnit { unit: Some(b.id) });
    f.incoming
        .send(BackendEvent::DeviceFinished {
            job: job.0,
            unit: a.id,
            result: a
                .state
                .known()
                .cloned()
                .ok_or_else(|| OperationError::unavailable("state")),
        })
        .unwrap();
    assert!(matches!(f.result(admitted), CommandOutcome::Applied { .. }));
    thread::sleep(Duration::from_millis(220));
    assert!(
        !f.drain()
            .iter()
            .any(|c| matches!(c, BackendCommand::Device { .. }))
    );
}

#[test]
fn gain_lock_inside_debounce_prevents_the_delayed_write() {
    let mut f = Rig::new(json!({}), json!({}), None);
    let a = unit(1, "A", false);
    f.incoming.send(BackendEvent::Unit(a.clone())).unwrap();
    f.barrier("unit");
    f.drain();
    let pending = f.submit(AppCommand::SetDeviceSetting {
        unit: a.id,
        setting: DeviceSetting::GainRaw(1280),
        timing: EditTiming::Debounced,
    });
    f.apply(AppCommand::SetGainLock { locked: true });
    assert!(matches!(f.result(pending), CommandOutcome::Cancelled));
    thread::sleep(Duration::from_millis(240));
    f.barrier("locked");
    assert!(
        !f.drain()
            .iter()
            .any(|c| matches!(c, BackendCommand::Device { .. }))
    );
    let rejected = f.submit(AppCommand::SetDeviceSetting {
        unit: a.id,
        setting: DeviceSetting::GainRaw(2560),
        timing: EditTiming::Immediate,
    });
    assert!(matches!(f.result(rejected), CommandOutcome::Rejected(_)));
}

#[test]
fn corrupt_geometry_is_untouched_and_does_not_block_explicit_hardware_mute() {
    let mut f = Rig::new(json!({}), json!({}), Some(("ui-state.json", "{corrupt")));
    let a = unit(1, "A", false);
    f.incoming.send(BackendEvent::Unit(a.clone())).unwrap();
    f.barrier("unit");
    let rejected = f.submit(AppCommand::SetPreferences {
        changes: PreferencesEdit {
            width: Some(900),
            ..PreferencesEdit::default()
        },
    });
    assert!(matches!(f.result(rejected), CommandOutcome::Rejected(_)));
    let accepted = f.submit(AppCommand::SetDeviceSetting {
        unit: a.id,
        setting: DeviceSetting::Mute(true),
        timing: EditTiming::Immediate,
    });
    let job = f.device_job();
    assert_eq!(job.2, vec![DeviceSetting::Mute(true)]);
    let mut state = a.state.known().unwrap().clone();
    state.muted = true;
    f.incoming
        .send(BackendEvent::DeviceFinished {
            job: job.0,
            unit: a.id,
            result: Ok(state),
        })
        .unwrap();
    assert!(matches!(f.result(accepted), CommandOutcome::Applied { .. }));
    assert_eq!(
        std::fs::read_to_string(f.root.path().join("ui-state.json")).unwrap(),
        "{corrupt"
    );
}

#[test]
fn old_incarnation_cannot_receive_edits_after_replacement() {
    let mut f = Rig::new(json!({}), json!({}), None);
    let old = unit(1, "A", false);
    f.incoming.send(BackendEvent::Unit(old.clone())).unwrap();
    f.barrier("old");
    let pending = f.submit(AppCommand::SetDeviceSetting {
        unit: old.id,
        setting: DeviceSetting::GainRaw(1280),
        timing: EditTiming::Debounced,
    });
    f.incoming.send(BackendEvent::UnitRetired(old.id)).unwrap();
    let mut replacement = old.clone();
    replacement.id.incarnation = 2;
    f.incoming
        .send(BackendEvent::Unit(replacement.clone()))
        .unwrap();
    f.barrier("new");
    assert!(matches!(f.result(pending), CommandOutcome::Cancelled));
    let stale = f.submit(AppCommand::SetDeviceSetting {
        unit: old.id,
        setting: DeviceSetting::GainRaw(2560),
        timing: EditTiming::Immediate,
    });
    assert!(matches!(f.result(stale), CommandOutcome::Rejected(_)));
    assert_eq!(f.handle.snapshot().selected_unit, Some(replacement.id));
    assert!(
        !f.drain()
            .iter()
            .any(|c| matches!(c, BackendCommand::Device { .. }))
    );
}

#[test]
fn stale_master_revision_or_runtime_identity_cannot_overwrite_new_desire() {
    let mut f = Rig::new(
        json!({}),
        json!({"volumes":{"personal":{"volume":0.6,"muted":false}}}),
        None,
    );
    let identity = NodeIdentity {
        server_cookie: 1,
        object_serial: "101".into(),
    };
    f.graph(
        vec![],
        IndexMap::from([(mid("personal"), identity.clone())]),
    );
    f.barrier("sink");
    let previous = f.handle.snapshot().revision;
    f.apply(AppCommand::SetMaster {
        mix: mid("personal"),
        level: 0.3,
        muted: false,
    });
    let current = f.handle.snapshot().revision;
    f.incoming
        .send(BackendEvent::Master {
            mix: mid("personal"),
            level: 0.9,
            muted: false,
            revision: previous,
            identity: identity.clone(),
        })
        .unwrap();
    f.barrier("old-revision");
    assert_eq!(
        f.handle.snapshot().desired.matrix.volumes["personal"].volume,
        0.3
    );
    let replacement = NodeIdentity {
        server_cookie: 2,
        object_serial: "101".into(),
    };
    f.graph(vec![], IndexMap::from([(mid("personal"), replacement)]));
    f.barrier("recreated");
    f.incoming
        .send(BackendEvent::Master {
            mix: mid("personal"),
            level: 1.0,
            muted: false,
            revision: current,
            identity,
        })
        .unwrap();
    f.barrier("old-identity");
    assert_eq!(
        f.handle.snapshot().desired.matrix.volumes["personal"].volume,
        0.3
    );
}

#[test]
fn single_wave_legacy_send_migrates_and_protected_row_requires_known_absence() {
    let mut f = Rig::new(
        json!({}),
        json!({"mic.personal":{"volume":0.375,"muted":true,"note":"preserved"}}),
        None,
    );
    let name = "alsa_input.usb-Elgato_Systems_Elgato_Wave_XLR_fixture.analog-stereo";
    f.graph(vec![capture(name, None, None)], IndexMap::new());
    f.barrier("discovered");
    let source = f
        .handle
        .snapshot()
        .desired
        .sources
        .values()
        .find(|s| s.node_name == name)
        .unwrap()
        .clone();
    assert!(source.protected);
    let key = format!("{}.personal", source.id);
    let snapshot = f.handle.snapshot();
    assert_eq!(snapshot.desired.matrix.cells[&key].volume, 0.375);
    assert_eq!(
        snapshot.desired.matrix.cells[&key].extra["note"],
        "preserved"
    );
    assert!(!snapshot.desired.matrix.cells.contains_key("mic.personal"));
    let protected = f.submit(AppCommand::RemoveSource {
        source: source.id.clone(),
    });
    assert!(matches!(f.result(protected), CommandOutcome::Rejected(_)));
    f.graph(vec![], IndexMap::new());
    f.barrier("absent");
    f.apply(AppCommand::RemoveSource {
        source: source.id.clone(),
    });
    assert!(!f.handle.snapshot().desired.sources.contains_key(&source.id));
    assert!(!f.handle.snapshot().desired.matrix.cells.contains_key(&key));
    assert!(
        !f.handle
            .snapshot()
            .preferences
            .offered_capture_nodes
            .iter()
            .any(|node| node == name)
    );
    let preferences: Value =
        serde_json::from_slice(&std::fs::read(f.root.path().join("ui-state.json")).unwrap())
            .unwrap();
    assert!(
        !preferences["offered_capture_nodes"]
            .as_array()
            .unwrap()
            .contains(&json!(name))
    );
    f.graph(vec![capture(name, None, None)], IndexMap::new());
    f.barrier("replugged");
    let snapshot = f.handle.snapshot();
    let replacement = snapshot
        .desired
        .sources
        .values()
        .find(|row| row.node_name == name)
        .unwrap();
    assert!(replacement.protected);
    assert_ne!(replacement.id, source.id);
}

#[test]
fn corrupt_topology_never_creates_routes_or_replaces_original_bytes() {
    let mut f = Rig::new(json!({}), json!({}), Some(("sources.json", "{corrupt")));
    let rejected = f.submit(AppCommand::AddSource {
        source: Source::new("Music".into(), SourceKind::App),
    });
    assert!(matches!(f.result(rejected), CommandOutcome::Rejected(_)));
    assert!(
        !f.drain()
            .iter()
            .any(|c| matches!(c, BackendCommand::Routing { .. }))
    );
    assert_eq!(
        std::fs::read_to_string(f.root.path().join("sources.json")).unwrap(),
        "{corrupt"
    );
}

#[test]
fn remote_level_preserves_the_latest_mute_and_cancels_older_drag() {
    let mut f = Rig::new(
        json!({"music":{}}),
        json!({"music.personal":{"volume":0.8,"muted":false}}),
        None,
    );
    let mute = f.submit(AppCommand::ToggleCellMute {
        source: sid("music"),
        mix: mid("personal"),
    });
    let level = f.submit(AppCommand::SetCellLevel {
        source: sid("music"),
        mix: mid("personal"),
        level: 0.3,
    });
    assert!(matches!(f.result(mute), CommandOutcome::Applied { .. }));
    assert!(matches!(f.result(level), CommandOutcome::Applied { .. }));
    let old = f.submit(AppCommand::SetCell {
        source: sid("music"),
        mix: mid("personal"),
        level: 0.9,
        muted: false,
        timing: EditTiming::Debounced,
    });
    let remote = f.submit(AppCommand::SetCellLevel {
        source: sid("music"),
        mix: mid("personal"),
        level: 0.2,
    });
    assert!(matches!(f.result(old), CommandOutcome::Cancelled));
    assert!(matches!(f.result(remote), CommandOutcome::Applied { .. }));
    let snapshot = f.handle.snapshot();
    assert_eq!(snapshot.desired.matrix.cells["music.personal"].volume, 0.2);
    assert!(snapshot.desired.matrix.cells["music.personal"].muted);
    assert!(snapshot.pending_cells.is_empty());
}

#[test]
fn stale_graph_cannot_restart_a_meter_for_a_previous_capture_binding() {
    use openwave_runtime::meter::{MeterEvent, MeterTarget};
    let mut f = Rig::new(
        json!({"mic":{"kind":"device","node_name":"mic_a"}}),
        json!({}),
        None,
    );
    let a = capture("mic_a", None, None);
    let b = capture("mic_b", None, None);
    let revision = f.handle.snapshot().revision;
    let observation = || MixerObservation {
        observation: Observation::Known(()),
        captures: vec![a.clone(), b.clone()],
        streams: vec![],
        outputs: vec![],
        default_sink: None,
        meter_targets: vec![MeterTarget {
            key: "src:mic".into(),
            node_name: a.node_name.clone(),
            identity: a.identity.clone(),
            raw: true,
            channels: 1,
        }],
        errors: vec![],
        mix_identities: IndexMap::new(),
        silent_sources: HashMap::new(),
        revision,
    };
    f.incoming.send(BackendEvent::Graph(observation())).unwrap();
    f.barrier("first-meter");
    f.incoming
        .send(BackendEvent::Meter(MeterEvent {
            key: "src:mic".into(),
            identity: a.identity.clone(),
            generation: 1,
            peak: 0.5,
        }))
        .unwrap();
    f.barrier("first-peak");
    assert_eq!(f.handle.snapshot().meters["src:mic"], 0.5);
    f.apply(AppCommand::EditSource {
        source: sid("mic"),
        changes: SourceEdit {
            node_name: Some("mic_b".into()),
            ..SourceEdit::default()
        },
    });
    assert!(!f.handle.snapshot().meters.contains_key("src:mic"));
    f.incoming.send(BackendEvent::Graph(observation())).unwrap();
    f.incoming
        .send(BackendEvent::Meter(MeterEvent {
            key: "src:mic".into(),
            identity: a.identity.clone(),
            generation: 1,
            peak: 0.9,
        }))
        .unwrap();
    f.barrier("stale-meter");
    assert!(!f.handle.snapshot().meters.contains_key("src:mic"));
}

#[test]
fn mute_during_pending_drag_commits_the_preview_without_reopening_old_gain() {
    let mut f = Rig::new(
        json!({"music":{}}),
        json!({"music.personal":{"volume":0.8,"muted":true,"note":"keep"}}),
        None,
    );
    f.drain();
    let drag = f.submit(AppCommand::SetCell {
        source: sid("music"),
        mix: mid("personal"),
        level: 0.1,
        muted: false,
        timing: EditTiming::Debounced,
    });
    let mute = f.submit(AppCommand::ToggleCellMute {
        source: sid("music"),
        mix: mid("personal"),
    });
    assert!(matches!(f.result(drag), CommandOutcome::Cancelled));
    assert!(matches!(f.result(mute), CommandOutcome::Applied { .. }));
    let snapshot = f.handle.snapshot();
    let cell = &snapshot.desired.matrix.cells["music.personal"];
    assert_eq!(cell.volume, 0.1);
    assert!(cell.muted);
    assert_eq!(cell.extra["note"], "keep");
    assert!(snapshot.pending_cells.is_empty());
    thread::sleep(Duration::from_millis(220));
    f.barrier("mute-survives-debounce");
    let routed: Vec<_> = f
        .drain()
        .into_iter()
        .filter_map(|command| match command {
            BackendCommand::Routing { desired, .. } => {
                Some(desired.matrix.cells["music.personal"].clone())
            }
            _ => None,
        })
        .collect();
    assert!(!routed.is_empty());
    assert!(routed.iter().all(|cell| cell.volume == 0.1 && cell.muted));
    let persisted: Value =
        serde_json::from_slice(&std::fs::read(f.root.path().join("mixes.json")).unwrap()).unwrap();
    assert_eq!(persisted["music.personal"]["volume"], 0.1);
    assert_eq!(persisted["music.personal"]["muted"], true);
}

#[test]
fn atomic_displayed_mute_supersedes_a_pending_drag() {
    let mut f = Rig::new(
        json!({"music":{}}),
        json!({"music.personal":{"volume":0.8,"muted":true}}),
        None,
    );
    let drag = f.submit(AppCommand::SetCell {
        source: sid("music"),
        mix: mid("personal"),
        level: 0.1,
        muted: false,
        timing: EditTiming::Debounced,
    });
    let mute = f.submit(AppCommand::SetCell {
        source: sid("music"),
        mix: mid("personal"),
        level: 0.1,
        muted: true,
        timing: EditTiming::Immediate,
    });
    assert!(matches!(f.result(drag), CommandOutcome::Cancelled));
    assert!(matches!(f.result(mute), CommandOutcome::Applied { .. }));
    let snapshot = f.handle.snapshot();
    assert_eq!(snapshot.desired.matrix.cells["music.personal"].volume, 0.1);
    assert!(snapshot.desired.matrix.cells["music.personal"].muted);
    assert!(snapshot.pending_cells.is_empty());
}

#[test]
fn offered_capture_marker_is_retained_until_its_last_source_is_removed() {
    let node = "alsa_input.usb-Elgato_Systems_Elgato_Wave_XLR_shared.analog-stereo";
    let mut f = Rig::new(
        json!({"a":{"kind":"device","node_name":node},"b":{"kind":"device","node_name":node}}),
        json!({}),
        None,
    );
    f.apply(AppCommand::SetPreferences {
        changes: PreferencesEdit {
            offered_capture_nodes: Some(vec![node.into()]),
            ..PreferencesEdit::default()
        },
    });
    f.apply(AppCommand::RemoveSource { source: sid("a") });
    assert_eq!(
        f.handle.snapshot().preferences.offered_capture_nodes,
        vec![node.to_string()]
    );
    f.apply(AppCommand::RemoveSource { source: sid("b") });
    assert!(
        f.handle
            .snapshot()
            .preferences
            .offered_capture_nodes
            .is_empty()
    );
    f.graph(vec![capture(node, None, None)], IndexMap::new());
    f.barrier("offered-again");
    assert!(
        f.handle
            .snapshot()
            .desired
            .sources
            .values()
            .any(|row| row.node_name == node && row.protected)
    );
}

#[test]
fn capture_removal_does_not_publish_topology_when_preferences_are_corrupt() {
    let mut f = Rig::new(json!({}), json!({}), Some(("ui-state.json", "{corrupt")));
    let node = "alsa_input.usb-Elgato_Systems_Elgato_Wave_XLR_corrupt.analog-stereo";
    f.graph(vec![capture(node, None, None)], IndexMap::new());
    f.barrier("offered");
    let source = f
        .handle
        .snapshot()
        .desired
        .sources
        .values()
        .find(|row| row.node_name == node)
        .unwrap()
        .id
        .clone();
    f.graph(vec![], IndexMap::new());
    f.barrier("unplugged");
    let before = std::fs::read(f.root.path().join("sources.json")).unwrap();
    let removal = f.submit(AppCommand::RemoveSource {
        source: source.clone(),
    });
    assert!(
        matches!(f.result(removal), CommandOutcome::Rejected(error) if error.code == ErrorCode::CorruptStore)
    );
    assert!(f.handle.snapshot().desired.sources.contains_key(&source));
    assert_eq!(
        std::fs::read(f.root.path().join("sources.json")).unwrap(),
        before
    );
    assert_eq!(
        std::fs::read_to_string(f.root.path().join("ui-state.json")).unwrap(),
        "{corrupt"
    );
}

#[test]
fn repeated_activation_failures_remain_retryable_without_running_host_setup() {
    let (release, activation) = mpsc::channel();
    let mut f = Rig::controlled(json!({}), json!({}), None, Some(activation), None);
    f.incoming
        .send(BackendEvent::Status {
            service: "setup-required".into(),
            setup_required: true,
        })
        .unwrap();
    f.wait(|snapshot| snapshot.setup_phase == SetupPhase::Required);
    let setup = f.submit(AppCommand::RunSetup);
    f.wait(|snapshot| snapshot.setup_phase == SetupPhase::Running);
    f.incoming
        .send(BackendEvent::Setup {
            job: setup.0,
            result: Ok(openwave_runtime::setup::SetupOutcome {
                needs_replug: true,
                message: "replug".into(),
            }),
        })
        .unwrap();
    assert!(matches!(f.result(setup), CommandOutcome::Applied { .. }));
    assert!(matches!(
        f.handle.snapshot().setup_phase,
        SetupPhase::Replug(_)
    ));
    f.drain();
    for succeeds in [false, false, true] {
        let command = f.submit(AppCommand::ContinueSetup);
        loop {
            match f.commands.recv_timeout(Duration::from_secs(5)).unwrap() {
                BackendCommand::Activate => break,
                BackendCommand::Setup { .. } => panic!("activation must not rerun host setup"),
                _ => {}
            }
        }
        assert_eq!(f.handle.snapshot().setup_phase, SetupPhase::Starting);
        release
            .send(if succeeds {
                Ok(())
            } else {
                Err(OperationError::unavailable("vendor control busy"))
            })
            .unwrap();
        let outcome = f.result(command);
        if succeeds {
            assert!(matches!(outcome, CommandOutcome::Applied { .. }));
            assert_eq!(f.handle.snapshot().setup_phase, SetupPhase::Ready);
            assert!(!f.handle.snapshot().setup_required);
        } else {
            assert!(matches!(outcome, CommandOutcome::Rejected(_)));
            assert!(matches!(
                f.handle.snapshot().setup_phase,
                SetupPhase::ActivationFailed(_)
            ));
        }
        assert_eq!(f.handle.snapshot().lifecycle, Lifecycle::Running);
    }
}

#[test]
fn failed_quit_keeps_mutations_frozen_and_allows_drain_retry() {
    let (release, shutdown) = mpsc::channel();
    let mut f = Rig::controlled(json!({"music":{}}), json!({}), None, None, Some(shutdown));
    let a = unit(1, "A", false);
    f.incoming.send(BackendEvent::Unit(a.clone())).unwrap();
    f.barrier("unit");
    let admitted = f.submit(AppCommand::SetDeviceSetting {
        unit: a.id,
        setting: DeviceSetting::GainRaw(2560),
        timing: EditTiming::Immediate,
    });
    let (job, target, _) = f.device_job();
    let pending = f.submit(AppCommand::SetCell {
        source: sid("music"),
        mix: mid("personal"),
        level: 0.1,
        muted: false,
        timing: EditTiming::Debounced,
    });
    let quit = f.submit(AppCommand::Shutdown);
    f.wait(|snapshot| snapshot.lifecycle == Lifecycle::Draining);
    assert!(f.handle.snapshot().pending_cells.is_empty());
    assert!(
        f.handle
            .submit(AppCommand::SetSourceMute {
                source: sid("music"),
                muted: true
            })
            .is_err()
    );
    let mut state = a.state.known().unwrap().clone();
    state.gain_raw = 2560;
    f.incoming
        .send(BackendEvent::DeviceFinished {
            job,
            unit: target,
            result: Ok(state),
        })
        .unwrap();
    let failure = ShutdownError {
        message: "owned graph cleanup is incomplete".into(),
        issues: vec![OperationIssue {
            target: "mixer".into(),
            message: "discovery changed".into(),
        }],
    };
    release.send(Err(failure.clone())).unwrap();
    assert!(matches!(f.result(admitted), CommandOutcome::Applied { .. }));
    assert!(matches!(f.result(pending), CommandOutcome::Cancelled));
    assert!(matches!(f.result(quit), CommandOutcome::Rejected(_)));
    assert_eq!(
        f.shutdown_finished
            .recv_timeout(Duration::from_secs(5))
            .unwrap(),
        Err(failure)
    );
    f.wait(|snapshot| snapshot.lifecycle == Lifecycle::Frozen);
    assert_eq!(
        f.handle
            .snapshot()
            .units
            .iter()
            .find(|unit| unit.id == a.id)
            .unwrap()
            .state
            .known()
            .unwrap()
            .gain_raw,
        2560
    );
    assert!(
        f.handle
            .submit(AppCommand::SetSourceMute {
                source: sid("music"),
                muted: true
            })
            .is_err()
    );
    let retry = f.submit(AppCommand::Shutdown);
    f.wait(|snapshot| snapshot.lifecycle == Lifecycle::Draining);
    release.send(Ok(())).unwrap();
    assert!(matches!(f.result(retry), CommandOutcome::Applied { .. }));
    f.wait(|snapshot| snapshot.lifecycle == Lifecycle::Stopped);
    assert_eq!(
        f.shutdown_finished
            .recv_timeout(Duration::from_secs(5))
            .unwrap(),
        Ok(())
    );
    f.handle.wait_stopped().unwrap();
    f.pump.take().unwrap().join().unwrap();
    assert!(matches!(
        f.completed.try_recv(),
        Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected)
    ));
}

#[test]
fn protected_source_removal_requires_a_current_known_absence() {
    let mut f = Rig::new(
        json!({"mic":{"kind":"device","node_name":"mic_a","protected":true}}),
        json!({}),
        None,
    );
    let initial = f.submit(AppCommand::RemoveSource { source: sid("mic") });
    assert!(matches!(f.result(initial), CommandOutcome::Rejected(_)));
    assert!(f.handle.snapshot().desired.sources.contains_key("mic"));
    f.graph(vec![], IndexMap::new());
    f.barrier("known-absent");
    f.incoming
        .send(BackendEvent::Graph(MixerObservation::default()))
        .unwrap();
    f.barrier("discovery-unavailable");
    let unknown = f.submit(AppCommand::RemoveSource { source: sid("mic") });
    assert!(matches!(f.result(unknown), CommandOutcome::Rejected(_)));
    assert!(f.handle.snapshot().desired.sources.contains_key("mic"));
    f.graph(vec![], IndexMap::new());
    f.barrier("known-absent-again");
    f.apply(AppCommand::RemoveSource { source: sid("mic") });
    assert!(!f.handle.snapshot().desired.sources.contains_key("mic"));
}

fn mixed_group_fixture() -> (Rig, UnitSnapshot) {
    let f = Rig::new(
        json!({
            "a":{"kind":"device","node_name":"external_mic","muted":false,"group":"Mics"},
            "b":{"kind":"device","node_name":"wave_mic","muted":true,"group":"Mics"},
            "other":{"kind":"app","name":"Other","muted":true,"group":"Other"}
        }),
        json!({"a.personal":{"volume":0.7,"muted":false},"b.personal":{"volume":0.7,"muted":false}}),
        None,
    );
    let b = unit(2, "B", true);
    f.incoming.send(BackendEvent::Unit(b.clone())).unwrap();
    f.graph(
        vec![
            capture("external_mic", None, Some(false)),
            capture("wave_mic", Some("B"), Some(true)),
        ],
        IndexMap::new(),
    );
    f.barrier("mixed-bound");
    f.drain();
    (f, b)
}

fn request_wave_open(f: &Rig) -> CommandId {
    let command = f.submit(AppCommand::SetSourceMute {
        source: sid("b"),
        muted: false,
    });
    f.wait(|s| !s.desired.sources["b"].muted);
    f.barrier("wave-requested");
    let commands = f.drain();
    assert!(commands.iter().any(|c| matches!(c, BackendCommand::CaptureMute { node_name, muted: true, .. } if node_name == "external_mic")));
    assert_no_wave_open(commands);
    command
}

fn assert_no_wave_open(commands: Vec<BackendCommand>) {
    for command in commands {
        match command {
            BackendCommand::Device { settings, .. } => {
                assert!(!settings.contains(&DeviceSetting::Mute(false)))
            }
            BackendCommand::Routing { desired, .. } => assert!(desired.sources["b"].muted),
            _ => {}
        }
    }
}

#[test]
fn mixed_group_requests_silence_for_unchanged_saved_muted_peer() {
    let mut f = Rig::new(
        json!({
            "a":{"kind":"device","node_name":"external_mic","muted":true,"group":"Mics"},
            "b":{"kind":"device","node_name":"wave_mic","muted":true,"group":"Mics"}
        }),
        json!({"a.personal":{"volume":0.7},"b.personal":{"volume":0.7}}),
        None,
    );
    let b = unit(2, "B", true);
    f.incoming.send(BackendEvent::Unit(b.clone())).unwrap();
    f.graph(
        vec![
            capture("external_mic", None, Some(false)),
            capture("wave_mic", Some("B"), Some(true)),
        ],
        IndexMap::new(),
    );
    f.barrier("saved-mute-baseline");
    assert!(f.handle.snapshot().desired.sources["a"].muted);
    f.drain();
    let command = request_wave_open(&f);
    f.graph(
        vec![
            capture("external_mic", None, Some(true)),
            capture("wave_mic", Some("B"), Some(true)),
        ],
        IndexMap::new(),
    );
    f.barrier("saved-peer-silenced");
    let (job, target, settings) = f.device_job();
    assert_eq!(target, b.id);
    assert_eq!(settings, vec![DeviceSetting::Mute(false)]);
    let mut opened = b.state.known().unwrap().clone();
    opened.muted = false;
    f.incoming
        .send(BackendEvent::DeviceFinished {
            job,
            unit: target,
            result: Ok(opened),
        })
        .unwrap();
    assert!(matches!(f.result(command), CommandOutcome::Applied { .. }));
    assert!(f.handle.snapshot().desired.sources["a"].muted);
    assert!(!f.handle.snapshot().desired.sources["b"].muted);
}

#[test]
fn mixed_group_proven_absent_peer_releases_only_after_current_observation() {
    let mut f = Rig::new(
        json!({
            "a":{"kind":"device","node_name":"external_mic","muted":true,"group":"Mics"},
            "b":{"kind":"device","node_name":"wave_mic","muted":true,"group":"Mics"},
            "music":{"kind":"app","muted":false}
        }),
        json!({"a.personal":{"volume":0.7},"b.personal":{"volume":0.7}}),
        None,
    );
    let b = unit(2, "B", true);
    f.incoming.send(BackendEvent::Unit(b.clone())).unwrap();
    let observed = vec![capture("wave_mic", Some("B"), Some(true))];
    f.graph(observed.clone(), IndexMap::new());
    f.barrier("peer-initially-absent");
    f.drain();
    let request = f.submit(AppCommand::SetSourceMute {
        source: sid("b"),
        muted: false,
    });
    f.wait(|s| !s.desired.sources["b"].muted);
    f.barrier("absent-peer-open-requested");
    assert_no_wave_open(f.drain());
    f.incoming
        .send(BackendEvent::Graph(MixerObservation::default()))
        .unwrap();
    f.barrier("absence-not-inferred-from-unknown");
    assert_no_wave_open(f.drain());
    f.graph(observed, IndexMap::new());
    f.barrier("current-peer-absence-proven");
    let (job, target, settings) = f.device_job();
    assert_eq!(target, b.id);
    assert_eq!(settings, vec![DeviceSetting::Mute(false)]);
    let mut opened = b.state.known().unwrap().clone();
    opened.muted = false;
    f.incoming
        .send(BackendEvent::DeviceFinished {
            job,
            unit: target,
            result: Ok(opened),
        })
        .unwrap();
    assert!(matches!(f.result(request), CommandOutcome::Applied { .. }));
    assert!(f.handle.snapshot().desired.sources["a"].muted);
    assert!(!f.handle.snapshot().desired.sources["music"].muted);
}

#[test]
fn mixed_group_ambiguous_peer_is_not_proven_absent() {
    let mut f = Rig::new(
        json!({
            "a":{"kind":"device","node_name":"external_mic","muted":true,"group":"Mics"},
            "b":{"kind":"device","node_name":"wave_mic","muted":true,"group":"Mics"}
        }),
        json!({"b.personal":{"volume":0.7}}),
        None,
    );
    let b = unit(2, "B", true);
    f.incoming.send(BackendEvent::Unit(b)).unwrap();
    let first = capture("external_mic", None, Some(true));
    let mut duplicate = first.clone();
    duplicate.identity.object_serial = "different-capture".into();
    duplicate.node_id = 11;
    let observed = vec![first, duplicate, capture("wave_mic", Some("B"), Some(true))];
    f.graph(observed.clone(), IndexMap::new());
    f.barrier("ambiguous-peer-bound");
    f.drain();
    let request = f.submit(AppCommand::SetSourceMute {
        source: sid("b"),
        muted: false,
    });
    f.wait(|s| !s.desired.sources["b"].muted);
    f.barrier("ambiguous-peer-requested");
    assert_no_wave_open(f.drain());
    f.graph(observed, IndexMap::new());
    f.barrier("ambiguous-peer-still-not-silence");
    assert_no_wave_open(f.drain());
    f.apply(AppCommand::Shutdown);
    assert!(matches!(f.result(request), CommandOutcome::Cancelled));
}

#[test]
fn shared_wave_handover_requires_row_silence_without_muting_other_group() {
    let mut f = Rig::new(
        json!({
            "a":{"kind":"device","node_name":"wave_a","muted":false,"group":"Mics"},
            "b":{"kind":"device","node_name":"wave_b","muted":true,"group":"Mics"},
            "c":{"kind":"device","node_name":"wave_a","muted":false,"group":"Other"}
        }),
        json!({"a.personal":{"volume":0.7},"b.personal":{"volume":0.7},"c.personal":{"volume":0.5}}),
        None,
    );
    let a = unit(1, "A", false);
    let b = unit(2, "B", true);
    f.incoming.send(BackendEvent::Unit(a.clone())).unwrap();
    f.incoming.send(BackendEvent::Unit(b.clone())).unwrap();
    let captures = vec![
        capture("wave_a", Some("A"), Some(false)),
        capture("wave_b", Some("B"), Some(true)),
    ];
    f.graph(captures.clone(), IndexMap::new());
    f.barrier("shared-input-bound");
    let stale_revision = f.handle.snapshot().revision;
    f.drain();
    let command = f.submit(AppCommand::SetSourceMute {
        source: sid("b"),
        muted: false,
    });
    f.wait(|s| !s.desired.sources["b"].muted);
    f.barrier("shared-opening-requested");
    let mut shared_job = None;
    for command in f.drain() {
        match command {
            BackendCommand::Device {
                job,
                unit,
                settings,
            } => {
                assert_eq!(unit, a.id, "B must wait for A's row silence");
                assert_eq!(settings, vec![DeviceSetting::Mute(false)]);
                assert!(shared_job.replace(job).is_none());
            }
            BackendCommand::CaptureMute { .. } => panic!("shared capture must stay open"),
            BackendCommand::Routing { desired, .. } => {
                assert!(desired.sources["a"].muted);
                assert!(desired.sources["b"].muted);
                assert!(!desired.sources["c"].muted);
            }
            _ => {}
        }
    }
    f.incoming
        .send(BackendEvent::DeviceFinished {
            job: shared_job.expect("shared unit remains open for C"),
            unit: a.id,
            result: Ok(a.state.known().unwrap().clone()),
        })
        .unwrap();
    f.barrier("shared-device-open-is-not-row-silence");
    assert_no_wave_open(f.drain());
    assert!(f.handle.snapshot().desired.sources["a"].muted);
    assert!(!f.handle.snapshot().desired.sources["c"].muted);
    // Neither stale routing proof nor another node generation may release B.
    for (label, revision, identity) in [
        (
            "stale-row-silence",
            stale_revision,
            captures[0].identity.clone(),
        ),
        (
            "wrong-row-generation",
            f.handle.snapshot().revision,
            NodeIdentity {
                server_cookie: 99,
                object_serial: "replacement".into(),
            },
        ),
    ] {
        f.incoming
            .send(BackendEvent::Graph(MixerObservation {
                observation: Observation::Known(()),
                captures: captures.clone(),
                silent_sources: HashMap::from([(sid("a"), identity)]),
                revision,
                ..Default::default()
            }))
            .unwrap();
        f.barrier(label);
        assert_no_wave_open(f.drain());
    }
    f.incoming
        .send(BackendEvent::Graph(MixerObservation {
            observation: Observation::Known(()),
            captures: captures.clone(),
            silent_sources: HashMap::from([(sid("a"), captures[0].identity.clone())]),
            revision: f.handle.snapshot().revision,
            ..Default::default()
        }))
        .unwrap();
    f.barrier("exact-row-silence");
    let (job, target, settings) = f.device_job();
    assert_eq!(target, b.id);
    assert_eq!(settings, vec![DeviceSetting::Mute(false)]);
    let mut opened = b.state.known().unwrap().clone();
    opened.muted = false;
    f.incoming
        .send(BackendEvent::DeviceFinished {
            job,
            unit: target,
            result: Ok(opened),
        })
        .unwrap();
    assert!(matches!(f.result(command), CommandOutcome::Applied { .. }));
    f.incoming.send(BackendEvent::Unit(a)).unwrap();
    f.barrier("same-value-shared-unit-poll");
    let snapshot = f.handle.snapshot();
    assert!(snapshot.desired.sources["a"].muted);
    assert!(!snapshot.desired.sources["b"].muted);
    assert!(!snapshot.desired.sources["c"].muted);
    for command in f.drain() {
        if let BackendCommand::Device { unit, settings, .. } = command {
            assert!(
                unit != target || !settings.contains(&DeviceSetting::Mute(true)),
                "unrelated group reconciliation must not close B"
            );
        }
    }
}

#[test]
fn mixed_group_waits_for_exact_capture_silence_without_blocking_other_groups() {
    let (mut f, b) = mixed_group_fixture();
    let command = request_wave_open(&f);
    f.incoming
        .send(BackendEvent::Graph(MixerObservation::default()))
        .unwrap();
    f.barrier("graph-unknown");
    assert_no_wave_open(f.drain());
    f.graph(
        vec![
            capture("external_mic", None, None),
            capture("wave_mic", Some("B"), Some(true)),
        ],
        IndexMap::new(),
    );
    f.barrier("silence-unknown");
    assert_no_wave_open(f.drain());
    f.incoming
        .send(BackendEvent::Error(OperationIssue {
            target: "capture external_mic".into(),
            message: "mute failed".into(),
        }))
        .unwrap();
    f.graph(
        vec![
            capture("external_mic", None, Some(false)),
            capture("wave_mic", Some("B"), Some(true)),
        ],
        IndexMap::new(),
    );
    f.barrier("silence-failed");
    assert_no_wave_open(f.drain());
    f.apply(AppCommand::SetSourceMute {
        source: sid("other"),
        muted: false,
    });
    assert!(!f.handle.snapshot().desired.sources["other"].muted);
    assert_no_wave_open(f.drain());
    f.graph(
        vec![
            capture("external_mic", None, Some(true)),
            capture("wave_mic", Some("B"), Some(true)),
        ],
        IndexMap::new(),
    );
    f.barrier("silence-confirmed");
    let (job, target, settings) = f.device_job();
    assert_eq!(target, b.id);
    assert_eq!(settings, vec![DeviceSetting::Mute(false)]);
    let mut opened = b.state.known().unwrap().clone();
    opened.muted = false;
    f.incoming
        .send(BackendEvent::DeviceFinished {
            job,
            unit: target,
            result: Ok(opened),
        })
        .unwrap();
    assert!(matches!(f.result(command), CommandOutcome::Applied { .. }));
}

#[test]
fn mixed_group_rejects_stale_revision_and_replacement_silence_acknowledgements() {
    let (mut f, _) = mixed_group_fixture();
    let stale_revision = f.handle.snapshot().revision;
    let command = request_wave_open(&f);
    f.incoming
        .send(BackendEvent::Graph(MixerObservation {
            observation: Observation::Known(()),
            captures: vec![
                capture("external_mic", None, Some(true)),
                capture("wave_mic", Some("B"), Some(true)),
            ],
            streams: vec![],
            outputs: vec![],
            default_sink: None,
            meter_targets: vec![],
            mix_identities: IndexMap::new(),
            errors: vec![],
            silent_sources: HashMap::new(),
            revision: stale_revision,
        }))
        .unwrap();
    f.barrier("stale-silence");
    assert_no_wave_open(f.drain());
    let mut replacement = capture("external_mic", None, Some(true));
    replacement.identity.server_cookie += 1;
    f.graph(
        vec![replacement, capture("wave_mic", Some("B"), Some(true))],
        IndexMap::new(),
    );
    f.barrier("replacement-silence");
    assert!(matches!(f.result(command), CommandOutcome::Cancelled));
    assert_no_wave_open(f.drain());
    f.graph(
        vec![
            capture("external_mic", None, Some(true)),
            capture("wave_mic", Some("B"), Some(true)),
        ],
        IndexMap::new(),
    );
    f.barrier("old-identity-returned");
    assert_no_wave_open(f.drain());
}

#[test]
fn mixed_group_cancels_superseded_open_before_late_silence() {
    let (mut f, b) = mixed_group_fixture();
    let command = request_wave_open(&f);
    let superseding = f.submit(AppCommand::SetSourceMute {
        source: sid("b"),
        muted: true,
    });
    let (job, target, settings) = f.device_job();
    assert_eq!(settings, vec![DeviceSetting::Mute(true)]);
    f.incoming
        .send(BackendEvent::DeviceFinished {
            job,
            unit: target,
            result: Ok(b.state.known().unwrap().clone()),
        })
        .unwrap();
    assert!(matches!(
        f.result(superseding),
        CommandOutcome::Applied { .. }
    ));
    assert!(matches!(f.result(command), CommandOutcome::Cancelled));
    f.graph(
        vec![
            capture("external_mic", None, Some(true)),
            capture("wave_mic", Some("B"), Some(true)),
        ],
        IndexMap::new(),
    );
    f.barrier("late-silence");
    assert_no_wave_open(f.drain());
}

#[test]
fn mixed_group_newer_device_mute_wins_across_unknown_graph_and_late_silence() {
    for toggle in [false, true] {
        let (mut f, b) = mixed_group_fixture();
        let opening = request_wave_open(&f);
        f.incoming
            .send(BackendEvent::Graph(MixerObservation::default()))
            .unwrap();
        f.barrier("mute-graph-unknown");
        let superseding = f.submit(if toggle {
            AppCommand::ToggleDeviceMute { unit: b.id }
        } else {
            AppCommand::SetDeviceSetting {
                unit: b.id,
                setting: DeviceSetting::Mute(true),
                timing: EditTiming::Immediate,
            }
        });
        f.wait(|s| s.desired.sources["b"].muted);
        f.barrier("newer-device-mute");
        let commands = f.drain();
        let (job, target) = commands
            .iter()
            .find_map(|command| match command {
                BackendCommand::Device {
                    job,
                    unit,
                    settings,
                } if *unit == b.id && settings == &vec![DeviceSetting::Mute(true)] => {
                    Some((*job, *unit))
                }
                _ => None,
            })
            .expect("newer captured B mute dispatched");
        assert_no_wave_open(commands);
        f.incoming
            .send(BackendEvent::DeviceFinished {
                job,
                unit: target,
                result: Ok(b.state.known().unwrap().clone()),
            })
            .unwrap();
        f.graph(
            vec![
                capture("external_mic", None, Some(true)),
                capture("wave_mic", Some("B"), Some(true)),
            ],
            IndexMap::new(),
        );
        f.barrier("late-exact-silence-after-device-mute");
        assert!(matches!(f.result(opening), CommandOutcome::Cancelled));
        assert!(matches!(
            f.result(superseding),
            CommandOutcome::Applied { .. }
        ));
        assert_no_wave_open(f.drain());
        let snapshot = f.handle.snapshot();
        assert!(snapshot.desired.sources["a"].muted);
        assert!(snapshot.desired.sources["b"].muted);
        assert!(snapshot.desired.sources["other"].muted);
        assert_eq!(
            snapshot
                .units
                .iter()
                .find(|unit| unit.id == b.id)
                .unwrap()
                .desired_mute,
            Some(true)
        );
        let saved: Value =
            serde_json::from_slice(&std::fs::read(f.root.path().join("sources.json")).unwrap())
                .unwrap();
        assert_eq!(saved["b"]["muted"], true);
    }
}

#[test]
fn mixed_group_retirement_cancels_the_captured_open_incarnation() {
    let (mut f, b) = mixed_group_fixture();
    let command = request_wave_open(&f);
    f.incoming.send(BackendEvent::UnitRetired(b.id)).unwrap();
    f.barrier("opening-retired");
    assert!(matches!(f.result(command), CommandOutcome::Cancelled));
    let mut replacement = b;
    replacement.id.incarnation += 1;
    f.incoming.send(BackendEvent::Unit(replacement)).unwrap();
    f.graph(
        vec![
            capture("external_mic", None, Some(true)),
            capture("wave_mic", Some("B"), Some(true)),
        ],
        IndexMap::new(),
    );
    f.barrier("new-incarnation");
    assert_no_wave_open(f.drain());
}

#[test]
fn mixed_group_shutdown_cancels_held_open() {
    let (mut f, _) = mixed_group_fixture();
    let command = request_wave_open(&f);
    f.apply(AppCommand::Shutdown);
    assert!(matches!(f.result(command), CommandOutcome::Cancelled));
    assert_no_wave_open(f.drain());
}

#[test]
fn capture_open_waits_for_completed_vendor_silence_and_survives_failure() {
    let mut f = Rig::new(grouped(), json!({}), None);
    let b = unit(2, "B", false);
    f.incoming.send(BackendEvent::Unit(b.clone())).unwrap();
    f.graph(
        vec![
            capture("mic_a", None, Some(true)),
            capture("mic_b", Some("B"), Some(false)),
        ],
        IndexMap::new(),
    );
    f.barrier("reverse-bound");
    f.drain();
    let command = f.submit(AppCommand::SetSourceMute {
        source: sid("a"),
        muted: false,
    });
    let (job, target, settings) = f.device_job();
    assert_eq!(target, b.id);
    assert_eq!(settings, vec![DeviceSetting::Mute(true)]);
    let mut muted = b.clone();
    let mut state = muted.state.known().unwrap().clone();
    state.muted = true;
    muted.state = Observation::Known(state);
    f.incoming.send(BackendEvent::Unit(muted.clone())).unwrap();
    f.barrier("poll-is-not-completion");
    assert!(
        !f.drain()
            .iter()
            .any(|c| matches!(c, BackendCommand::CaptureMute { muted: false, .. }))
    );
    f.incoming
        .send(BackendEvent::DeviceFinished {
            job,
            unit: target,
            result: Err(OperationError::unavailable("silence failed")),
        })
        .unwrap();
    f.barrier("vendor-silence-failed");
    assert!(
        !f.drain()
            .iter()
            .any(|c| matches!(c, BackendCommand::CaptureMute { muted: false, .. }))
    );
    f.incoming.send(BackendEvent::Unit(muted)).unwrap();
    f.barrier("vendor-silence-reobserved");
    assert!(f.drain().iter().any(|c| matches!(c, BackendCommand::CaptureMute { node_name, muted: false, .. } if node_name == "mic_a")));
    assert!(matches!(f.result(command), CommandOutcome::Rejected(_)));
}

#[test]
fn mixed_group_rebinding_cancels_the_old_capture_binding_open() {
    let (mut f, _) = mixed_group_fixture();
    let command = request_wave_open(&f);
    f.apply(AppCommand::EditSource {
        source: sid("b"),
        changes: SourceEdit {
            node_name: Some("new_wave_mic".into()),
            ..SourceEdit::default()
        },
    });
    assert!(matches!(f.result(command), CommandOutcome::Cancelled));
    f.graph(
        vec![
            capture("external_mic", None, Some(true)),
            capture("new_wave_mic", Some("B"), Some(true)),
        ],
        IndexMap::new(),
    );
    f.barrier("rebound-silence");
    assert_no_wave_open(f.drain());
}

#[test]
fn later_nonvendor_edge_uses_group_handover_without_origin_echo() {
    let f = Rig::new(grouped(), json!({}), None);
    f.graph(
        vec![
            capture("mic_a", None, Some(false)),
            capture("mic_b", None, Some(false)),
        ],
        IndexMap::new(),
    );
    f.barrier("group-baseline");
    assert!(f.handle.snapshot().desired.sources["a"].muted);
    f.drain();
    f.graph(
        vec![
            capture("mic_a", None, Some(true)),
            capture("mic_b", None, Some(false)),
        ],
        IndexMap::new(),
    );
    f.barrier("group-external-muted");
    f.graph(
        vec![
            capture("mic_a", None, Some(false)),
            capture("mic_b", None, Some(false)),
        ],
        IndexMap::new(),
    );
    f.barrier("group-external-open");
    assert!(!f.handle.snapshot().desired.sources["a"].muted);
    assert!(f.handle.snapshot().desired.sources["b"].muted);
    let writes: Vec<_> = f
        .drain()
        .into_iter()
        .filter_map(|command| match command {
            BackendCommand::CaptureMute {
                node_name, muted, ..
            } => Some((node_name, muted)),
            BackendCommand::Routing { desired, .. } => {
                assert!(desired.sources["a"].muted);
                None
            }
            _ => None,
        })
        .collect();
    assert_eq!(writes, vec![("mic_b".into(), true)]);
    f.graph(
        vec![
            capture("mic_a", None, Some(false)),
            capture("mic_b", None, Some(true)),
        ],
        IndexMap::new(),
    );
    f.barrier("group-silenced");
    let commands = f.drain();
    assert!(
        !commands
            .iter()
            .any(|command| matches!(command, BackendCommand::CaptureMute { .. }))
    );
    assert!(commands.iter().any(|command| matches!(command, BackendCommand::Routing { desired, .. } if !desired.sources["a"].muted && desired.sources["b"].muted)));
}

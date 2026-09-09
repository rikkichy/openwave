use openwave_core::{calibration::Metrics, model::*};
use openwave_runtime::{
    calibration::CalibrationEvent,
    controller::*,
    mixer::{GraphBackend, GraphSnapshot, Mixer, MixerEvent, MixerObservation, RoutingChild},
    uninstall::{UninstallPlan, UninstallResult},
};
use serde_json::json;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

fn sid(id: &str) -> SourceId {
    SourceId::new(id).unwrap()
}
fn unavailable() -> OperationError {
    OperationError::unavailable("injected graph outage")
}
fn graph(a: bool, b: bool, serial: u32) -> GraphSnapshot {
    let objects = json!([
        {"id":0,"type":"PipeWire:Interface:Core","info":{"cookie":71}},
        {"id":10,"type":"PipeWire:Interface:Node","info":{"props":{"node.name":"mic_a","object.serial":serial,"media.class":"Audio/Source","audio.channels":1}}},
        {"id":11,"type":"PipeWire:Interface:Node","info":{"props":{"node.name":"mic_b","object.serial":200,"media.class":"Audio/Source","audio.channels":1}}}
    ]);
    GraphSnapshot::parse(
        &objects,
        &json!([]),
        &json!([
            {"name":"mic_a","mute":a,"properties":{"object.serial":serial}},
            {"name":"mic_b","mute":b,"properties":{"object.serial":200}}
        ]),
        &json!([]),
        &json!([]),
        None,
    )
    .unwrap()
}

struct ScriptedGraph {
    samples: mpsc::Receiver<Result<GraphSnapshot>>,
    last: GraphSnapshot,
    attempts: mpsc::Sender<(NodeIdentity, bool)>,
    fail: Arc<AtomicBool>,
}
impl GraphBackend for ScriptedGraph {
    fn snapshot(&mut self) -> Result<GraphSnapshot> {
        match self.samples.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(graph)) => {
                self.last = graph.clone();
                Ok(graph)
            }
            Ok(Err(error)) => Err(error),
            Err(mpsc::RecvTimeoutError::Disconnected) => Ok(self.last.clone()),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(unavailable()),
        }
    }
    fn write_definitions(&mut self, mixes: &Mixes) -> Result<()> {
        assert!(mixes.is_empty());
        Ok(())
    }
    fn set_capture_mute(&mut self, capture: &CaptureSnapshot, muted: bool) -> Result<()> {
        self.attempts
            .send((capture.identity.clone(), muted))
            .unwrap();
        if self.fail.load(Ordering::SeqCst) {
            Err(OperationError::unavailable(
                "injected capture write failure",
            ))
        } else {
            Ok(())
        }
    }
    fn create_sink(&mut self, _: &str, _: &str, _: &str) -> Result<u32> {
        panic!("unexpected sink")
    }
    fn unload_module(&mut self, _: u32) -> Result<()> {
        panic!("unexpected unload")
    }
    fn destroy_node(&mut self, _: u32) -> Result<()> {
        panic!("unexpected destroy")
    }
    fn move_stream(&mut self, _: &StreamSnapshot, _: &str) -> Result<()> {
        panic!("unexpected move")
    }
    fn link(&mut self, _: u32, _: u32) -> Result<()> {
        panic!("unexpected link")
    }
    fn set_level(&mut self, _: u32, _: f64, _: bool) -> Result<()> {
        panic!("unexpected level")
    }
    fn spawn_loopback(
        &mut self,
        _: &str,
        _: &str,
        _: Option<&str>,
    ) -> Result<Box<dyn RoutingChild>> {
        panic!("unexpected loopback")
    }
    fn spawn_filter(&mut self, _: &Path) -> Result<Box<dyn RoutingChild>> {
        panic!("unexpected filter")
    }
}
struct GraphRig {
    mixer: Mixer,
    samples: Option<mpsc::Sender<Result<GraphSnapshot>>>,
    events: mpsc::Receiver<MixerEvent>,
    attempts: mpsc::Receiver<(NodeIdentity, bool)>,
    fail: Arc<AtomicBool>,
}
impl GraphRig {
    fn new() -> Self {
        let (samples, receiver) = mpsc::channel();
        let (send, attempts) = mpsc::channel();
        let fail = Arc::new(AtomicBool::new(false));
        let backend = ScriptedGraph {
            samples: receiver,
            last: graph(false, true, 100),
            attempts: send,
            fail: fail.clone(),
        };
        let (mixer, events) = Mixer::start_with_backend(
            Box::new(backend),
            Default::default(),
            Duration::from_millis(1),
        )
        .unwrap();
        Self {
            mixer,
            samples: Some(samples),
            events,
            attempts,
            fail,
        }
    }
    fn dispatch(&self, commands: Vec<BackendCommand>) {
        for command in commands {
            match command {
                BackendCommand::Routing {
                    revision,
                    desired,
                    bindings,
                } => self.mixer.set_desired(revision, desired, bindings).unwrap(),
                BackendCommand::CaptureMute {
                    node_name,
                    binding,
                    muted,
                } => self
                    .mixer
                    .set_capture_mute(node_name, binding, muted)
                    .unwrap(),
                BackendCommand::MeterTargets(_) => {}
                _ => panic!("unexpected controller operation"),
            }
        }
    }
    fn sample(&self, sample: Result<GraphSnapshot>) -> MixerObservation {
        self.samples.as_ref().unwrap().send(sample).unwrap();
        match self.events.recv_timeout(Duration::from_secs(5)).unwrap() {
            MixerEvent::Observed(observation) => observation,
            _ => panic!("unexpected master observation"),
        }
    }
}
impl Drop for GraphRig {
    fn drop(&mut self) {
        self.samples.take();
        self.mixer.stop().unwrap();
    }
}
struct FixtureBackend {
    root: PathBuf,
    commands: mpsc::Sender<BackendCommand>,
    events: mpsc::Receiver<BackendEvent>,
}
impl Backend for FixtureBackend {
    fn identity(&self) -> &Path {
        &self.root
    }
    fn dispatch(&mut self, command: BackendCommand) -> Result<()> {
        self.commands.send(command).map_err(|_| unavailable())
    }
    fn next_event(&mut self) -> Option<BackendEvent> {
        self.events.try_recv().ok()
    }
    fn shutdown(&mut self) -> std::result::Result<(), ShutdownError> {
        Ok(())
    }
    fn prepare_removal(&mut self, _: &UninstallPlan, _: bool) -> Result<()> {
        panic!("unexpected removal")
    }
    fn remove(&mut self, _: &UninstallPlan, _: bool) -> Result<UninstallResult> {
        panic!("unexpected removal")
    }
}
struct ControllerRig {
    root: tempfile::TempDir,
    handle: RuntimeHandle,
    incoming: mpsc::Sender<BackendEvent>,
    commands: mpsc::Receiver<BackendCommand>,
    completed: mpsc::Receiver<(CommandId, CommandOutcome)>,
    saved: HashMap<CommandId, CommandOutcome>,
    pump: Option<thread::JoinHandle<()>>,
    sequence: u64,
}
impl ControllerRig {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("sources.json"),
            json!({
                "a":{"kind":"device","node_name":"mic_a","muted":false,"group":"Mics"},
                "b":{"kind":"device","node_name":"mic_b","muted":true,"group":"Mics"}
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(root.path().join("mixdefs.json"), "{}").unwrap();
        let (incoming, events) = mpsc::channel();
        let (send, commands) = mpsc::channel();
        let identity = root.path().to_owned();
        let (handle, notifications) = RuntimeHandle::start_with(identity.clone(), move || {
            Ok(Box::new(FixtureBackend {
                root: identity,
                commands: send,
                events,
            }))
        })
        .unwrap();
        let (send, completed) = mpsc::channel();
        let pump = thread::spawn(move || {
            let context = glib::MainContext::new();
            while let Ok(event) = context.block_on(notifications.recv()) {
                if let RuntimeEvent::CommandFinished { id, result } = event {
                    if send.send((id, result)).is_err() {
                        break;
                    }
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
            sequence: 0,
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
    fn barrier(&mut self) {
        self.sequence += 1;
        let label = format!("barrier {}", self.sequence);
        self.incoming
            .send(BackendEvent::Status {
                service: label.clone(),
                setup_required: false,
            })
            .unwrap();
        self.wait(|s| s.service_status == label);
    }
    fn observe(&mut self, observation: MixerObservation) {
        self.incoming
            .send(BackendEvent::Graph(observation))
            .unwrap();
        self.barrier();
    }
    fn fresh(&mut self, captures: Vec<CaptureSnapshot>) {
        self.observe(MixerObservation {
            observation: Observation::Known(()),
            captures,
            revision: self.handle.snapshot().revision,
            ..Default::default()
        });
    }
    fn outcome(&mut self, id: CommandId) -> CommandOutcome {
        if let Some(result) = self.saved.remove(&id) {
            return result;
        }
        loop {
            let (next, result) = self.completed.recv_timeout(Duration::from_secs(5)).unwrap();
            if next == id {
                return result;
            }
            assert!(self.saved.insert(next, result).is_none());
        }
    }
    fn apply(&mut self, command: AppCommand) {
        let id = self.handle.submit(command).unwrap();
        assert!(matches!(self.outcome(id), CommandOutcome::Applied { .. }));
    }
    fn reject(&mut self, command: AppCommand) {
        let id = self.handle.submit(command).unwrap();
        assert!(matches!(self.outcome(id), CommandOutcome::Rejected(_)));
    }
    fn commands(&self) -> Vec<BackendCommand> {
        self.commands.try_iter().collect()
    }
    fn rows(&self, a: bool, b: bool) {
        let snapshot = self.handle.snapshot();
        assert_eq!(snapshot.desired.sources["a"].muted, a);
        assert_eq!(snapshot.desired.sources["b"].muted, b);
    }
    fn record(&mut self, token: &CalibrationToken, speech: bool) {
        let id = self
            .handle
            .submit(if speech {
                AppCommand::RecordSpeech {
                    token: token.clone(),
                }
            } else {
                AppCommand::RecordNoise {
                    token: token.clone(),
                }
            })
            .unwrap();
        loop {
            if matches!(
                self.commands.recv_timeout(Duration::from_secs(5)).unwrap(),
                BackendCommand::RecordCalibration { .. }
            ) {
                break;
            }
        }
        self.incoming
            .send(BackendEvent::Calibration(CalibrationEvent {
                token: token.clone(),
                result: Ok(Metrics {
                    peaks_db: vec![if speech { -20.0 } else { -60.0 }; 60],
                    balance: 0.01,
                    sub_db: -2.0,
                    voice_low_db: -20.0,
                    tilt_db: -30.0,
                }),
            }))
            .unwrap();
        assert!(matches!(self.outcome(id), CommandOutcome::Applied { .. }));
    }
}
impl Drop for ControllerRig {
    fn drop(&mut self) {
        let _ = self.handle.submit(AppCommand::Shutdown);
        self.handle.wait_stopped().unwrap();
        if let Some(pump) = self.pump.take() {
            pump.join().unwrap();
        }
    }
}

#[test]
fn group_write_sample_cannot_undo_intent_and_later_confirmation_releases_feedback() {
    let mut controller = ControllerRig::new();
    let mixer = GraphRig::new();
    mixer.dispatch(controller.commands());
    controller.observe(mixer.sample(Ok(graph(false, true, 100))));
    let switching = controller
        .handle
        .submit(AppCommand::SwitchGroup {
            group: "Mics".into(),
        })
        .unwrap();
    controller.wait(|snapshot| !snapshot.desired.sources["b"].muted);
    controller.barrier();
    controller.rows(true, false);
    // A routing observation can be queued before the subsequent capture request is
    // admitted, even with the current revision. The controller must fence it too.
    controller.fresh(graph(false, true, 100).captures);
    controller.rows(true, false);
    mixer.dispatch(controller.commands());
    let old = mixer.sample(Ok(graph(false, true, 100)));
    assert!(old.captures[0].muted.known().is_none());
    assert_eq!(old.captures[1].muted.known(), Some(&true));
    controller.observe(old);
    controller.rows(true, false);
    let writes: Vec<_> = mixer.attempts.try_iter().collect();
    assert_eq!(writes.len(), 1);
    assert!(
        writes[0].1,
        "only the departing capture may be silenced before acknowledgement"
    );
    controller.observe(mixer.sample(Ok(graph(true, true, 100))));
    mixer.dispatch(controller.commands());
    assert!(matches!(
        controller.outcome(switching),
        CommandOutcome::Applied { .. }
    ));
    controller.observe(mixer.sample(Ok(graph(true, true, 100))));
    let writes: Vec<_> = mixer.attempts.try_iter().collect();
    assert_eq!(writes.len(), 1);
    assert!(
        !writes[0].1,
        "the arriving capture opens only after silence acknowledgement"
    );
    controller.observe(mixer.sample(Ok(graph(true, false, 100))));
    assert!(
        controller
            .handle
            .snapshot()
            .captures
            .iter()
            .all(|capture| capture.muted.known().is_some())
    );
    controller.rows(true, false);
    assert!(mixer.attempts.try_recv().is_err());
    let mut external = graph(false, false, 100).captures;
    external[1].muted = Observation::Unknown(unavailable());
    controller.fresh(external);
    controller.rows(false, true);
    let writes: Vec<_> = controller
        .commands()
        .into_iter()
        .filter_map(|command| match command {
            BackendCommand::CaptureMute {
                node_name, muted, ..
            } => Some((node_name, muted)),
            _ => None,
        })
        .collect();
    assert_eq!(
        writes,
        vec![("mic_b".into(), true)],
        "external origin is not echoed but its peer is silenced"
    );
}

#[test]
fn failed_capture_writes_and_graph_outage_preserve_intent_until_replacement_confirms() {
    let mut controller = ControllerRig::new();
    let mixer = GraphRig::new();
    mixer.dispatch(controller.commands());
    controller.observe(mixer.sample(Ok(graph(false, true, 100))));
    controller.apply(AppCommand::SetSourceMute {
        source: sid("a"),
        muted: true,
    });
    mixer.fail.store(true, Ordering::SeqCst);
    mixer.dispatch(controller.commands());
    for _ in 0..2 {
        let failed = mixer.sample(Ok(graph(false, true, 100)));
        assert!(failed.captures[0].muted.known().is_none());
        assert!(failed.errors.iter().any(|issue| issue.target == "mic_a"));
        controller.observe(failed);
        controller.rows(true, true);
    }
    let unknown = mixer.sample(Err(unavailable()));
    assert!(unknown.observation.known().is_none());
    assert_eq!(unknown.captures[0].node_name, "mic_a");
    controller.observe(unknown);
    controller.rows(true, true);
    assert_eq!(
        controller.handle.snapshot().captures[0]
            .identity
            .object_serial,
        "100"
    );
    assert_eq!(mixer.attempts.try_iter().count(), 2);
    mixer.fail.store(false, Ordering::SeqCst);
    let mut absent = graph(false, true, 100);
    absent.captures.clear();
    controller.observe(mixer.sample(Ok(absent)));
    controller.observe(mixer.sample(Ok(graph(false, true, 101))));
    controller.rows(true, true);
    assert!(
        controller.handle.snapshot().captures[0]
            .muted
            .known()
            .is_none()
    );
    controller.observe(mixer.sample(Ok(graph(true, true, 101))));
    assert_eq!(
        controller.handle.snapshot().captures[0].muted.known(),
        Some(&true)
    );
    let attempts: Vec<_> = mixer.attempts.try_iter().collect();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].0.object_serial, "101");
}

#[test]
fn replacing_binding_ownership_discards_unconfirmed_capture_intent() {
    let mut controller = ControllerRig::new();
    let mixer = GraphRig::new();
    mixer.dispatch(controller.commands());
    controller.observe(mixer.sample(Ok(graph(false, true, 100))));
    controller.apply(AppCommand::SetSourceMute {
        source: sid("a"),
        muted: true,
    });
    mixer.dispatch(controller.commands());
    controller.observe(mixer.sample(Ok(graph(false, true, 100))));
    assert_eq!(mixer.attempts.try_iter().count(), 1);
    controller.apply(AppCommand::EditSource {
        source: sid("a"),
        changes: SourceEdit {
            node_name: Some("other_microphone".into()),
            ..Default::default()
        },
    });
    controller.apply(AppCommand::EditSource {
        source: sid("a"),
        changes: SourceEdit {
            node_name: Some("mic_a".into()),
            ..Default::default()
        },
    });
    mixer.dispatch(controller.commands());
    controller.observe(mixer.sample(Ok(graph(false, true, 100))));
    controller.observe(mixer.sample(Ok(graph(false, true, 100))));
    controller.rows(true, true);
    assert_eq!(
        controller.handle.snapshot().captures[0].muted.known(),
        Some(&false)
    );
    assert!(mixer.attempts.try_recv().is_err());
}

#[test]
fn vendor_retirement_preserves_software_mute_until_a_deliberate_capture_edge() {
    use openwave_core::{
        profiles::ProfileId,
        protocol::{ConfigBuffer, DeviceInfo},
    };
    let mut controller = ControllerRig::new();
    let mut captures = graph(false, true, 100).captures;
    captures[0]
        .properties
        .insert("device.serial".into(), json!("A"));
    captures[1].muted = Observation::Unknown(unavailable());
    controller.fresh(captures.clone());
    let profile = ProfileId::WaveXlr;
    let mut state = ConfigBuffer::decode(profile, &vec![0; profile.profile().config_len])
        .unwrap()
        .state();
    state.muted = true;
    let unit = UnitSnapshot {
        id: UnitId {
            profile,
            bus: 1,
            address: 1,
            incarnation: 1,
        },
        info: DeviceInfo {
            serial: "A".into(),
            api: "1".into(),
            firmware: "1".into(),
        },
        state: Observation::Known(state),
        desired_mute: None,
        input_peak: 0.0,
        output_peak: 0.0,
        errors: Vec::new(),
    };
    controller
        .incoming
        .send(BackendEvent::Unit(unit.clone()))
        .unwrap();
    controller.barrier();
    controller.rows(true, true);
    controller.observe(MixerObservation::default());
    controller
        .incoming
        .send(BackendEvent::UnitRetired(unit.id))
        .unwrap();
    controller.barrier();
    assert!(!controller.commands().iter().any(|command| matches!(
        command,
        BackendCommand::CaptureMute { .. } | BackendCommand::Device { .. }
    )));
    // Losing vendor ownership is not an external unmute gesture. Preserve the
    // software mute until the nonvendor capture actually changes its baseline.
    controller.fresh(captures.clone());
    controller.rows(true, true);
    captures[0].muted = Observation::Known(true);
    controller.fresh(captures.clone());
    controller.rows(true, true);
    captures[0].muted = Observation::Known(false);
    let peer_node = captures[1].node_name.clone();
    controller.fresh(captures);
    controller.rows(false, true);
    // Do not echo the originating unmute. The unknown peer still needs an
    // explicit silence request before this group's routing can open.
    let requests: Vec<_> = controller
        .commands()
        .into_iter()
        .filter_map(|command| match command {
            BackendCommand::CaptureMute {
                node_name, muted, ..
            } => Some((node_name, muted)),
            _ => None,
        })
        .collect();
    assert_eq!(requests, vec![(peer_node, true)]);
}

#[test]
fn unknown_graph_expires_review_and_recording_without_writing_settings() {
    let mut controller = ControllerRig::new();
    controller.fresh(graph(false, true, 100).captures);
    controller.apply(AppCommand::StartCalibration { source: sid("a") });
    let token = controller
        .handle
        .snapshot()
        .calibration
        .as_ref()
        .unwrap()
        .token
        .clone();
    controller.record(&token, false);
    controller.record(&token, true);
    assert!(matches!(
        controller
            .handle
            .snapshot()
            .calibration
            .as_ref()
            .unwrap()
            .phase,
        CalibrationPhase::Review { .. }
    ));
    let before = std::fs::read(controller.root.path().join("sources.json")).unwrap();
    controller.observe(MixerObservation::default());
    assert!(matches!(
        controller
            .handle
            .snapshot()
            .calibration
            .as_ref()
            .unwrap()
            .phase,
        CalibrationPhase::Expired(_)
    ));
    assert_eq!(controller.handle.snapshot().captures[0].node_name, "mic_a");
    controller.reject(AppCommand::AcceptCalibration {
        token: token.clone(),
    });
    controller.reject(AppCommand::StartCalibration { source: sid("a") });
    controller.fresh(graph(false, true, 100).captures);
    controller.reject(AppCommand::AcceptCalibration { token });
    controller.apply(AppCommand::StartCalibration { source: sid("a") });
    let token = controller
        .handle
        .snapshot()
        .calibration
        .as_ref()
        .unwrap()
        .token
        .clone();
    let recording = controller
        .handle
        .submit(AppCommand::RecordNoise {
            token: token.clone(),
        })
        .unwrap();
    controller.wait(|s| {
        matches!(
            s.calibration.as_ref().unwrap().phase,
            CalibrationPhase::RecordingNoise
        )
    });
    controller.observe(MixerObservation::default());
    assert!(matches!(
        controller.outcome(recording),
        CommandOutcome::Rejected(_)
    ));
    controller
        .incoming
        .send(BackendEvent::Calibration(CalibrationEvent {
            token: token.clone(),
            result: Ok(Metrics {
                peaks_db: vec![-60.0; 60],
                balance: 0.01,
                sub_db: -2.0,
                voice_low_db: -20.0,
                tilt_db: -30.0,
            }),
        }))
        .unwrap();
    controller.barrier();
    controller.reject(AppCommand::RecordSpeech { token });
    assert_eq!(
        std::fs::read(controller.root.path().join("sources.json")).unwrap(),
        before
    );
}

#[test]
fn unknown_graph_cannot_infer_vendor_binding_or_offer_cached_capture_rows() {
    use openwave_core::{
        profiles::ProfileId,
        protocol::{ConfigBuffer, DeviceInfo},
    };
    let mut controller = ControllerRig::new();
    let profile = ProfileId::WaveXlr;
    let mut state = ConfigBuffer::decode(profile, &vec![0; profile.profile().config_len])
        .unwrap()
        .state();
    state.muted = true;
    let mut unit = UnitSnapshot {
        id: UnitId {
            profile,
            bus: 1,
            address: 1,
            incarnation: 1,
        },
        info: DeviceInfo {
            serial: "A".into(),
            api: "1".into(),
            firmware: "1".into(),
        },
        state: Observation::Known(state),
        desired_mute: None,
        input_peak: 0.0,
        output_peak: 0.0,
        errors: Vec::new(),
    };
    let mut captures = graph(false, true, 100).captures;
    captures[0]
        .properties
        .insert("device.serial".into(), json!("A"));
    controller
        .incoming
        .send(BackendEvent::Unit(unit.clone()))
        .unwrap();
    controller.fresh(captures.clone());
    controller
        .incoming
        .send(BackendEvent::Unit(unit.clone()))
        .unwrap();
    controller.barrier();
    controller.rows(true, true);
    captures[0].node_name =
        "alsa_input.usb-Elgato_Systems_Elgato_Wave_XLR_cached.analog-stereo".into();
    controller.observe(MixerObservation {
        captures,
        ..Default::default()
    });
    if let Observation::Known(state) = &mut unit.state {
        state.muted = false;
    }
    controller
        .incoming
        .send(BackendEvent::Unit(unit.clone()))
        .unwrap();
    controller.barrier();
    controller.rows(true, true);
    let snapshot = controller.handle.snapshot();
    assert_eq!(snapshot.captures[0].node_name, "mic_a");
    assert_eq!(
        snapshot
            .desired
            .sources
            .keys()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        vec!["a", "b"]
    );
    assert_eq!(snapshot.units[0].desired_mute, None);
    controller.commands();
    let id = controller
        .handle
        .submit(AppCommand::SetDeviceSetting {
            unit: unit.id,
            setting: DeviceSetting::Mute(true),
            timing: EditTiming::Immediate,
        })
        .unwrap();
    let job = loop {
        if let BackendCommand::Device {
            job,
            unit: actual,
            settings,
        } = controller
            .commands
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
        {
            assert_eq!(actual, unit.id);
            assert_eq!(settings, vec![DeviceSetting::Mute(true)]);
            break job;
        }
    };
    let mut state = unit.state.known().unwrap().clone();
    state.muted = true;
    controller
        .incoming
        .send(BackendEvent::DeviceFinished {
            job,
            unit: unit.id,
            result: Ok(state),
        })
        .unwrap();
    assert!(matches!(
        controller.outcome(id),
        CommandOutcome::Applied { .. }
    ));
}

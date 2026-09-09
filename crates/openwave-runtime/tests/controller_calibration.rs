use openwave_core::{calibration::Metrics, effects::FxSettings, model::*};
use openwave_runtime::{
    calibration::CalibrationEvent,
    controller::{
        AppCommand, Backend, BackendCommand, BackendEvent, EditTiming, RuntimeEvent, RuntimeEvents,
        RuntimeHandle, ShutdownError,
    },
    mixer::MixerObservation,
    uninstall::{UninstallPlan, UninstallResult},
};
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    path::{Path, PathBuf},
    pin::pin,
    sync::{Arc, mpsc},
    task::{Context, Poll, Wake, Waker},
    thread,
    time::{Duration, Instant},
};

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
        self.commands
            .send(command)
            .map_err(|_| OperationError::unavailable("fixture receiver closed"))
    }
    fn next_event(&mut self) -> Option<BackendEvent> {
        self.events.try_recv().ok()
    }
    fn shutdown(&mut self) -> std::result::Result<(), ShutdownError> {
        Ok(())
    }
    fn prepare_removal(&mut self, _: &UninstallPlan, _: bool) -> Result<()> {
        panic!("calibration cannot prepare removal")
    }
    fn remove(&mut self, _: &UninstallPlan, _: bool) -> Result<UninstallResult> {
        panic!("calibration cannot remove an installation")
    }
}
struct ThreadWake(thread::Thread);
impl Wake for ThreadWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

struct Fixture {
    root: tempfile::TempDir,
    handle: RuntimeHandle,
    events: RuntimeEvents,
    incoming: mpsc::Sender<BackendEvent>,
    commands: mpsc::Receiver<BackendCommand>,
    outcomes: HashMap<CommandId, CommandOutcome>,
    completed: HashSet<CommandId>,
    source: SourceId,
    capture: CaptureSnapshot,
}
impl Fixture {
    fn new(channels: Option<u32>) -> Self {
        let root = tempfile::tempdir().unwrap();
        let mut row = Source::new("Fixture microphone".into(), SourceKind::Device);
        row.id = SourceId::new("mic").unwrap();
        row.node_name = "fixture_raw".into();
        row.extra.insert("channels".into(), serde_json::json!(8)); // Persisted metadata must not determine capture format.
        row.fx = Some(FxSettings {
            eq_low: 2.0,
            eq_mid: -1.0,
            delay_ms: 42.0,
            ..FxSettings::default()
        });
        let sources: Sources = [(row.id.clone(), row.clone())].into_iter().collect();
        std::fs::write(
            root.path().join("sources.json"),
            serde_json::to_vec(&sources).unwrap(),
        )
        .unwrap();
        let capture = CaptureSnapshot {
            identity: NodeIdentity {
                server_cookie: 71,
                object_serial: "1234".into(),
            },
            node_id: 9,
            node_name: row.node_name.clone(),
            name: row.name,
            muted: Observation::Known(false),
            channels,
            properties: serde_json::json!({"object.serial":1234, "media.class":"Audio/Source"})
                .as_object()
                .unwrap()
                .clone(),
        };
        let (send_commands, commands) = mpsc::channel();
        let (incoming, receive_events) = mpsc::channel();
        let identity = root.path().to_owned();
        let (handle, events) = RuntimeHandle::start_with(root.path().to_owned(), move || {
            Ok(Box::new(FixtureBackend {
                root: identity,
                commands: send_commands,
                events: receive_events,
            }))
        })
        .unwrap();
        let fixture = Self {
            root,
            handle,
            events,
            incoming,
            commands,
            outcomes: HashMap::new(),
            completed: HashSet::new(),
            source: row.id,
            capture,
        };
        fixture.graph(fixture.capture.clone());
        fixture.until(|snapshot| snapshot.captures.len() == 1);
        fixture
    }
    fn graph(&self, capture: CaptureSnapshot) {
        self.incoming
            .send(BackendEvent::Graph(MixerObservation {
                observation: Observation::Known(()),
                captures: vec![capture],
                streams: Vec::new(),
                outputs: Vec::new(),
                default_sink: None,
                meter_targets: Vec::new(),
                errors: Vec::new(),
                mix_identities: Default::default(),
                silent_sources: Default::default(),
                revision: self.handle.snapshot().revision,
            }))
            .unwrap();
    }
    fn until(&self, predicate: impl Fn(&AppSnapshot) -> bool) -> Arc<AppSnapshot> {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let snapshot = self.handle.snapshot();
            if predicate(&snapshot) {
                return snapshot;
            }
            assert!(
                Instant::now() < deadline,
                "controller snapshot did not reach expected state"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }
    fn outcome(&mut self, id: CommandId) -> CommandOutcome {
        if let Some(outcome) = self.outcomes.remove(&id) {
            return outcome;
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let event = {
                let future = self.events.recv();
                let mut future = pin!(future);
                let waker = Waker::from(Arc::new(ThreadWake(thread::current())));
                let mut context = Context::from_waker(&waker);
                loop {
                    if let Poll::Ready(event) = future.as_mut().poll(&mut context) {
                        break event.unwrap();
                    }
                    assert!(Instant::now() < deadline, "command {id:?} did not complete");
                    thread::park_timeout(Duration::from_millis(5));
                }
            };
            if let RuntimeEvent::CommandFinished {
                id: completed,
                result,
            } = event
            {
                assert!(
                    self.completed.insert(completed),
                    "command completed more than once"
                );
                if completed == id {
                    return result;
                }
                self.outcomes.insert(completed, result);
            }
        }
    }
    fn submit(&mut self, command: AppCommand) -> CommandOutcome {
        let id = self.handle.submit(command).unwrap();
        self.outcome(id)
    }
    fn start(&mut self) -> CalibrationToken {
        assert!(matches!(
            self.submit(AppCommand::StartCalibration {
                source: self.source.clone()
            }),
            CommandOutcome::Applied { .. }
        ));
        self.until(|snapshot| {
            matches!(
                snapshot.calibration.as_ref().map(|s| &s.phase),
                Some(CalibrationPhase::NoiseReady)
            )
        })
        .calibration
        .as_ref()
        .unwrap()
        .token
        .clone()
    }
    fn recording(&self, token: &CalibrationToken, speech: bool) -> CommandId {
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
            match self.commands.recv_timeout(Duration::from_secs(3)).unwrap() {
                BackendCommand::RecordCalibration {
                    token: actual,
                    seconds,
                } => {
                    assert_eq!(&actual, token);
                    assert_eq!(seconds, if speech { 5 } else { 3 });
                    return id;
                }
                BackendCommand::Device { .. } => panic!("calibration submitted hardware settings"),
                _ => {}
            }
        }
    }
    fn complete(&self, token: &CalibrationToken, metrics: Metrics) {
        self.incoming
            .send(BackendEvent::Calibration(CalibrationEvent {
                token: token.clone(),
                result: Ok(metrics),
            }))
            .unwrap();
    }
    fn noise(&mut self, token: &CalibrationToken) {
        let id = self.recording(token, false);
        self.complete(token, metrics(-60.0));
        assert!(matches!(self.outcome(id), CommandOutcome::Applied { .. }));
        self.until(|snapshot| {
            matches!(
                snapshot.calibration.as_ref().map(|s| &s.phase),
                Some(CalibrationPhase::SpeechReady)
            )
        });
    }
    fn review(&mut self, token: &CalibrationToken) -> FxSettings {
        self.noise(token);
        let id = self.recording(token, true);
        self.complete(token, metrics(-20.0));
        assert!(matches!(self.outcome(id), CommandOutcome::Applied { .. }));
        let snapshot = self.until(|snapshot| {
            matches!(
                snapshot.calibration.as_ref().map(|s| &s.phase),
                Some(CalibrationPhase::Review { .. })
            )
        });
        match &snapshot.calibration.as_ref().unwrap().phase {
            CalibrationPhase::Review { proposal, .. } => proposal.clone(),
            _ => unreachable!(),
        }
    }
    fn bytes(&self) -> Vec<u8> {
        std::fs::read(self.root.path().join("sources.json")).unwrap()
    }
    fn expired(&self) {
        let snapshot = self.until(|snapshot| {
            matches!(
                snapshot.calibration.as_ref().map(|s| &s.phase),
                Some(CalibrationPhase::Expired(_))
            )
        });
        assert!(
            snapshot
                .errors
                .iter()
                .any(|issue| issue.target == "calibration")
        );
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.handle.submit(AppCommand::Shutdown);
        self.handle.wait_stopped().unwrap();
    }
}
fn metrics(peak: f64) -> Metrics {
    Metrics {
        peaks_db: vec![peak; 60],
        balance: 0.01,
        sub_db: -2.0,
        voice_low_db: -20.0,
        tilt_db: -30.0,
    }
}

#[test]
fn staged_review_applies_only_proposed_controls_to_latest_fx() {
    let mut fixture = Fixture::new(Some(1));
    let before = fixture.bytes();
    let token = fixture.start();
    assert_eq!(token.channels, 1);
    let proposal = fixture.review(&token);
    assert_eq!(
        fixture.bytes(),
        before,
        "recording and review must not persist settings"
    );
    assert_eq!(proposal.gate_thresh, -52.0);
    assert_eq!(proposal.comp_thresh, -26.0);
    assert_eq!(proposal.comp_ratio, 3.0);
    assert_eq!(proposal.lowcut, 120);
    assert_eq!(proposal.eq_high, 4.0);
    assert!(proposal.gate && proposal.comp && proposal.mono);
    let mut current = fixture.handle.snapshot().desired.sources[&fixture.source]
        .fx
        .clone()
        .unwrap();
    current.delay_ms = 123.0;
    current.eq_low = -3.0;
    assert!(matches!(
        fixture.submit(AppCommand::SetFx {
            source: fixture.source.clone(),
            settings: current,
            timing: EditTiming::Immediate
        }),
        CommandOutcome::Applied { .. }
    ));
    assert!(matches!(
        fixture.submit(AppCommand::AcceptCalibration { token }),
        CommandOutcome::Applied { .. }
    ));
    let snapshot = fixture.until(|snapshot| snapshot.calibration.is_none());
    let actual = snapshot.desired.sources[&fixture.source]
        .fx
        .as_ref()
        .unwrap();
    let mut expected = proposal;
    expected.delay_ms = 123.0;
    expected.eq_low = -3.0;
    assert_eq!(actual, &expected);
    let saved: serde_json::Value = serde_json::from_slice(&fixture.bytes()).unwrap();
    assert_eq!(saved["mic"]["fx"], serde_json::to_value(actual).unwrap());
}

#[test]
fn phases_refuse_out_of_order_requests_without_starting_extra_recordings() {
    let mut fixture = Fixture::new(None);
    let before = fixture.bytes();
    let token = fixture.start();
    assert_eq!(
        token.channels, 2,
        "unknown current channels use stereo, not persisted channels"
    );
    assert!(matches!(
        fixture.submit(AppCommand::RecordSpeech {
            token: token.clone()
        }),
        CommandOutcome::Rejected(_)
    ));
    assert!(matches!(
        fixture.submit(AppCommand::AcceptCalibration {
            token: token.clone()
        }),
        CommandOutcome::Rejected(_)
    ));
    let recording = fixture.recording(&token, false);
    assert!(matches!(
        fixture.submit(AppCommand::RecordNoise {
            token: token.clone()
        }),
        CommandOutcome::Rejected(_)
    ));
    assert!(
        !fixture.outcomes.contains_key(&recording),
        "record command completed before worker result"
    );
    fixture.complete(&token, metrics(-60.0));
    assert!(matches!(
        fixture.outcome(recording),
        CommandOutcome::Applied { .. }
    ));
    assert!(matches!(
        fixture.submit(AppCommand::RecordNoise {
            token: token.clone()
        }),
        CommandOutcome::Rejected(_)
    ));
    assert!(matches!(
        fixture.submit(AppCommand::AcceptCalibration { token }),
        CommandOutcome::Rejected(_)
    ));
    assert!(!fixture.commands.try_iter().any(|command| matches!(
        command,
        BackendCommand::RecordCalibration { .. } | BackendCommand::Device { .. }
    )));
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn cancellation_and_stale_callbacks_cannot_affect_replacement_session() {
    let mut fixture = Fixture::new(Some(2));
    let before = fixture.bytes();
    let old = fixture.start();
    let old_recording = fixture.recording(&old, false);
    let new = fixture.start();
    assert_ne!(old.session, new.session);
    assert_eq!(fixture.outcome(old_recording), CommandOutcome::Cancelled);
    let new_recording = fixture.recording(&new, false);
    fixture.complete(&old, metrics(-60.0));
    assert!(matches!(
        fixture.submit(AppCommand::CancelCalibration { token: old }),
        CommandOutcome::Rejected(_)
    ));
    assert!(!fixture.outcomes.contains_key(&new_recording));
    assert!(matches!(
        fixture.submit(AppCommand::CancelCalibration { token: new.clone() }),
        CommandOutcome::Applied { .. }
    ));
    assert_eq!(fixture.outcome(new_recording), CommandOutcome::Cancelled);
    fixture.complete(&new, metrics(-60.0));
    let third = fixture.start(); // Completion barrier also detects duplicate completions from late callbacks.
    assert_ne!(third.session, new.session);
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn server_generation_change_rejects_inflight_recording_and_late_result() {
    let mut fixture = Fixture::new(Some(2));
    let before = fixture.bytes();
    let token = fixture.start();
    let command = fixture.recording(&token, false);
    let mut changed = fixture.capture.clone();
    changed.identity.server_cookie += 1;
    fixture.graph(changed);
    fixture.expired();
    assert!(matches!(
        fixture.outcome(command),
        CommandOutcome::Rejected(OperationError {
            code: ErrorCode::Identity,
            ..
        })
    ));
    fixture.complete(&token, metrics(-60.0));
    assert!(matches!(
        fixture.submit(AppCommand::RecordSpeech { token }),
        CommandOutcome::Rejected(_)
    ));
    fixture.expired();
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn current_channels_change_expires_between_recording_phases() {
    let mut fixture = Fixture::new(Some(1));
    let before = fixture.bytes();
    let token = fixture.start();
    fixture.noise(&token);
    let mut changed = fixture.capture.clone();
    changed.channels = Some(2);
    fixture.graph(changed);
    fixture.expired();
    assert!(matches!(
        fixture.submit(AppCommand::RecordSpeech { token }),
        CommandOutcome::Rejected(_)
    ));
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn recreated_raw_node_cannot_accept_review_from_previous_serial() {
    let mut fixture = Fixture::new(Some(2));
    let before = fixture.bytes();
    let token = fixture.start();
    fixture.review(&token);
    let mut changed = fixture.capture.clone();
    changed.identity.object_serial = "5678".into();
    changed
        .properties
        .insert("object.serial".into(), serde_json::json!(5678));
    fixture.graph(changed);
    fixture.expired();
    assert!(matches!(
        fixture.submit(AppCommand::AcceptCalibration { token }),
        CommandOutcome::Rejected(_)
    ));
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn rebinding_and_restoring_source_does_not_revive_review() {
    let mut fixture = Fixture::new(Some(2));
    let token = fixture.start();
    fixture.review(&token);
    for node in ["other_raw", "fixture_raw"] {
        assert!(matches!(
            fixture.submit(AppCommand::EditSource {
                source: fixture.source.clone(),
                changes: SourceEdit {
                    node_name: Some(node.into()),
                    ..SourceEdit::default()
                }
            }),
            CommandOutcome::Applied { .. }
        ));
    }
    let before = fixture.bytes();
    fixture.expired();
    assert!(matches!(
        fixture.submit(AppCommand::AcceptCalibration { token }),
        CommandOutcome::Rejected(_)
    ));
    assert_eq!(fixture.bytes(), before);
    assert_eq!(
        fixture.handle.snapshot().desired.sources[&fixture.source]
            .fx
            .as_ref()
            .unwrap()
            .gate,
        false
    );
}

#[test]
fn missing_real_serial_and_unsupported_current_channels_refuse_start() {
    let mut fixture = Fixture::new(Some(2));
    let before = fixture.bytes();
    let mut fallback = fixture.capture.clone();
    fallback.properties.remove("object.serial");
    fallback.identity.object_serial = fallback.node_id.to_string();
    fixture.graph(fallback);
    fixture.until(|snapshot| {
        !snapshot.captures[0]
            .properties
            .contains_key("object.serial")
    });
    assert!(matches!(
        fixture.submit(AppCommand::StartCalibration {
            source: fixture.source.clone()
        }),
        CommandOutcome::Rejected(_)
    ));
    let mut unsupported = fixture.capture.clone();
    unsupported.channels = Some(8);
    fixture.graph(unsupported);
    fixture.until(|snapshot| snapshot.captures[0].channels == Some(8));
    assert!(matches!(
        fixture.submit(AppCommand::StartCalibration {
            source: fixture.source.clone()
        }),
        CommandOutcome::Rejected(_)
    ));
    assert_eq!(fixture.bytes(), before);
}

#[test]
fn unsafe_measurements_reject_without_applying_and_shutdown_cancels_recording() {
    let mut fixture = Fixture::new(Some(2));
    let before = fixture.bytes();
    let token = fixture.start();
    fixture.noise(&token);
    let speech = fixture.recording(&token, true);
    fixture.complete(&token, metrics(0.0));
    assert!(matches!(
        fixture.outcome(speech),
        CommandOutcome::Rejected(_)
    ));
    fixture.expired();
    assert_eq!(fixture.bytes(), before);
    let next = fixture.start();
    let recording = fixture.recording(&next, false);
    let shutdown = fixture.handle.submit(AppCommand::Shutdown).unwrap();
    assert_eq!(fixture.outcome(recording), CommandOutcome::Cancelled);
    assert!(matches!(
        fixture.outcome(shutdown),
        CommandOutcome::Applied { .. }
    ));
    fixture.handle.wait_stopped().unwrap();
    assert_eq!(fixture.bytes(), before);
}

//! Device-free controlled runtime for actual GTK callback regressions.
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
    cell::RefCell,
    collections::{HashMap, VecDeque},
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex, MutexGuard, mpsc},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const DEADLINE: Duration = Duration::from_secs(5);

struct PrivateRoot(PathBuf);
impl PrivateRoot {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        for attempt in 0..100 {
            let path = std::env::temp_dir().join(format!(
                "openwave-widget-test-{}-{nonce}-{attempt}",
                std::process::id()
            ));
            match std::fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("private fixture directory: {error}"),
            }
        }
        panic!("private fixture directory collision limit");
    }
}
impl Drop for PrivateRoot {
    fn drop(&mut self) {
        // Only this successfully-created, captured fixture directory is removed.
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            eprintln!("fixture directory cleanup {}: {error}", self.0.display());
        }
    }
}

#[derive(Default)]
struct FixtureState {
    units: Vec<UnitSnapshot>,
    failures: Vec<String>,
    shutdown: bool,
    shutdown_attempts: usize,
    fail_shutdown_once: bool,
}

// Preserve poisoned state for diagnostics and shutdown rather than unwrap it.
fn fixture_state(state: &Mutex<FixtureState>) -> MutexGuard<'_, FixtureState> {
    match state.lock() {
        Ok(guard) => guard,
        Err(poison) => {
            let mut guard = poison.into_inner();
            guard.failures.push("fixture state was poisoned".into());
            state.clear_poison();
            guard
        }
    }
}
struct FixtureBackend {
    identity: PathBuf,
    state: Arc<Mutex<FixtureState>>,
    incoming: mpsc::Receiver<BackendEvent>,
    pending: VecDeque<BackendEvent>,
}
impl FixtureBackend {
    fn forbidden(&self, operation: &str) -> OperationError {
        let message = format!("widget fixture forbids {operation}");
        fixture_state(&self.state).failures.push(message.clone());
        OperationError::unavailable(message)
    }
}
impl Backend for FixtureBackend {
    fn identity(&self) -> &Path {
        &self.identity
    }
    fn dispatch(&mut self, command: BackendCommand) -> Result<()> {
        match command {
            BackendCommand::Routing {
                desired, bindings, ..
            } => {
                let fixture_sources = desired.sources.is_empty()
                    || (desired.sources.len() == 1
                        && desired.sources.values().all(|source| {
                            source.kind == SourceKind::App
                                && source.name == "Player"
                                && source.match_app_names == ["Player"]
                        }));
                if !fixture_sources || !bindings.is_empty() || desired.mixes != default_mixes() {
                    return Err(self.forbidden("non-fixture routing"));
                }
                // Fixture sources have no streams or audio nodes to route.
                Ok(())
            }
            BackendCommand::MeterTargets(targets) if targets.is_empty() => Ok(()),
            BackendCommand::Device {
                job,
                unit,
                settings,
            } => {
                if settings.is_empty()
                    || settings.iter().any(|setting| {
                        !matches!(setting, DeviceSetting::Mute(_))
                            && !matches!(setting, DeviceSetting::HeadphoneDb(db)
                        if db.is_finite() && (-60.0..=0.0).contains(db))
                    })
                {
                    return Err(
                        self.forbidden("device setting other than fixture headphone level or mute")
                    );
                }
                let mut state = fixture_state(&self.state);
                let Some(observed) = state
                    .units
                    .iter_mut()
                    .find(|candidate| candidate.id == unit)
                else {
                    drop(state);
                    return Err(self.forbidden("unknown device identity"));
                };
                let mut device = observed
                    .state
                    .known()
                    .cloned()
                    .expect("known fixture device state");
                for setting in settings {
                    match setting {
                        DeviceSetting::HeadphoneDb(db) => device.hp_volume_db = db,
                        DeviceSetting::Mute(muted) => device.muted = muted,
                        _ => unreachable!("validated fixture setting"),
                    }
                }
                observed.state = Observation::Known(device.clone());
                let observation = observed.clone();
                drop(state);
                self.pending.push_back(BackendEvent::DeviceFinished {
                    job,
                    unit,
                    result: Ok(device),
                });
                self.pending.push_back(BackendEvent::Unit(observation));
                Ok(())
            }
            BackendCommand::MeterTargets(_) => Err(self.forbidden("live meter targets")),
            BackendCommand::CaptureMute { .. } => Err(self.forbidden("capture mute")),
            BackendCommand::RecordCalibration { .. } => Err(self.forbidden("calibration")),
            BackendCommand::CancelCalibration(_) => Err(self.forbidden("calibration cancellation")),
            BackendCommand::Autostart { .. } => Err(self.forbidden("autostart")),
            BackendCommand::Setup { .. } => Err(self.forbidden("setup")),
            BackendCommand::Rescan => Err(self.forbidden("rescan")),
            BackendCommand::Activate => Err(self.forbidden("activation")),
        }
    }
    fn next_event(&mut self) -> Option<BackendEvent> {
        self.pending
            .pop_front()
            .or_else(|| self.incoming.try_recv().ok())
    }
    fn shutdown(&mut self) -> std::result::Result<(), ShutdownError> {
        // This fixture owns no USB, audio, service or child-process workers.
        let mut state = fixture_state(&self.state);
        state.shutdown_attempts += 1;
        if state.fail_shutdown_once && state.shutdown_attempts == 1 {
            return Err(ShutdownError {
                message: "controlled owned-worker drain failure".into(),
                issues: vec![],
            });
        }
        state.shutdown = true;
        Ok(())
    }
    fn prepare_removal(&mut self, _: &UninstallPlan, _: bool) -> Result<()> {
        Err(self.forbidden("removal preparation"))
    }
    fn remove(&mut self, _: &UninstallPlan, _: bool) -> Result<UninstallResult> {
        Err(self.forbidden("removal"))
    }
}

pub(crate) struct Rig {
    _root: PrivateRoot,
    handle: RuntimeHandle,
    incoming: mpsc::Sender<BackendEvent>,
    state: Arc<Mutex<FixtureState>>,
    submitted: Rc<RefCell<Vec<CommandId>>>,
    targets: Rc<RefCell<Vec<UnitId>>>,
    completed: mpsc::Receiver<(CommandId, CommandOutcome)>,
    outcomes: RefCell<HashMap<CommandId, CommandOutcome>>,
    pump: Option<thread::JoinHandle<()>>,
    pump_closed: mpsc::Receiver<()>,
    ui_events: mpsc::Receiver<RuntimeEvent>,
}
impl Rig {
    pub(crate) fn new(matrix: Value, units: Vec<UnitSnapshot>) -> Self {
        let root = PrivateRoot::new();
        for (name, value) in [("sources.json", json!({})), ("mixes.json", matrix)] {
            std::fs::write(root.0.join(name), serde_json::to_vec(&value).unwrap()).unwrap();
        }
        let state = Arc::new(Mutex::new(FixtureState {
            units: units.clone(),
            ..FixtureState::default()
        }));
        let backend_state = state.clone();
        let identity = root.0.clone();
        let (incoming, events) = mpsc::channel();
        let (handle, notifications) = RuntimeHandle::start_with(root.0.clone(), move || {
            Ok(Box::new(FixtureBackend {
                identity,
                state: backend_state,
                incoming: events,
                pending: VecDeque::new(),
            }))
        })
        .unwrap();
        let (send, completed) = mpsc::channel();
        let (closed, pump_closed) = mpsc::channel();
        let (ui_send, ui_events) = mpsc::channel();
        let pump = thread::spawn(move || {
            let context = glib::MainContext::new();
            while let Ok(event) = context.block_on(notifications.recv()) {
                let _ = ui_send.send(event.clone());
                if let RuntimeEvent::CommandFinished { id, result } = event {
                    let _ = send.send((id, result));
                }
            }
            let _ = closed.send(());
        });
        let rig = Self {
            _root: root,
            handle,
            incoming,
            state,
            submitted: Rc::new(RefCell::new(Vec::new())),
            targets: Rc::new(RefCell::new(Vec::new())),
            completed,
            outcomes: RefCell::new(HashMap::new()),
            pump: Some(pump),
            pump_closed,
            ui_events,
        };
        rig.wait(|snapshot| snapshot.revision > 0);
        for unit in units {
            rig.incoming.send(BackendEvent::Unit(unit)).unwrap();
        }
        rig.incoming
            .send(BackendEvent::Graph(MixerObservation {
                observation: Observation::Known(()),
                captures: vec![],
                streams: vec![],
                outputs: vec![],
                default_sink: None,
                meter_targets: vec![],
                mix_identities: Default::default(),
                silent_sources: Default::default(),
                errors: vec![],
                revision: rig.handle.snapshot().revision,
            }))
            .unwrap();
        rig.barrier("widget fixture ready");
        rig
    }
    fn check_backend(&self) {
        let failures = fixture_state(&self.state).failures.clone();
        assert!(
            failures.is_empty(),
            "unexpected fixture operations: {failures:?}"
        );
    }
    fn wait(&self, predicate: impl Fn(&AppSnapshot) -> bool) {
        let deadline = Instant::now() + DEADLINE;
        while !predicate(&self.handle.snapshot()) {
            self.check_backend();
            assert!(
                Instant::now() < deadline,
                "widget fixture snapshot deadline"
            );
            thread::sleep(Duration::from_millis(2));
        }
        self.check_backend();
    }
    fn barrier(&self, label: &str) {
        self.incoming
            .send(BackendEvent::Status {
                service: label.into(),
                setup_required: false,
            })
            .unwrap();
        self.wait(|snapshot| snapshot.service_status == label);
    }
    pub(crate) fn submitter(&self) -> crate::Submit {
        let handle = self.handle.clone();
        let submitted = self.submitted.clone();
        let targets = self.targets.clone();
        let state = self.state.clone();
        Rc::new(move |command| {
            let permitted = matches!(
                &command,
                AppCommand::SelectUnit { .. }
                    | AppCommand::SetDeviceSetting {
                        setting: DeviceSetting::HeadphoneDb(_),
                        ..
                    }
                    | AppCommand::SetDeviceSetting {
                        setting: DeviceSetting::Mute(_),
                        ..
                    }
                    | AppCommand::SetMaster { .. }
            ) || matches!(&command, AppCommand::AddSource { source }
                if source.kind == SourceKind::App && source.name == "Player"
                    && source.match_app_names == ["Player"]);
            if !permitted {
                // Avoid unwinding through a GTK signal trampoline; fail on observation instead.
                fixture_state(&state)
                    .failures
                    .push(format!("unexpected widget command: {command:?}"));
                return;
            }
            if let AppCommand::SetDeviceSetting { unit, .. } = &command {
                targets.borrow_mut().push(*unit);
            }
            match handle.submit(command) {
                Ok(id) => submitted.borrow_mut().push(id),
                Err(error) => fixture_state(&state)
                    .failures
                    .push(format!("widget submit failed: {error}")),
            }
        })
    }
    pub(crate) fn snapshot(&self) -> Arc<AppSnapshot> {
        self.check_backend();
        self.handle.snapshot()
    }
    pub(crate) fn finish_submissions(&self) {
        let ids = self.submitted.borrow().clone();
        let deadline = Instant::now() + DEADLINE;
        for id in ids {
            while !self.outcomes.borrow().contains_key(&id) {
                self.check_backend();
                let remaining = deadline.saturating_duration_since(Instant::now());
                let (next, result) = self
                    .completed
                    .recv_timeout(remaining)
                    .expect("widget command completion deadline (including production debounce)");
                assert!(
                    self.outcomes.borrow_mut().insert(next, result).is_none(),
                    "duplicate completion"
                );
            }
            assert!(
                matches!(
                    self.outcomes.borrow().get(&id),
                    Some(CommandOutcome::Applied { .. })
                ),
                "widget command {id:?} failed: {:?}",
                self.outcomes.borrow().get(&id)
            );
        }
        // Flush all device observations without ever rendering a widget snapshot.
        let label = format!("widget fixture completed {}", self.submitted.borrow().len());
        self.barrier(&label);
    }
    pub(crate) fn device_states(&self) -> Vec<UnitSnapshot> {
        self.check_backend();
        fixture_state(&self.state).units.clone()
    }
    pub(crate) fn submitted_targets(&self) -> Vec<UnitId> {
        self.targets.borrow().clone()
    }
    pub(crate) fn handle(&self) -> RuntimeHandle {
        self.handle.clone()
    }
    pub(crate) fn paths(&self) -> openwave_runtime::paths::RuntimePaths {
        let mut paths = asset_paths();
        paths.identity = self._root.0.clone();
        paths.source = Some(self._root.0.clone());
        paths
    }
    pub(crate) fn events(&self) -> Vec<RuntimeEvent> {
        self.ui_events.try_iter().collect()
    }
    pub(crate) fn fail_first_shutdown(&self) {
        fixture_state(&self.state).fail_shutdown_once = true;
    }
    pub(crate) fn shutdown_attempts(&self) -> usize {
        fixture_state(&self.state).shutdown_attempts
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        let _ = self.handle.submit(AppCommand::Shutdown);
        let closed = self.pump_closed.recv_timeout(DEADLINE).is_ok();
        let mut failure = None;
        if closed {
            if let Err(error) = self.handle.wait_stopped() {
                failure = Some(error.to_string());
            }
            if self.pump.take().is_some_and(|pump| pump.join().is_err()) {
                failure = Some("fixture event pump panicked".into());
            }
            let state = fixture_state(&self.state);
            if !state.shutdown || !state.failures.is_empty() {
                failure = Some(format!(
                    "fixture shutdown={}, failures={:?}",
                    state.shutdown, state.failures
                ));
            }
        } else {
            failure = Some("fixture controller/event pump shutdown exceeded five seconds".into());
        }
        if let Some(failure) = failure {
            if thread::panicking() {
                eprintln!("fixture cleanup failure during unwind: {failure}");
            } else {
                panic!("{failure}");
            }
        }
    }
}

pub(crate) fn unit(serial: &str, address: u8, hp_db: f64) -> UnitSnapshot {
    let profile = ProfileId::WaveXlr;
    let mut state = ConfigBuffer::decode(profile, &vec![0; profile.profile().config_len])
        .unwrap()
        .state();
    state.hp_volume_db = hp_db;
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

pub(crate) fn asset_paths() -> openwave_runtime::paths::RuntimePaths {
    let data = std::env::var_os("OPENWAVE_TEST_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."));
    openwave_runtime::paths::RuntimePaths {
        executable: data.join("unused-widget-fixture-executable"),
        maintenance: data.join("unused-widget-fixture-maintenance"),
        identity: data.clone(),
        source: Some(data.clone()),
        prefix: None,
        data,
    }
}

pub(crate) fn icons() -> Rc<crate::icons::Icons> {
    Rc::new(crate::icons::Icons::new(asset_paths()))
}

pub(crate) fn descendants<T: glib::object::IsA<gtk::Widget> + glib::object::ObjectType>(
    root: &impl glib::object::IsA<gtk::Widget>,
) -> Vec<T> {
    use adw::prelude::*;
    let mut pending: Vec<gtk::Widget> = vec![root.as_ref().clone()];
    let mut found = Vec::new();
    while let Some(widget) = pending.pop() {
        if let Ok(item) = widget.clone().downcast::<T>() {
            found.push(item);
        }
        let mut child = widget.first_child();
        while let Some(widget) = child {
            child = widget.next_sibling();
            pending.push(widget);
        }
    }
    found
}

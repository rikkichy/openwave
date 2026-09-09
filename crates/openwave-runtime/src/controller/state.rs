use super::*;
use crate::store::ConfigStore;
use openwave_core::{
    protocol::{ConnectedIdentity, device_for_capture, validate_setting},
    routing, scenes,
};
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

mod calibration;
mod mutations;
mod mutes;
mod scene;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SettingField {
    Gain,
    Mute,
    Headphones,
    Phantom,
    LowImpedance,
    Monitor,
}
fn setting_field(setting: DeviceSetting) -> SettingField {
    match setting {
        DeviceSetting::GainRaw(_) => SettingField::Gain,
        DeviceSetting::Mute(_) => SettingField::Mute,
        DeviceSetting::HeadphoneDb(_) => SettingField::Headphones,
        DeviceSetting::Phantom(_) => SettingField::Phantom,
        DeviceSetting::LowImpedance(_) => SettingField::LowImpedance,
        DeviceSetting::MonitorMix(_) => SettingField::Monitor,
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum PendingKey {
    Device(UnitId, SettingField),
    Cell(SourceId, MixId),
    Fx(SourceId),
}
struct PendingEdit {
    due: Instant,
    id: CommandId,
    command: AppCommand,
}
#[derive(Default)]
struct RequestCompletion {
    scene: bool,
    waiting: bool,
    cancelled: bool,
    remaining: HashSet<u64>,
    skipped: Vec<OperationIssue>,
    failed: Vec<OperationIssue>,
    first_error: Option<OperationError>,
}
struct DeviceJob {
    owner: Option<CommandId>,
    unit: UnitId,
    settings: Vec<DeviceSetting>,
    dispatched: bool,
    open_sources: Vec<(SourceId, String, String)>,
}

struct Controller {
    store: ConfigStore,
    backend: Box<dyn Backend>,
    shared: Arc<HandleState>,
    events: EventPublisher,
    view: AppSnapshot,
    graph_observation: Observation<()>,
    units: IndexMap<UnitId, UnitSnapshot>,
    retired: HashSet<UnitId>,
    requests: HashMap<CommandId, RequestCompletion>,
    accepted: HashSet<CommandId>,
    pending: HashMap<PendingKey, PendingEdit>,
    jobs: HashMap<u64, DeviceJob>,
    next_job: u64,
    bindings: IndexMap<String, CaptureBinding>,
    capture_intents: HashMap<String, (CaptureBinding, bool)>,
    capture_baselines: HashMap<SourceId, (CaptureBinding, NodeIdentity, bool)>,
    held_captures: HashMap<String, (CaptureBinding, Option<CommandId>)>,
    handovers: HashMap<String, mutes::Handover>,
    graph_revision: Option<u64>,
    silent_sources: HashMap<SourceId, NodeIdentity>,
    binding_epoch: u64,
    intents: HashMap<(UnitId, SettingField), (CommandId, DeviceSetting)>,
    meter_targets: HashMap<String, NodeIdentity>,
    meter_specs: Vec<MeterTarget>,
    observed_mixes: IndexMap<MixId, NodeIdentity>,
    routing_revision: u64,
    meters: IndexMap<String, f64>,
    errors: Vec<OperationIssue>,
    errors_dirty: bool,
    calibration: Option<calibration::CalibrationSession>,
    next_session: u64,
    frozen: bool,
    stopped: bool,
    dirty: bool,
    meters_dirty: bool,
}

pub(super) fn start(
    root: PathBuf,
    factory: impl FnOnce() -> Result<Box<dyn Backend>> + Send + 'static,
) -> Result<(RuntimeHandle, RuntimeEvents)> {
    let (commands, receive) = mpsc::channel();
    let (snapshot_send, snapshots) = async_channel::bounded(1);
    let (completion_send, completions) = async_channel::unbounded();
    let inner = Arc::new(HandleState {
        commands,
        snapshot: RwLock::new(Arc::new(AppSnapshot::default())),
        admission: Mutex::new(Admission {
            next_command: 1,
            accepting: true,
            stopped: false,
        }),
        worker: Mutex::new(None),
    });
    let shared = Arc::clone(&inner);
    let worker = std::thread::Builder::new()
        .name("openwave-controller".into())
        .spawn(move || {
            let events = EventPublisher {
                snapshots: snapshot_send,
                completions: completion_send,
            };
            let store = ConfigStore::load(&root);
            match factory() {
                Ok(backend) => Controller::new(store, backend, shared, events).run(receive),
                Err(error) => {
                    let mut admission = shared
                        .admission
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner());
                    admission.accepting = false;
                    admission.stopped = true;
                    drop(admission);
                    for request in receive.try_iter() {
                        events.complete(request.id, CommandOutcome::Rejected(error.clone()));
                    }
                    let snapshot = AppSnapshot {
                        lifecycle: Lifecycle::Stopped,
                        desired: Arc::new(store.desired()),
                        preferences: Arc::new(store.preferences.value().clone()),
                        setup_phase: SetupPhase::Failed(error.to_string()),
                        errors: Arc::new(vec![OperationIssue {
                            target: "startup".into(),
                            message: error.to_string(),
                        }]),
                        ..AppSnapshot::default()
                    };
                    *shared
                        .snapshot
                        .write()
                        .unwrap_or_else(|poison| poison.into_inner()) = Arc::new(snapshot);
                    events.snapshot_changed();
                    events.shutdown(Err(ShutdownError {
                        message: error.to_string(),
                        issues: Vec::new(),
                    }));
                }
            }
        })?;
    *inner
        .worker
        .lock()
        .map_err(|_| OperationError::unavailable("Controller join ownership failed"))? =
        Some(worker);
    Ok((
        RuntimeHandle { inner },
        RuntimeEvents {
            snapshots,
            completions,
        },
    ))
}

impl Controller {
    fn new(
        store: ConfigStore,
        backend: Box<dyn Backend>,
        shared: Arc<HandleState>,
        events: EventPublisher,
    ) -> Self {
        let errors = store.issues();
        let view = AppSnapshot {
            revision: 1,
            desired: Arc::new(store.desired()),
            preferences: Arc::new(store.preferences.value().clone()),
            lifecycle: Lifecycle::Running,
            ..AppSnapshot::default()
        };
        let mut state = Self {
            store,
            backend,
            shared,
            events,
            view,
            units: IndexMap::new(),
            retired: HashSet::new(),
            requests: HashMap::new(),
            accepted: HashSet::new(),
            graph_observation: Observation::Unknown(OperationError::unavailable(
                "Graph has not been observed",
            )),
            pending: HashMap::new(),
            jobs: HashMap::new(),
            next_job: 1,
            bindings: IndexMap::new(),
            binding_epoch: 0,
            intents: HashMap::new(),
            capture_intents: HashMap::new(),
            capture_baselines: HashMap::new(),
            held_captures: HashMap::new(),
            handovers: HashMap::new(),
            graph_revision: None,
            silent_sources: HashMap::new(),
            meter_targets: HashMap::new(),
            meter_specs: Vec::new(),
            observed_mixes: IndexMap::new(),
            routing_revision: 0,
            meters: IndexMap::new(),
            errors,
            errors_dirty: true,
            calibration: None,
            next_session: 1,
            frozen: false,
            stopped: false,
            dirty: true,
            meters_dirty: false,
        };
        state.refresh_bindings();
        state.route();
        state
    }
    fn run(mut self, receiver: mpsc::Receiver<CommandEnvelope>) {
        while !self.stopped {
            for _ in 0..256 {
                let Some(event) = self.backend.next_event() else {
                    break;
                };
                self.observe(event);
            }
            let now = Instant::now();
            let due: Vec<_> = self
                .pending
                .iter()
                .filter(|(_, edit)| edit.due <= now)
                .map(|(key, _)| key.clone())
                .collect();
            for key in due {
                if let Some(edit) = self.pending.remove(&key) {
                    if let Some(request) = self.requests.get_mut(&edit.id) {
                        request.waiting = false;
                    }
                    self.refresh_pending_view();
                    self.apply(edit.id, edit.command);
                }
            }
            self.publish();
            let wait = self
                .pending
                .values()
                .map(|edit| edit.due.saturating_duration_since(Instant::now()))
                .min()
                .unwrap_or(Duration::from_millis(10))
                .min(Duration::from_millis(10));
            match receiver.recv_timeout(wait) {
                Ok(request) => self.accept(request),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let mut admission = self
            .shared
            .admission
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        admission.accepting = false;
        admission.stopped = true;
        drop(admission);
        for request in receiver.try_iter() {
            self.events.complete(request.id, CommandOutcome::Cancelled);
        }
    }
    fn accept(&mut self, request: CommandEnvelope) {
        self.accepted.insert(request.id);
        self.requests
            .insert(request.id, RequestCompletion::default());
        self.apply(request.id, request.command);
    }
    fn publish(&mut self) {
        if !self.dirty {
            return;
        }
        if self.meters_dirty {
            self.view.meters = Arc::new(self.meters.clone());
            self.meters_dirty = false;
        }
        if self.errors_dirty {
            self.view.errors = Arc::new(self.errors.clone());
            self.errors_dirty = false;
        }
        *self
            .shared
            .snapshot
            .write()
            .unwrap_or_else(|poison| poison.into_inner()) = Arc::new(self.view.clone());
        self.events.snapshot_changed();
        self.dirty = false;
    }
    fn issue(&mut self, target: impl Into<String>, error: impl ToString) {
        let target = target.into();
        let issue = OperationIssue {
            target: target.clone(),
            message: error.to_string(),
        };
        if let Some(existing) = self.errors.iter_mut().find(|issue| issue.target == target) {
            *existing = issue;
        } else {
            self.errors.push(issue);
        }
        self.errors_dirty = true;
        self.dirty = true;
    }
    fn clear_issue(&mut self, target: &str) {
        let previous = self.errors.len();
        self.errors.retain(|issue| issue.target != target);
        if self.errors.len() != previous {
            self.errors_dirty = true;
            self.dirty = true;
        }
    }
    fn refresh_desired(&mut self) {
        self.view.revision = self.view.revision.saturating_add(1);
        self.view.desired = Arc::new(self.store.desired());
        self.view.preferences = Arc::new(self.store.preferences.value().clone());
        self.dirty = true;
    }
    fn route(&mut self) {
        if self.frozen || !self.store.routing_available() {
            return;
        }
        self.routing_revision = self.view.revision;
        let command = BackendCommand::Routing {
            revision: self.view.revision,
            desired: self.routing_desired(),
            bindings: self.bindings.clone(),
        };
        if let Err(error) = self.backend.dispatch(command) {
            self.issue("routing", error);
        }
    }
    fn refresh_bindings(&mut self) {
        let mut owners: IndexMap<String, Vec<SourceId>> = IndexMap::new();
        for (id, source) in self.store.sources.value() {
            if source.kind == SourceKind::Device && !source.node_name.is_empty() {
                owners
                    .entry(source.node_name.clone())
                    .or_default()
                    .push(id.clone());
            }
        }
        for ids in owners.values_mut() {
            ids.sort();
        }
        self.bindings.retain(|node, _| owners.contains_key(node));
        for (node, owners) in owners {
            if self
                .bindings
                .get(&node)
                .is_some_and(|binding| binding.owners == owners)
            {
                continue;
            }
            self.binding_epoch = self.binding_epoch.saturating_add(1);
            self.bindings.insert(
                node,
                CaptureBinding {
                    epoch: self.binding_epoch,
                    owners,
                },
            );
        }
        self.capture_intents
            .retain(|node, (binding, _)| self.bindings.get(node) == Some(binding));
        self.capture_baselines.retain(|id, (binding, _, _)| {
            self.store
                .sources
                .value()
                .get(id)
                .is_some_and(|source| self.bindings.get(&source.node_name) == Some(binding))
        });
    }
    fn commit_sources(&mut self, sources: Sources) -> Result<()> {
        let before = Arc::clone(&self.view.desired);
        self.store.sources.replace(sources)?;
        self.invalidate_meters(&before.sources);
        self.clear_issue("sources");
        self.refresh_desired();
        self.refresh_bindings();
        self.refresh_calibration_validity();
        self.sync_unit_view();
        self.update_handovers(&before.sources);
        self.route();
        Ok(())
    }
    fn set_meter_targets(&mut self, targets: Vec<MeterTarget>) {
        self.meter_targets = targets
            .iter()
            .map(|target| (target.key.clone(), target.identity.clone()))
            .collect();
        self.meters
            .retain(|key, _| self.meter_targets.contains_key(key));
        self.meters_dirty = true;
        self.dirty = true;
        self.meter_specs = targets;
        if !self.frozen {
            if let Err(error) = self
                .backend
                .dispatch(BackendCommand::MeterTargets(self.meter_specs.clone()))
            {
                self.issue("meters", error);
            }
        }
    }
    fn invalidate_meters(&mut self, before: &Sources) {
        let retained: Vec<_> = self
            .meter_specs
            .iter()
            .filter(|target| {
                if let Some(id) = target.key.strip_prefix("src:") {
                    match (before.get(id), self.store.sources.value().get(id)) {
                        (Some(old), Some(current)) => {
                            old.kind == current.kind
                                && old.node_name == current.node_name
                                && old.match_app_names == current.match_app_names
                                && old.fx == current.fx
                        }
                        _ => false,
                    }
                } else if let Some(id) = target.key.strip_prefix("mix:") {
                    self.store.mixes.value().contains_key(id)
                } else {
                    true
                }
            })
            .cloned()
            .collect();
        if retained.len() != self.meter_specs.len() {
            self.set_meter_targets(retained);
        }
    }
    fn commit_matrix(&mut self, matrix: MatrixState) -> Result<()> {
        self.store.matrix.replace(matrix)?;
        self.clear_issue("matrix");
        self.refresh_desired();
        self.route();
        Ok(())
    }
    fn bound_unit(&self, source: &Source) -> Option<UnitId> {
        if source.kind != SourceKind::Device || self.graph_observation.known().is_none() {
            return None;
        }
        let mut matching = self
            .view
            .captures
            .iter()
            .filter(|capture| capture.node_name == source.node_name);
        let capture = matching.next()?;
        if matching.next().is_some() {
            return None;
        }
        let serial = capture
            .properties
            .get("device.serial")
            .and_then(serde_json::Value::as_str)?;
        let units: Vec<_> = self
            .units
            .values()
            .map(|unit| ConnectedIdentity {
                unit: unit.id,
                serial: &unit.info.serial,
                connected: true,
            })
            .collect();
        device_for_capture(serial, &units)
    }
    fn unit_sources(&self, unit: UnitId) -> Vec<SourceId> {
        self.store
            .sources
            .value()
            .iter()
            .filter(|(_, source)| self.bound_unit(source) == Some(unit))
            .map(|(id, _)| id.clone())
            .collect()
    }
    fn sync_unit_view(&mut self) {
        let desired: HashMap<_, _> = self
            .units
            .keys()
            .map(|unit| {
                let sources = self.unit_sources(*unit);
                let muted = if sources.is_empty() {
                    None
                } else {
                    Some(
                        sources
                            .iter()
                            .all(|id| self.store.sources.value()[id].muted),
                    )
                };
                (*unit, muted)
            })
            .collect();
        for unit in self.units.values_mut() {
            unit.desired_mute = desired[&unit.id].or_else(|| {
                self.intents
                    .get(&(unit.id, SettingField::Mute))
                    .and_then(|(_, setting)| match setting {
                        DeviceSetting::Mute(muted) => Some(*muted),
                        _ => None,
                    })
            });
        }
        let mut units: Vec<_> = self.units.values().cloned().collect();
        units.sort_by_key(|unit| (unit.id.bus, unit.id.address));
        self.view.units = Arc::new(units);
        let mut intents = IndexMap::<UnitId, Vec<DeviceSetting>>::new();
        for ((unit, _), (_, setting)) in &self.intents {
            intents.entry(*unit).or_default().push(*setting);
        }
        self.view.unit_intents = Arc::new(intents);
        self.dirty = true;
    }
    fn remember_intents(&mut self, id: CommandId, unit: UnitId, settings: &[DeviceSetting]) {
        for setting in settings {
            self.intents
                .insert((unit, setting_field(*setting)), (id, *setting));
        }
        self.sync_unit_view();
    }
    fn queue_device(
        &mut self,
        owner: Option<CommandId>,
        unit: UnitId,
        settings: Vec<DeviceSetting>,
    ) -> Result<()> {
        if !self.units.contains_key(&unit) {
            return Err(OperationError::unavailable(
                "Captured device is disconnected",
            ));
        }
        let settings: Vec<_> = settings
            .into_iter()
            .map(|setting| validate_setting(unit.profile, setting))
            .collect::<Result<_>>()?;
        if settings.is_empty() {
            return Ok(());
        }
        let job = self.next_job;
        self.next_job = self
            .next_job
            .checked_add(1)
            .ok_or_else(|| OperationError::unavailable("Device job space exhausted"))?;
        let open_sources: Vec<(SourceId, String, String)> = if settings
            .contains(&DeviceSetting::Mute(false))
        {
            self.mute_sources_for_unit(unit)
                .into_iter()
                .filter_map(|id| {
                    let source = self.store.sources.value().get(&id)?;
                    (!source.muted).then(|| (id, source.node_name.clone(), source.group.clone()))
                })
                .collect()
        } else {
            Vec::new()
        };
        let dispatched = !open_sources
            .iter()
            .any(|(_, _, group)| self.handovers.contains_key(group));
        if dispatched {
            self.backend.dispatch(BackendCommand::Device {
                job,
                unit,
                settings: settings.clone(),
            })?;
        }
        self.jobs.insert(
            job,
            DeviceJob {
                owner,
                unit,
                settings: settings.clone(),
                dispatched,
                open_sources,
            },
        );
        if let Some(id) = owner {
            if let Some(request) = self.requests.get_mut(&id) {
                request.remaining.insert(job);
            }
            self.remember_intents(id, unit, &settings);
        }
        Ok(())
    }
    fn sync_mutes(&mut self, before: &Sources, origin: &Origin, owner: Option<CommandId>) {
        self.sync_mutes_excluding(before, origin, owner, &HashSet::new());
    }
    fn sync_mutes_excluding(
        &mut self,
        before: &Sources,
        origin: &Origin,
        owner: Option<CommandId>,
        excluded: &HashSet<UnitId>,
    ) {
        let changed: Vec<_> = self
            .store
            .sources
            .value()
            .iter()
            .filter(|(id, source)| before.get(*id).is_none_or(|old| old.muted != source.muted))
            .map(|(_, source)| source.clone())
            .collect();
        let mut units = HashSet::new();
        let mut captures = HashSet::new();
        for source in changed {
            if source.kind != SourceKind::Device {
                continue;
            }
            if let Some(unit) = self.bound_unit(&source) {
                if !excluded.contains(&unit)
                    && !matches!(origin, Origin::Hardware(from) if *from == unit)
                {
                    units.insert(unit);
                }
            } else {
                captures.insert(source.node_name);
            }
        }
        let mut writes: Vec<_> = units
            .into_iter()
            .map(|unit| {
                let muted = self
                    .unit_sources(unit)
                    .iter()
                    .all(|source| self.store.sources.value()[source].muted);
                (unit, muted)
            })
            .collect();
        writes.sort_by_key(|(_, muted)| !*muted);
        for (unit, muted) in writes {
            if let Err(error) = self.queue_device(owner, unit, vec![DeviceSetting::Mute(muted)]) {
                self.operation_failure(owner, format!("device {unit:?}"), error);
            }
        }
        for node_name in captures {
            let Some(binding) = self.bindings.get(&node_name).cloned() else {
                continue;
            };
            if matches!(origin, Origin::Capture(identity) if self.view.captures.iter().any(|capture| capture.node_name == node_name && &capture.identity == identity))
            {
                continue;
            }
            let muted = binding
                .owners
                .iter()
                .all(|id| self.store.sources.value()[id].muted);
            if let Err(error) = self.queue_capture(node_name.clone(), binding, muted, owner) {
                self.operation_failure(owner, format!("capture {node_name}"), error);
            }
        }
    }
    fn operation_failure(
        &mut self,
        owner: Option<CommandId>,
        target: String,
        error: OperationError,
    ) {
        self.issue(target.clone(), &error);
        if let Some(request) = owner.and_then(|id| self.requests.get_mut(&id)) {
            request.cancelled |= error.code == ErrorCode::Cancelled;
            if request.first_error.is_none() {
                request.first_error = Some(error.clone());
            }
            request.failed.push(OperationIssue {
                target,
                message: error.to_string(),
            });
        }
    }
    fn finish(&mut self, id: CommandId, outcome: CommandOutcome) {
        if !self.accepted.remove(&id) {
            return;
        }
        self.requests.remove(&id);
        self.view.scene_pending = self.requests.values().any(|request| request.scene);
        self.dirty = true;
        let before = self.intents.len();
        self.intents.retain(|_, (owner, _)| *owner != id);
        if before != self.intents.len() {
            self.sync_unit_view();
        }
        if matches!(outcome, CommandOutcome::SceneFinished { .. }) {
            self.view.scene_outcome = Some(outcome.clone());
            self.dirty = true;
        }
        self.publish();
        self.events.complete(id, outcome);
    }
    fn finish_if_ready(&mut self, id: CommandId) {
        let Some(request) = self.requests.get(&id) else {
            return;
        };
        if request.waiting
            || !request.remaining.is_empty()
            || self
                .held_captures
                .values()
                .any(|(_, owner)| *owner == Some(id))
        {
            return;
        }
        let outcome = if request.scene {
            CommandOutcome::SceneFinished {
                revision: self.view.revision,
                skipped: request.skipped.clone(),
                failed: request.failed.clone(),
            }
        } else if request.cancelled {
            CommandOutcome::Cancelled
        } else if !request.failed.is_empty() {
            let code = request
                .first_error
                .as_ref()
                .map_or(ErrorCode::Unavailable, |error| error.code);
            CommandOutcome::Rejected(OperationError::new(
                code,
                request
                    .failed
                    .iter()
                    .map(|issue| format!("{}: {}", issue.target, issue.message))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ))
        } else {
            CommandOutcome::Applied {
                revision: self.view.revision,
            }
        };
        self.finish(id, outcome);
    }
    fn cancel_pending(&mut self, predicate: impl Fn(&PendingKey) -> bool) {
        let keys: Vec<_> = self
            .pending
            .keys()
            .filter(|key| predicate(key))
            .cloned()
            .collect();
        let cancelled: Vec<_> = keys
            .into_iter()
            .filter_map(|key| self.pending.remove(&key))
            .map(|edit| edit.id)
            .collect();
        if cancelled.is_empty() {
            return;
        }
        self.refresh_pending_view();
        for id in cancelled {
            self.finish(id, CommandOutcome::Cancelled);
        }
    }
    fn defer(&mut self, key: PendingKey, id: CommandId, command: AppCommand, delay: Duration) {
        if let Some(request) = self.requests.get_mut(&id) {
            request.waiting = true;
        }
        if let AppCommand::SetDeviceSetting { unit, setting, .. } = &command {
            self.intents
                .insert((*unit, setting_field(*setting)), (id, *setting));
            self.sync_unit_view();
        }
        let previous = self.pending.insert(
            key,
            PendingEdit {
                due: Instant::now() + delay,
                id,
                command,
            },
        );
        self.refresh_pending_view();
        if let Some(previous) = previous {
            self.finish(previous.id, CommandOutcome::Cancelled);
        }
    }
    fn refresh_pending_view(&mut self) {
        let mut cells = IndexMap::new();
        let mut effects = IndexMap::new();
        for edit in self.pending.values() {
            match &edit.command {
                AppCommand::SetCell {
                    source,
                    mix,
                    level,
                    muted,
                    ..
                } => {
                    cells.insert(
                        format!("{source}.{mix}"),
                        LevelState {
                            volume: *level,
                            muted: *muted,
                            ..LevelState::default()
                        },
                    );
                }
                AppCommand::SetFx {
                    source, settings, ..
                } => {
                    effects.insert(source.clone(), settings.clone());
                }
                _ => {}
            }
        }
        self.view.pending_cells = Arc::new(cells);
        self.view.pending_fx = Arc::new(effects);
        self.dirty = true;
    }
    fn observe_unit(&mut self, unit: UnitSnapshot) {
        if self.retired.contains(&unit.id) {
            return;
        }
        let id = unit.id;
        let muted = unit.state.known().map(|state| state.muted);
        self.units.insert(id, unit);
        if self.view.selected_unit.is_none() {
            self.view.selected_unit = Some(id);
        }
        if !self.frozen {
            if let Some(muted) = muted {
                let targets = self.unit_sources(id);
                // Multiple rows may deliberately keep independent software mutes
                // while sharing one open device. Reconcile a hardware mismatch
                // against their aggregate, not each muted row of that open input.
                let desired_mute = targets
                    .iter()
                    .all(|source| self.store.sources.value()[source].muted);
                for source in targets {
                    if desired_mute != muted
                        && !self.unit_mute_pending(id)
                        && self.store.sources.value()[&source].muted != muted
                    {
                        if let Err(error) =
                            self.set_source_mute(&source, muted, Origin::Hardware(id), None)
                        {
                            self.issue("sources", error);
                        }
                    }
                }
            }
        }
        self.sync_unit_view();
    }
    fn observe(&mut self, event: BackendEvent) {
        match event {
            BackendEvent::Unit(unit) => self.observe_unit(unit),
            BackendEvent::UnitRetired(unit) => {
                self.retired.insert(unit);
                self.units.shift_remove(&unit);
                self.cancel_pending(
                    |key| matches!(key, PendingKey::Device(target, _) if *target == unit),
                );
                self.intents.retain(|(target, _), _| *target != unit);
                if self.view.selected_unit == Some(unit) {
                    self.view.selected_unit = self
                        .units
                        .keys()
                        .min_by_key(|unit| (unit.bus, unit.address))
                        .copied();
                }
                self.sync_unit_view();
            }
            BackendEvent::DeviceFinished { job, unit, result } => {
                if self.jobs.get(&job).is_some_and(|owned| !owned.dispatched) {
                    return;
                }
                let Some(owned) = self.jobs.remove(&job) else {
                    return;
                };
                if owned.unit != unit {
                    if let Some(snapshot) = self.units.get_mut(&owned.unit) {
                        snapshot.state = Observation::Unknown(OperationError::new(
                            ErrorCode::Identity,
                            "Worker completion changed captured unit",
                        ));
                    }
                    self.operation_failure(
                        owned.owner,
                        "device identity".into(),
                        OperationError::new(
                            ErrorCode::Identity,
                            "Worker completion changed captured unit",
                        ),
                    );
                } else {
                    match result {
                        Ok(state) => {
                            if let Some(mut snapshot) = self.units.get(&unit).cloned() {
                                snapshot.state = Observation::Known(state);
                                self.observe_unit(snapshot);
                            }
                        }
                        Err(error) => {
                            if let Some(snapshot) = self.units.get_mut(&unit) {
                                snapshot.state = Observation::Unknown(error.clone());
                            }
                            self.operation_failure(owned.owner, format!("device {unit:?}"), error)
                        }
                    }
                }
                if let Some(id) = owned.owner {
                    if let Some(request) = self.requests.get_mut(&id) {
                        request.remaining.remove(&job);
                    }
                    self.finish_if_ready(id);
                }
            }
            BackendEvent::Graph(mut graph) => {
                self.graph_observation = graph.observation;
                self.graph_revision = self.graph_observation.known().map(|_| graph.revision);
                let known = self.graph_observation.known().is_some();
                if known {
                    for capture in &mut graph.captures {
                        if let Some((binding, muted)) = self.capture_intents.get(&capture.node_name)
                        {
                            if graph.revision == self.routing_revision
                                && self.bindings.get(&capture.node_name) == Some(binding)
                                && capture.muted.known() == Some(muted)
                            {
                                self.capture_intents.remove(&capture.node_name);
                            } else {
                                capture.muted = Observation::Unknown(OperationError::unavailable(
                                    "Capture mute intent is not yet confirmed",
                                ));
                            }
                        }
                    }
                    self.view.captures = Arc::new(graph.captures);
                    self.view.streams = Arc::new(graph.streams);
                    self.view.outputs = Arc::new(graph.outputs);
                    self.view.default_output = graph.default_sink;
                    self.observed_mixes = graph.mix_identities;
                    self.silent_sources = graph.silent_sources;
                } else {
                    self.observed_mixes.clear();
                    self.silent_sources.clear();
                }
                self.errors
                    .retain(|issue| !issue.target.starts_with("routing"));
                self.errors_dirty = true;
                for issue in graph.errors {
                    self.issue(format!("routing: {}", issue.target), issue.message);
                }
                if known && graph.revision == self.routing_revision {
                    self.set_meter_targets(graph.meter_targets);
                }
                if !self.frozen {
                    if known {
                        // A graph sampled before the latest routing intent is display
                        // data, not authority to undo that intent or its binding epoch.
                        if graph.revision == self.routing_revision {
                            self.follow_capture_mutes();
                        }
                        self.auto_add_captures();
                    }
                    self.refresh_calibration_validity();
                }
                self.sync_unit_view();
            }
            BackendEvent::Master {
                mix,
                level,
                muted,
                revision,
                identity,
            } => {
                if !self.frozen
                    && self.graph_observation.known().is_some()
                    && revision == self.routing_revision
                    && self.observed_mixes.get(&mix) == Some(&identity)
                    && self.store.mixes.value().contains_key(&mix)
                {
                    let mut matrix = self.store.matrix.value().clone();
                    if matrix.volumes.get(&mix).is_none_or(|current| {
                        (current.volume - level).abs() > 0.001 || current.muted != muted
                    }) {
                        let entry = matrix.volumes.entry(mix).or_insert_with(|| LevelState {
                            volume: 1.0,
                            ..LevelState::default()
                        });
                        entry.volume = level;
                        entry.muted = muted;
                        if let Err(error) = self.commit_matrix(matrix) {
                            self.issue("matrix", error);
                        }
                    }
                }
            }
            BackendEvent::Meter(event) => {
                if event.peak.is_finite()
                    && self.meter_targets.get(&event.key) == Some(&event.identity)
                    && (event.key.starts_with("src:") || event.key.starts_with("mix:"))
                {
                    self.meters.insert(event.key, event.peak);
                    self.meters_dirty = true;
                    self.dirty = true;
                }
            }
            BackendEvent::Calibration(event) => self.calibration_complete(event),
            BackendEvent::Autostart { job, actual, error } => {
                match actual {
                    Ok((enabled, hidden)) => {
                        self.view.autostart = enabled;
                        self.view.hidden_autostart = hidden;
                    }
                    Err(error) => {
                        self.operation_failure(job.map(CommandId), "autostart".into(), error)
                    }
                }
                if let Some(error) = error {
                    self.operation_failure(job.map(CommandId), "autostart".into(), error);
                }
                if let Some(job) = job {
                    let id = CommandId(job);
                    if let Some(request) = self.requests.get_mut(&id) {
                        request.waiting = false;
                    }
                    self.finish_if_ready(id);
                }
                self.dirty = true;
            }
            BackendEvent::Setup { job, result } => {
                let id = CommandId(job);
                match result {
                    Ok(outcome) => {
                        self.view.setup_required = false;
                        self.view.setup_phase = SetupPhase::Replug(outcome.message);
                    }
                    Err(error) => {
                        self.view.setup_phase = SetupPhase::Failed(error.to_string());
                        self.operation_failure(Some(id), "setup".into(), error);
                    }
                }
                if let Some(request) = self.requests.get_mut(&id) {
                    request.waiting = false;
                }
                self.finish_if_ready(id);
                self.dirty = true;
            }
            BackendEvent::Status {
                service,
                setup_required,
            } => {
                self.view.service_status = service;
                self.view.setup_required = setup_required;
                if self.view.setup_phase == SetupPhase::Checking {
                    self.view.setup_phase = if setup_required {
                        SetupPhase::Required
                    } else {
                        SetupPhase::Ready
                    };
                }
                self.dirty = true;
            }
            BackendEvent::Error(issue) => self.issue(issue.target, issue.message),
        }
        self.advance_handovers();
    }
}

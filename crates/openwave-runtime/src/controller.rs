use crate::uninstall::UninstallPlan;
use crate::{
    calibration::CalibrationEvent,
    meter::{MeterEvent, MeterTarget},
    mixer::{CaptureBinding, MixerObservation},
    uninstall::UninstallResult,
};
use async_channel::{Receiver, Sender};
use indexmap::IndexMap;
use openwave_core::{effects::FxSettings, model::*, scenes::SceneId};
use std::{
    future::{Future, poll_fn},
    path::{Path, PathBuf},
    pin::pin,
    sync::{Arc, Mutex, RwLock, mpsc},
    task::Poll,
    thread::JoinHandle,
};

mod native;
mod state;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditTiming {
    Immediate,
    Debounced,
}
#[derive(Debug, Clone)]
pub enum AppCommand {
    AddSource {
        source: Source,
    },
    EditSource {
        source: SourceId,
        changes: SourceEdit,
    },
    RemoveSource {
        source: SourceId,
    },
    OrderSources {
        order: Vec<SourceId>,
    },
    AddMix {
        mix: Mix,
    },
    EditMix {
        mix: MixId,
        changes: MixEdit,
    },
    RemoveMix {
        mix: MixId,
    },
    OrderMixes {
        order: Vec<MixId>,
    },
    SetSourceLevel {
        source: SourceId,
        level: f64,
    },
    SetSourceMute {
        source: SourceId,
        muted: bool,
    },
    ToggleSourceMute {
        source: SourceId,
    },
    SetCell {
        source: SourceId,
        mix: MixId,
        level: f64,
        muted: bool,
        timing: EditTiming,
    },
    SetCellLevel {
        source: SourceId,
        mix: MixId,
        level: f64,
    },
    ToggleCellMute {
        source: SourceId,
        mix: MixId,
    },
    SetOutput {
        mix: MixId,
        choice: String,
    },
    SetMaster {
        mix: MixId,
        level: f64,
        muted: bool,
    },
    SetFx {
        source: SourceId,
        settings: FxSettings,
        timing: EditTiming,
    },
    ToggleFx {
        source: SourceId,
        effect: String,
    },
    JoinGroup {
        source: SourceId,
        target: SourceId,
    },
    LeaveGroup {
        source: SourceId,
    },
    SwitchGroup {
        group: String,
    },
    SelectUnit {
        unit: Option<UnitId>,
    },
    SetDeviceSetting {
        unit: UnitId,
        setting: DeviceSetting,
        timing: EditTiming,
    },
    ToggleDeviceMute {
        unit: UnitId,
    },
    SetGainLock {
        locked: bool,
    },
    SetPreferences {
        changes: PreferencesEdit,
    },
    SetAutostart {
        enabled: bool,
        hidden: bool,
    },
    SaveScene {
        name: String,
    },
    ApplyScene {
        scene: SceneId,
    },
    DeleteScene {
        scene: SceneId,
    },
    StartCalibration {
        source: SourceId,
    },
    RecordNoise {
        token: CalibrationToken,
    },
    RecordSpeech {
        token: CalibrationToken,
    },
    AcceptCalibration {
        token: CalibrationToken,
    },
    CancelCalibration {
        token: CalibrationToken,
    },
    Shutdown,
    PrepareUninstall {
        canonical_identity: String,
    },
    ConfirmUninstall {
        plan: Arc<UninstallPlan>,
        delete_settings: bool,
    },
    Reconnect,
    RunSetup,
    ContinueSetup,
}
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Shutdown incomplete: {message}")]
pub struct ShutdownError {
    pub message: String,
    pub issues: Vec<OperationIssue>,
}
#[derive(Debug, Clone)]
pub enum RuntimeEvent {
    SnapshotChanged,
    CommandFinished {
        id: CommandId,
        result: CommandOutcome,
    },
    ShutdownFinished(std::result::Result<(), ShutdownError>),
    UninstallFinished(UninstallResult),
}
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SubmitError {
    #[error("OpenWave is shutting down; mutations are frozen")]
    Frozen,
    #[error("OpenWave controller is unavailable")]
    Unavailable,
    #[error("Command identifier space exhausted")]
    Exhausted,
}

#[derive(Clone)]
pub struct RuntimeHandle {
    inner: Arc<HandleState>,
}
struct HandleState {
    commands: mpsc::Sender<CommandEnvelope>,
    snapshot: RwLock<Arc<AppSnapshot>>,
    admission: Mutex<Admission>,
    worker: Mutex<Option<JoinHandle<()>>>,
}
struct Admission {
    next_command: u64,
    accepting: bool,
    stopped: bool,
}
struct CommandEnvelope {
    id: CommandId,
    command: AppCommand,
}
pub struct RuntimeEvents {
    snapshots: Receiver<RuntimeEvent>,
    completions: Receiver<RuntimeEvent>,
}
impl RuntimeEvents {
    pub async fn recv(&self) -> std::result::Result<RuntimeEvent, async_channel::RecvError> {
        let mut completed = pin!(self.completions.recv());
        let mut changed = pin!(self.snapshots.recv());
        let mut completions_closed = false;
        let mut snapshots_closed = None;
        poll_fn(|cx| {
            if !completions_closed {
                match completed.as_mut().poll(cx) {
                    Poll::Ready(Ok(event)) => return Poll::Ready(Ok(event)),
                    Poll::Ready(Err(_)) => completions_closed = true,
                    Poll::Pending => {}
                }
            }
            if snapshots_closed.is_none() {
                match changed.as_mut().poll(cx) {
                    Poll::Ready(Ok(event)) => return Poll::Ready(Ok(event)),
                    Poll::Ready(Err(error)) => snapshots_closed = Some(error),
                    Poll::Pending => {}
                }
            }
            if completions_closed {
                if let Some(error) = snapshots_closed {
                    return Poll::Ready(Err(error));
                }
            }
            Poll::Pending
        })
        .await
    }
}
impl RuntimeHandle {
    pub fn submit(&self, command: AppCommand) -> std::result::Result<CommandId, SubmitError> {
        let mut admission = self
            .inner
            .admission
            .lock()
            .map_err(|_| SubmitError::Unavailable)?;
        if admission.stopped {
            return Err(SubmitError::Unavailable);
        }
        if !admission.accepting
            && !matches!(
                command,
                AppCommand::Shutdown
                    | AppCommand::PrepareUninstall { .. }
                    | AppCommand::ConfirmUninstall { .. }
            )
        {
            return Err(SubmitError::Frozen);
        }
        let id = CommandId(admission.next_command);
        admission.next_command = admission
            .next_command
            .checked_add(1)
            .ok_or(SubmitError::Exhausted)?;
        self.inner
            .commands
            .send(CommandEnvelope { id, command })
            .map_err(|_| SubmitError::Unavailable)?;
        Ok(id)
    }
    pub fn snapshot(&self) -> Arc<AppSnapshot> {
        Arc::clone(
            &self
                .inner
                .snapshot
                .read()
                .unwrap_or_else(|poison| poison.into_inner()),
        )
    }
    pub fn start_with(
        root: PathBuf,
        factory: impl FnOnce() -> Result<Box<dyn Backend>> + Send + 'static,
    ) -> Result<(Self, RuntimeEvents)> {
        state::start(root, factory)
    }
    pub fn wait_stopped(&self) -> std::result::Result<(), ShutdownError> {
        let worker = self
            .inner
            .worker
            .lock()
            .map_err(|_| ShutdownError {
                message: "Controller join ownership failed".into(),
                issues: Vec::new(),
            })?
            .take();
        if let Some(worker) = worker {
            worker.join().map_err(|_| ShutdownError {
                message: "Controller worker panicked".into(),
                issues: Vec::new(),
            })?;
        }
        Ok(())
    }
}

struct EventPublisher {
    snapshots: Sender<RuntimeEvent>,
    completions: Sender<RuntimeEvent>,
}
impl EventPublisher {
    fn snapshot_changed(&self) {
        let _ = self.snapshots.try_send(RuntimeEvent::SnapshotChanged);
    }
    fn complete(&self, id: CommandId, result: CommandOutcome) {
        let _ = self
            .completions
            .try_send(RuntimeEvent::CommandFinished { id, result });
    }
    fn shutdown(&self, result: std::result::Result<(), ShutdownError>) {
        let _ = self
            .completions
            .try_send(RuntimeEvent::ShutdownFinished(result));
    }
}

pub enum BackendCommand {
    Routing {
        revision: u64,
        desired: Arc<DesiredState>,
        bindings: IndexMap<String, CaptureBinding>,
    },
    Device {
        job: u64,
        unit: UnitId,
        settings: Vec<DeviceSetting>,
    },
    CaptureMute {
        node_name: String,
        binding: CaptureBinding,
        muted: bool,
    },
    MeterTargets(Vec<MeterTarget>),
    RecordCalibration {
        token: CalibrationToken,
        seconds: u32,
    },
    CancelCalibration(CalibrationToken),
    Autostart {
        job: u64,
        enabled: bool,
        hidden: bool,
    },
    Setup {
        job: u64,
        mixes: Mixes,
    },
    Rescan,
    Activate,
}

pub enum BackendEvent {
    Unit(UnitSnapshot),
    UnitRetired(UnitId),
    DeviceFinished {
        job: u64,
        unit: UnitId,
        result: Result<openwave_core::protocol::DeviceState>,
    },
    Graph(MixerObservation),
    Master {
        mix: MixId,
        level: f64,
        muted: bool,
        revision: u64,
        identity: NodeIdentity,
    },
    Meter(MeterEvent),
    Calibration(CalibrationEvent),
    Autostart {
        job: Option<u64>,
        actual: Result<(bool, bool)>,
        error: Option<OperationError>,
    },
    Setup {
        job: u64,
        result: Result<crate::setup::SetupOutcome>,
    },
    Status {
        service: String,
        setup_required: bool,
    },
    Error(OperationIssue),
}

pub trait Backend: Send {
    fn identity(&self) -> &Path;
    fn dispatch(&mut self, command: BackendCommand) -> Result<()>;
    fn next_event(&mut self) -> Option<BackendEvent>;
    fn shutdown(&mut self) -> std::result::Result<(), ShutdownError>;
    fn prepare_removal(&mut self, plan: &UninstallPlan, delete_settings: bool) -> Result<()>;
    fn remove(&mut self, plan: &UninstallPlan, delete_settings: bool) -> Result<UninstallResult>;
}

impl RuntimeHandle {
    pub fn launch(
        paths: crate::paths::RuntimePaths,
        allowed_bus_owner: Option<String>,
    ) -> Result<(Self, RuntimeEvents)> {
        crate::process::require_user()?;
        let root = crate::paths::config_dir()?;
        Self::start_with(root, move || {
            Ok(Box::new(native::NativeBackend::new(
                paths,
                allowed_bus_owner,
            )?))
        })
    }
}

impl EventPublisher {
    fn uninstall(&self, result: UninstallResult) {
        let _ = self
            .completions
            .try_send(RuntimeEvent::UninstallFinished(result));
    }
}

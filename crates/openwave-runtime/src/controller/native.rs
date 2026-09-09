use super::*;
use crate::{
    calibration::{CalibrationEvent, CalibrationWorker},
    desktop,
    device::{DeviceEvent, DeviceManager},
    health::HealthMonitor,
    meter::{MeterEvent, MeterMonitor},
    mixer::{Mixer, MixerEvent},
    paths::{Lease, RuntimePaths},
    service, setup, uninstall,
};
use std::{
    collections::VecDeque,
    sync::atomic::{AtomicBool, Ordering},
};

fn service_warning(status: Result<service::ServiceStatus>) -> String {
    match status {
        Ok(status) if status.running && !status.failed => String::new(),
        Ok(status) => status.message,
        Err(error) => format!("Capture service status unavailable: {error}"),
    }
}

enum HostCommand {
    Autostart {
        job: u64,
        enabled: bool,
        hidden: bool,
    },
    Setup {
        job: u64,
        mixes: Mixes,
    },
}
struct HostWorker {
    sender: Option<mpsc::Sender<HostCommand>>,
    events: mpsc::Receiver<BackendEvent>,
    stopping: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    stop_error: Option<OperationError>,
}
impl HostWorker {
    fn start(paths: RuntimePaths) -> Result<Self> {
        Self::start_with(paths, service::HostContext::discover)
    }
    fn start_with(
        paths: RuntimePaths,
        mut host_context: impl FnMut() -> Result<service::HostContext> + Send + 'static,
    ) -> Result<Self> {
        let (sender, commands) = mpsc::channel();
        let (send, events) = mpsc::channel();
        let stopping = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::clone(&stopping);
        let thread = std::thread::Builder::new()
            .name("openwave-integration".into())
            .spawn(move || {
                let _ = send.send(BackendEvent::Autostart {
                    job: None,
                    actual: desktop::autostart_state(),
                    error: None,
                });
                while let Ok(command) = commands.recv() {
                    match command {
                        HostCommand::Autostart {
                            job,
                            enabled,
                            hidden,
                        } => {
                            let result = if cancelled.load(Ordering::Acquire) {
                                Err(OperationError::new(
                                    ErrorCode::Cancelled,
                                    "Autostart edit cancelled during shutdown",
                                ))
                            } else {
                                desktop::set_autostart(&paths, enabled, hidden)
                            };
                            let (actual, error) = match result {
                                Ok(actual) => (Ok(actual), None),
                                Err(error) => (desktop::autostart_state(), Some(error)),
                            };
                            if send
                                .send(BackendEvent::Autostart {
                                    job: Some(job),
                                    actual,
                                    error,
                                })
                                .is_err()
                            {
                                break;
                            }
                        }
                        HostCommand::Setup { job, mixes } => {
                            let result = if cancelled.load(Ordering::Acquire) {
                                Err(OperationError::new(
                                    ErrorCode::Cancelled,
                                    "Setup cancelled during shutdown",
                                ))
                            } else {
                                host_context().and_then(|host| {
                                    let outcome = setup::run_with(&paths, &mixes, &host)?;
                                    let _ = send.send(BackendEvent::Status {
                                        service: service_warning(service::status_with(
                                            &paths, &host,
                                        )),
                                        setup_required: false,
                                    });
                                    Ok(outcome)
                                })
                            };
                            if send.send(BackendEvent::Setup { job, result }).is_err() {
                                break;
                            }
                        }
                    }
                }
            })?;
        Ok(Self {
            sender: Some(sender),
            events,
            stopping,
            thread: Some(thread),
            stop_error: None,
        })
    }
    fn send(&self, command: HostCommand) -> Result<()> {
        self.sender
            .as_ref()
            .ok_or_else(|| {
                OperationError::new(ErrorCode::Frozen, "Host integration worker is stopped")
            })?
            .send(command)
            .map_err(|_| OperationError::unavailable("Host integration worker exited"))
    }
    fn stop(&mut self) -> Result<()> {
        self.stopping.store(true, Ordering::Release);
        self.sender.take();
        if let Some(thread) = self.thread.take() {
            self.stop_error = thread
                .join()
                .err()
                .map(|_| OperationError::unavailable("Host integration worker panicked"));
        }
        self.stop_error.clone().map_or(Ok(()), Err)
    }
}
impl Drop for HostWorker {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

struct ActiveWorkers {
    devices: DeviceManager,
    device_events: mpsc::Receiver<DeviceEvent>,
    mixer: Mixer,
    mixer_events: mpsc::Receiver<MixerEvent>,
    meters: MeterMonitor,
    meter_events: mpsc::Receiver<MeterEvent>,
    calibration: CalibrationWorker,
    calibration_events: mpsc::Receiver<CalibrationEvent>,
    health: HealthMonitor,
    drain_issues: Option<Vec<OperationIssue>>,
}
impl ActiveWorkers {
    fn start(paths: &RuntimePaths) -> Result<Self> {
        let (mut meters, meter_events) = MeterMonitor::start()?;
        let readiness = meters.readiness();
        let (mut mixer, mixer_events) = match Mixer::start(paths.clone(), readiness.clone()) {
            Ok(value) => value,
            Err(error) => {
                let _ = meters.stop();
                return Err(error);
            }
        };
        let (mut devices, device_events) = match DeviceManager::start() {
            Ok(value) => value,
            Err(error) => {
                let _ = meters.stop();
                let _ = mixer.stop();
                return Err(error);
            }
        };
        let (mut calibration, calibration_events) = match CalibrationWorker::start() {
            Ok(value) => value,
            Err(error) => {
                let _ = devices.stop();
                let _ = meters.stop();
                let _ = mixer.stop();
                return Err(error);
            }
        };
        let health = match HealthMonitor::start(false, Arc::new(move || readiness.gaps())) {
            Ok(value) => value,
            Err(error) => {
                let _ = calibration.stop();
                let _ = devices.stop();
                let _ = meters.stop();
                let _ = mixer.stop();
                return Err(error);
            }
        };
        Ok(Self {
            devices,
            device_events,
            mixer,
            mixer_events,
            meters,
            meter_events,
            calibration,
            calibration_events,
            health,
            drain_issues: None,
        })
    }
    fn next_event(&mut self) -> Option<BackendEvent> {
        if let Ok(event) = self.device_events.try_recv() {
            return Some(match event {
                DeviceEvent::Connected(unit) | DeviceEvent::Observed(unit) => {
                    BackendEvent::Unit(unit)
                }
                DeviceEvent::Retired(unit) => BackendEvent::UnitRetired(unit),
                DeviceEvent::Completed { job, unit, result } => {
                    BackendEvent::DeviceFinished { job, unit, result }
                }
                DeviceEvent::Error { unit, error } => BackendEvent::Error(OperationIssue {
                    target: unit
                        .map_or_else(|| "USB discovery".into(), |unit| format!("device {unit:?}")),
                    message: error.to_string(),
                }),
            });
        }
        if let Ok(event) = self.mixer_events.try_recv() {
            return Some(match event {
                MixerEvent::Observed(graph) => BackendEvent::Graph(graph),
                MixerEvent::MasterObserved {
                    mix,
                    level,
                    muted,
                    revision,
                    identity,
                } => BackendEvent::Master {
                    mix,
                    level,
                    muted,
                    revision,
                    identity,
                },
            });
        }
        if let Ok(event) = self.calibration_events.try_recv() {
            return Some(BackendEvent::Calibration(event));
        }
        while let Ok(event) = self.meter_events.try_recv() {
            if self.meters.accepts(&event) {
                return Some(BackendEvent::Meter(event));
            }
        }
        None
    }
    fn stop(&mut self) -> std::result::Result<(), ShutdownError> {
        // Health retains restoration debt and its own sticky join failures.
        // Retry that debt before draining/cleaning the remaining audio workers.
        let health = self.health.stop();
        // Only consumed device/calibration joins need this outer failure cache.
        let drained = self.drain_issues.get_or_insert_with(|| {
            let mut issues = Vec::new();
            for (target, result) in [
                ("calibration", self.calibration.stop()),
                ("devices", self.devices.stop()),
            ] {
                if let Err(error) = result {
                    issues.push(OperationIssue {
                        target: target.into(),
                        message: error.to_string(),
                    });
                }
            }
            issues
        });
        let mut issues = drained.clone();
        if let Err(error) = health {
            issues.push(OperationIssue {
                target: "health".into(),
                message: error.to_string(),
            });
        }
        // MeterMonitor retains pending readers and terminal failures itself.
        // Retry its actual owner, and never start final graph cleanup until all
        // readers have drained successfully.
        match self.meters.stop() {
            Err(error) => issues.push(OperationIssue {
                target: "meters".into(),
                message: error.to_string(),
            }),
            Ok(()) => {
                // Mixer retains cleanup ownership after a failed attempt.
                if let Err(error) = self.mixer.stop() {
                    issues.push(OperationIssue {
                        target: "mixer".into(),
                        message: error.to_string(),
                    });
                }
            }
        }
        if issues.is_empty() {
            Ok(())
        } else {
            Err(ShutdownError {
                message: issues
                    .iter()
                    .map(|issue| format!("{}: {}", issue.target, issue.message))
                    .collect::<Vec<_>>()
                    .join("\n"),
                issues,
            })
        }
    }
}
impl Drop for ActiveWorkers {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

pub(super) struct NativeBackend {
    paths: RuntimePaths,
    allowed_bus_owner: Option<String>,
    installation_lease: Option<Lease>,
    vendor_lease: Option<Lease>,
    active: Option<ActiveWorkers>,
    host: Option<HostWorker>,
    queued: VecDeque<BackendEvent>,
    desired: Option<(u64, Arc<DesiredState>, IndexMap<String, CaptureBinding>)>,
    stopping: bool,
    stopped: bool,
}
impl NativeBackend {
    pub(super) fn new(paths: RuntimePaths, allowed_bus_owner: Option<String>) -> Result<Self> {
        let installation_lease = Lease::installation_shared(&paths.identity)?;
        let mut queued = VecDeque::new();
        let required = if setup::is_sandboxed() {
            false
        } else {
            match setup::inspect(&paths) {
                Ok(state) => state.required,
                Err(error) => {
                    queued.push_back(BackendEvent::Error(OperationIssue {
                        target: "setup inspection".into(),
                        message: error.to_string(),
                    }));
                    true
                }
            }
        };
        let service_status = if setup::is_sandboxed() {
            "Sandbox: native host setup and capture service management are unavailable here.".into()
        } else {
            service_warning(service::status(&paths))
        };
        queued.push_back(BackendEvent::Status {
            service: service_status,
            setup_required: required,
        });
        let host = HostWorker::start(paths.clone())?;
        let mut backend = Self {
            paths,
            allowed_bus_owner,
            installation_lease: Some(installation_lease),
            vendor_lease: None,
            active: None,
            host: Some(host),
            queued,
            desired: None,
            stopping: false,
            stopped: false,
        };
        if !required {
            backend.activate()?;
        }
        Ok(backend)
    }
    fn activate(&mut self) -> Result<()> {
        if self.stopping {
            return Err(OperationError::new(
                ErrorCode::Frozen,
                "Native workers are draining or stopped",
            ));
        }
        if self.active.is_some() {
            return Ok(());
        }
        // No USB enumeration or competing owner before both leases are held.
        let vendor = Lease::vendor_control(self.allowed_bus_owner.as_deref())?;
        let mut active = ActiveWorkers::start(&self.paths)?;
        if let Some((revision, desired, bindings)) = &self.desired {
            if let Err(error) =
                active
                    .mixer
                    .set_desired(*revision, Arc::clone(desired), bindings.clone())
            {
                let _ = active.stop();
                return Err(error);
            }
        }
        self.vendor_lease = Some(vendor);
        self.active = Some(active);
        Ok(())
    }
    fn active(&mut self) -> Result<&mut ActiveWorkers> {
        self.active.as_mut().ok_or_else(|| {
            OperationError::unavailable(
                "Complete setup and Continue before controlling audio or devices",
            )
        })
    }
}
impl Backend for NativeBackend {
    fn identity(&self) -> &Path {
        &self.paths.identity
    }
    fn dispatch(&mut self, command: BackendCommand) -> Result<()> {
        if self.stopping {
            return Err(OperationError::new(
                ErrorCode::Frozen,
                "Native workers are draining or stopped",
            ));
        }
        match command {
            BackendCommand::Routing {
                revision,
                desired,
                bindings,
            } => {
                if let Some(active) = self.active.as_mut() {
                    active
                        .mixer
                        .set_desired(revision, Arc::clone(&desired), bindings.clone())?;
                }
                self.desired = Some((revision, desired, bindings));
            }
            BackendCommand::Device {
                job,
                unit,
                settings,
            } => self.active()?.devices.submit(unit, job, settings)?,
            BackendCommand::CaptureMute {
                node_name,
                binding,
                muted,
            } => self
                .active()?
                .mixer
                .set_capture_mute(node_name, binding, muted)?,
            BackendCommand::MeterTargets(targets) => self.active()?.meters.set_targets(targets)?,
            BackendCommand::RecordCalibration { token, seconds } => {
                self.active()?.calibration.record(token, seconds)?
            }
            BackendCommand::CancelCalibration(token) => {
                if let Some(active) = self.active.as_mut() {
                    active.calibration.cancel(&token)?;
                }
            }
            BackendCommand::Autostart {
                job,
                enabled,
                hidden,
            } => self
                .host
                .as_ref()
                .ok_or_else(|| OperationError::unavailable("Integration worker is stopped"))?
                .send(HostCommand::Autostart {
                    job,
                    enabled,
                    hidden,
                })?,
            BackendCommand::Setup { job, mixes } => self
                .host
                .as_ref()
                .ok_or_else(|| OperationError::unavailable("Integration worker is stopped"))?
                .send(HostCommand::Setup { job, mixes })?,
            BackendCommand::Rescan => self.active()?.devices.rescan()?,
            BackendCommand::Activate => self.activate()?,
        }
        Ok(())
    }
    fn next_event(&mut self) -> Option<BackendEvent> {
        if let Some(event) = self.queued.pop_front() {
            return Some(event);
        }
        if let Some(host) = self.host.as_ref() {
            if let Ok(event) = host.events.try_recv() {
                return Some(event);
            }
        }
        self.active.as_mut().and_then(ActiveWorkers::next_event)
    }
    fn shutdown(&mut self) -> std::result::Result<(), ShutdownError> {
        if self.stopped {
            return Ok(());
        }
        self.stopping = true;
        let mut issues = Vec::new();
        if let Some(host) = self.host.as_mut() {
            if let Err(error) = host.stop() {
                issues.push(OperationIssue {
                    target: "host integration".into(),
                    message: error.to_string(),
                });
            }
            self.queued.extend(host.events.try_iter());
        }
        if let Some(active) = self.active.as_mut() {
            if let Err(error) = active.stop() {
                issues.extend(error.issues);
            }
            while let Some(event) = active.next_event() {
                self.queued.push_back(event);
            }
        }
        if !issues.is_empty() {
            return Err(ShutdownError {
                message: issues
                    .iter()
                    .map(|issue| format!("{}: {}", issue.target, issue.message))
                    .collect::<Vec<_>>()
                    .join("\n"),
                issues,
            });
        }
        self.host.take();
        self.active.take();
        self.vendor_lease.take();
        // Successful joins and owned graph cleanup precede lease release.
        // Confirmed removal obtains its exclusive lease afterwards.
        self.installation_lease.take();
        self.stopped = true;
        Ok(())
    }
    fn prepare_removal(&mut self, plan: &UninstallPlan, delete_settings: bool) -> Result<()> {
        uninstall::prepare(&self.paths, plan, delete_settings)
    }
    fn remove(&mut self, plan: &UninstallPlan, delete_settings: bool) -> Result<UninstallResult> {
        if !self.stopped || self.installation_lease.is_some() {
            return Err(OperationError::unavailable(
                "Workers must drain and release their lease before removal",
            ));
        }
        Ok(uninstall::execute(
            &self.paths,
            plan,
            delete_settings,
            false,
        ))
    }
}
impl Drop for NativeBackend {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_refreshes_service_warning_before_completion() {
        use std::{
            fs,
            os::unix::{fs::PermissionsExt, process::ExitStatusExt},
            process::{Command, ExitStatus, Output},
            time::Duration,
        };

        let Some(root) = std::env::var_os("OPENWAVE_SETUP_STATUS_FIXTURE") else {
            let root = tempfile::tempdir().unwrap();
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "controller::native::tests::setup_refreshes_service_warning_before_completion",
                    "--nocapture",
                ])
                .env("OPENWAVE_SETUP_STATUS_FIXTURE", root.path())
                .env("HOME", root.path())
                .env("XDG_CONFIG_HOME", root.path().join("config"))
                .env("XDG_DATA_HOME", root.path().join("data"))
                .env("XDG_STATE_HOME", root.path().join("state"))
                .env("XDG_RUNTIME_DIR", root.path().join("runtime"))
                .env_remove("DBUS_SESSION_BUS_ADDRESS")
                .env_remove("FLATPAK_ID")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        };
        let root = PathBuf::from(root);

        struct CaptureService {
            unit: PathBuf,
            daemon: PathBuf,
            home: PathBuf,
            restarted: AtomicBool,
            lose_fragment: bool,
        }
        impl service::HostCommands for CaptureService {
            fn available(&self, program: &str) -> bool {
                program == "systemctl"
            }
            fn run(&self, program: &str, args: &[String], _: Duration) -> Result<Output> {
                assert_eq!(program, "systemctl");
                assert_eq!(args.first().map(String::as_str), Some("--user"));
                let mut text = String::new();
                match args.get(1).map(String::as_str) {
                    Some("show") => {
                        assert_eq!(args[2], service::SYSTEMD_UNIT);
                        let restarted = self.restarted.load(Ordering::Acquire);
                        text = format!(
                            "LoadState=loaded\nActiveState={}\nUnitFileState={}\nFragmentPath={}\nExecStart={{ path={} ; argv[]={}; ignore_errors=no ; }}\nWorkingDirectory=!{}\n",
                            if restarted { "active" } else { "inactive" },
                            if restarted { "enabled" } else { "disabled" },
                            self.unit.display(),
                            self.daemon.display(),
                            self.daemon.display(),
                            self.home.display(),
                        );
                    }
                    Some("daemon-reload") => {}
                    Some("enable" | "reset-failed" | "restart") => {
                        assert_eq!(args[2], service::SYSTEMD_UNIT);
                        if args[1] == "restart" {
                            self.restarted.store(true, Ordering::Release);
                            if self.lose_fragment {
                                fs::remove_file(&self.unit)?;
                            }
                        }
                    }
                    _ => panic!("Unexpected host operation: {args:?}"),
                }
                Ok(Output {
                    status: ExitStatus::from_raw(0),
                    stdout: text.into_bytes(),
                    stderr: Vec::new(),
                })
            }
            fn package_owner(
                &self,
                _: &[PathBuf],
            ) -> Result<Option<crate::installation::InstallMethod>> {
                Ok(None)
            }
        }

        // A completed setup is not itself proof of service health. A failed
        // post-setup lookup must replace the old warning, not announce success.
        for healthy in [true, false] {
            let root = root.join(if healthy { "running" } else { "unavailable" });
            let prefix = root.join("install");
            let data = prefix.join("share/openwave");
            let daemon = prefix.join("bin/openwave-daemon");
            fs::create_dir_all(daemon.parent().unwrap()).unwrap();
            fs::write(&daemon, b"inert fixture; never executed").unwrap();
            fs::set_permissions(&daemon, fs::Permissions::from_mode(0o755)).unwrap();
            fs::create_dir_all(data.join("wireplumber")).unwrap();
            fs::write(
                data.join("wireplumber").join(setup::WIREPLUMBER_NAME),
                "monitor.alsa.rules = []\n",
            )
            .unwrap();
            let paths = RuntimePaths {
                executable: prefix.join("bin/openwave"),
                prefix: Some(prefix),
                data: data.clone(),
                identity: data,
                maintenance: root.join("maintenance"),
                source: None,
            };
            let unit = root.join("config/systemd/user/openwave.service");
            let host = service::HostContext {
                home: root.join("home"),
                config_home: root.join("config"),
                data_home: root.join("data"),
                udev_directory: root.join("udev"),
                runit_link: root.join("var/service/wavexlr-audio"),
                runit_definition: root.join("etc/sv/wavexlr-audio"),
                username: "fixture".into(),
                uid: 1000,
                durable_bins: Vec::new(),
                sandboxed: false,
                commands: Arc::new(CaptureService {
                    unit: unit.clone(),
                    daemon,
                    home: root.join("home"),
                    restarted: AtomicBool::new(false),
                    lose_fragment: !healthy,
                }),
            };
            fs::create_dir_all(unit.parent().unwrap()).unwrap();
            fs::write(&unit, service::render_unit(&paths, &host).unwrap()).unwrap();
            fs::create_dir_all(&host.udev_directory).unwrap();
            fs::write(
                host.udev_directory.join("99-openwave.rules"),
                setup::udev_rules(),
            )
            .unwrap();
            let initial = service::status_with(&paths, &host).unwrap();
            assert!(!initial.running);
            let previous_warning = initial.message;
            let mut displayed_warning = previous_warning.clone();
            let mut host = Some(host);
            let mut worker =
                HostWorker::start_with(paths, move || Ok(host.take().unwrap())).unwrap();
            worker
                .send(HostCommand::Setup {
                    job: 7,
                    mixes: default_mixes(),
                })
                .unwrap();
            loop {
                match worker.events.recv_timeout(Duration::from_secs(5)).unwrap() {
                    BackendEvent::Autostart { .. } => {}
                    BackendEvent::Status {
                        service,
                        setup_required,
                    } => {
                        assert!(!setup_required);
                        displayed_warning = service;
                    }
                    BackendEvent::Setup { job, result } => {
                        assert_eq!(job, 7);
                        assert!(result.is_ok(), "{result:?}");
                        break;
                    }
                    _ => panic!("Unexpected setup event"),
                }
            }
            worker.stop().unwrap();
            assert_ne!(
                displayed_warning, previous_warning,
                "setup left a stale service warning"
            );
            assert_eq!(
                displayed_warning.is_empty(),
                healthy,
                "only a confirmed running service may clear the UI warning",
            );
        }
    }

    #[test]
    fn failed_join_keeps_installation_owned_on_retry() {
        let Some(root) = std::env::var_os("OPENWAVE_NATIVE_DRAIN_FIXTURE") else {
            let root = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "controller::native::tests::failed_join_keeps_installation_owned_on_retry",
                    "--nocapture",
                ])
                .env("OPENWAVE_NATIVE_DRAIN_FIXTURE", root.path())
                .env("HOME", root.path())
                .env("XDG_CONFIG_HOME", "")
                .env("XDG_DATA_HOME", "")
                .env("XDG_STATE_HOME", "")
                .env("XDG_RUNTIME_DIR", root.path().join("runtime"))
                .env_remove("DBUS_SESSION_BUS_ADDRESS")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        };
        let identity = std::fs::canonicalize(root).unwrap();
        // Synthetic integration threads exercise the real native join and lease
        // boundary without starting any USB, desktop, audio or service worker.
        for panics in [false, true] {
            let (send, events) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                if panics {
                    panic!("integration fixture panic");
                }
                send.send(BackendEvent::Status {
                    service: "drained".into(),
                    setup_required: false,
                })
                .unwrap();
            });
            let host = HostWorker {
                sender: None,
                events,
                stopping: Arc::new(AtomicBool::new(false)),
                thread: Some(worker),
                stop_error: None,
            };
            let paths = RuntimePaths {
                executable: identity.join("openwave"),
                prefix: None,
                data: identity.clone(),
                identity: identity.clone(),
                maintenance: identity.join("maintenance"),
                source: None,
            };
            let mut backend = NativeBackend {
                paths,
                allowed_bus_owner: None,
                installation_lease: Some(Lease::installation_shared(&identity).unwrap()),
                vendor_lease: None,
                active: None,
                host: Some(host),
                queued: VecDeque::new(),
                desired: None,
                stopping: false,
                stopped: false,
            };
            assert_eq!(
                Lease::installation_exclusive(&identity).unwrap_err().code,
                ErrorCode::Busy
            );
            let first = backend.shutdown();
            if panics {
                let failure = first.unwrap_err();
                assert!(
                    failure
                        .issues
                        .iter()
                        .any(|issue| issue.target == "host integration")
                );
                assert_eq!(
                    Lease::installation_exclusive(&identity).unwrap_err().code,
                    ErrorCode::Busy
                );
                assert_eq!(backend.shutdown(), Err(failure));
                assert_eq!(
                    Lease::installation_exclusive(&identity).unwrap_err().code,
                    ErrorCode::Busy
                );
            } else {
                first.unwrap();
                drop(Lease::installation_exclusive(&identity).unwrap());
                assert!(
                    matches!(backend.next_event(), Some(BackendEvent::Status { service, .. }) if service == "drained")
                );
                backend.shutdown().unwrap();
                assert!(backend.next_event().is_none());
            }
            assert_eq!(
                backend.dispatch(BackendCommand::Rescan).unwrap_err().code,
                ErrorCode::Frozen
            );
        }
    }

    #[test]
    fn pending_meter_gates_cleanup_and_retries_preserve_independent_drain_failures() {
        use crate::{
            mixer::{GraphBackend, GraphSnapshot, RoutingChild},
            process::OwnedChild,
            recovery::Commands,
        };
        use serde_json::json;
        use std::{
            process::{Command, Stdio},
            thread,
            time::{Duration, Instant},
        };

        if rustix::process::geteuid().is_root() {
            return;
        }
        let Some(root) = std::env::var_os("OPENWAVE_COMBINED_DRAIN_FIXTURE") else {
            let root = tempfile::tempdir().unwrap();
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "controller::native::tests::pending_meter_gates_cleanup_and_retries_preserve_independent_drain_failures",
                    "--nocapture",
                ])
                .env("OPENWAVE_COMBINED_DRAIN_FIXTURE", root.path())
                .env("HOME", root.path())
                .env("XDG_CONFIG_HOME", root.path().join("config"))
                .env("XDG_DATA_HOME", root.path().join("data"))
                .env("XDG_STATE_HOME", root.path().join("state"))
                .env("XDG_RUNTIME_DIR", root.path().join("runtime"))
                .env_remove("DBUS_SESSION_BUS_ADDRESS")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        };
        let identity = std::fs::canonicalize(root).unwrap();

        // One real reconciler-owned intake, with no routes, devices or host audio.
        // Unload is an observable final cleanup mutation, not a stop-call counter.
        #[derive(Default)]
        struct Graph {
            sink_present: AtomicBool,
            refuse_unload: AtomicBool,
            unload_attempts: std::sync::atomic::AtomicUsize,
        }
        struct FixtureGraph {
            shared: Arc<Graph>,
            sink: Option<(String, String)>,
        }
        impl GraphBackend for FixtureGraph {
            fn snapshot(&mut self) -> Result<GraphSnapshot> {
                let mut objects = vec![json!({
                    "id": 0, "type": "PipeWire:Interface:Core",
                    "info": {"cookie": 1}
                })];
                let mut sinks = Vec::new();
                if let Some((name, owner)) = &self.sink {
                    objects.push(json!({
                        "id": 10, "type": "PipeWire:Interface:Node",
                        "info": {"props": {
                            "node.name": name, "object.serial": "10010",
                            "media.class": "Audio/Sink",
                            "pulse.module.id": 20, "openwave.owner": owner
                        }}
                    }));
                    sinks.push(json!({
                        "name": name, "index": 10, "description": name,
                        "mute": false, "volume": {"mono": {"value": 65536}},
                        "properties": {"object.serial": "10010"}
                    }));
                }
                GraphSnapshot::parse(
                    &json!(objects),
                    &json!(sinks),
                    &json!([]),
                    &json!([]),
                    &json!([]),
                    None,
                )
            }
            fn write_definitions(&mut self, mixes: &Mixes) -> Result<()> {
                assert!(mixes.is_empty());
                Ok(())
            }
            fn create_sink(&mut self, name: &str, _: &str, owner: &str) -> Result<u32> {
                assert!(self.sink.is_none());
                self.sink = Some((name.into(), owner.into()));
                self.shared.sink_present.store(true, Ordering::Release);
                Ok(20)
            }
            fn unload_module(&mut self, module: u32) -> Result<()> {
                assert_eq!(module, 20);
                self.shared.unload_attempts.fetch_add(1, Ordering::AcqRel);
                if self.shared.refuse_unload.load(Ordering::Acquire) {
                    return Err(OperationError::unavailable("fixture intake still owned"));
                }
                self.sink.take().expect("owned intake");
                self.shared.sink_present.store(false, Ordering::Release);
                Ok(())
            }
            fn destroy_node(&mut self, _: u32) -> Result<()> {
                panic!("no persistent node removal")
            }
            fn move_stream(&mut self, _: &StreamSnapshot, _: &str) -> Result<()> {
                panic!("no application streams")
            }
            fn link(&mut self, _: u32, _: u32) -> Result<()> {
                panic!("no routes")
            }
            fn set_level(&mut self, _: u32, _: f64, _: bool) -> Result<()> {
                panic!("no mix masters or routes")
            }
            fn set_capture_mute(&mut self, _: &CaptureSnapshot, _: bool) -> Result<()> {
                panic!("no hardware captures")
            }
            fn spawn_loopback(
                &mut self,
                _: &str,
                _: &str,
                _: Option<&str>,
            ) -> Result<Box<dyn RoutingChild>> {
                panic!("no routes")
            }
            fn spawn_filter(&mut self, _: &Path) -> Result<Box<dyn RoutingChild>> {
                panic!("no effects")
            }
        }
        struct NoHost;
        impl Commands for NoHost {
            fn run(&self, _: &str, _: &[String], _: Duration, _: bool) -> Result<Vec<u8>> {
                panic!("pre-cancelled health must not run host commands")
            }
            fn cancelled(&self) -> bool {
                true
            }
        }
        struct Release(Arc<AtomicBool>);
        impl Drop for Release {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        // Exercise each consumed join independently: one worker's sticky error
        // must neither mask meter progress nor turn into successful lease release.
        for failed_worker in [None, Some("devices"), Some("calibration")] {
            let release = Arc::new(AtomicBool::new(false));
            let _release_on_unwind = Release(release.clone());
            let gate = release.clone();
            let (spawned_tx, spawned) = mpsc::channel();
            let (meters, meter_events) = MeterMonitor::start_with_reader(move |_| {
                let child = OwnedChild::spawn("sleep", &["60".into()], Stdio::piped())?;
                spawned_tx.send(child.id()).unwrap();
                // Hold ownership after spawn, as a delayed reader handoff would.
                while !gate.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(5));
                }
                Ok(child)
            })
            .unwrap();
            let graph = Arc::new(Graph {
                refuse_unload: AtomicBool::new(true),
                ..Graph::default()
            });
            let (mixer, mixer_events) = Mixer::start_with_backend(
                Box::new(FixtureGraph {
                    shared: graph.clone(),
                    sink: None,
                }),
                meters.readiness(),
                Duration::from_millis(10),
            )
            .unwrap();
            let mut desired = DesiredState::default();
            let mut source = Source::new("Held intake".into(), SourceKind::App);
            source.id = SourceId::new("held").unwrap();
            desired.sources.insert(source.id.clone(), source);
            mixer
                .set_desired(1, Arc::new(desired), IndexMap::new())
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let event = mixer_events
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .expect("mixer published owned intake");
                if let MixerEvent::Observed(observation) = event {
                    assert!(observation.errors.is_empty(), "{:?}", observation.errors);
                    if !observation.meter_targets.is_empty() {
                        meters.set_targets(observation.meter_targets).unwrap();
                        break;
                    }
                }
            }
            let pid = spawned.recv_timeout(Duration::from_secs(5)).unwrap();
            let child_path = PathBuf::from(format!("/proc/{pid}"));
            let (scanned_tx, scanned) = mpsc::channel();
            let (devices, device_events) = DeviceManager::start_empty_with_scan(move || {
                scanned_tx.send(()).unwrap();
                assert_ne!(failed_worker, Some("devices"), "fixture discovery panic");
            })
            .unwrap();
            scanned.recv_timeout(Duration::from_secs(5)).unwrap();
            let (capturing_tx, capturing) = mpsc::channel();
            let (calibration, calibration_events) =
                CalibrationWorker::start_with(Arc::new(move |_, _, _| {
                    capturing_tx.send(()).unwrap();
                    panic!("fixture calibration panic")
                }))
                .unwrap();
            if failed_worker == Some("calibration") {
                calibration
                    .record(
                        CalibrationToken {
                            session: 1,
                            source: SourceId::new("held").unwrap(),
                            node_name: "fixture_raw".into(),
                            identity: NodeIdentity {
                                server_cookie: 1,
                                object_serial: "fixture".into(),
                            },
                            channels: 1,
                        },
                        openwave_core::calibration::FLOOR_SECONDS,
                    )
                    .unwrap();
                capturing.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            let health = HealthMonitor::start_with(
                false,
                Arc::new(std::collections::HashMap::new),
                Arc::new(AtomicBool::new(true)),
                NoHost,
            )
            .unwrap();
            let mut backend = NativeBackend {
                paths: RuntimePaths {
                    executable: identity.join("openwave"),
                    prefix: None,
                    data: identity.clone(),
                    identity: identity.clone(),
                    maintenance: identity.join("maintenance"),
                    source: None,
                },
                allowed_bus_owner: None,
                installation_lease: Some(Lease::installation_shared(&identity).unwrap()),
                vendor_lease: None,
                active: Some(ActiveWorkers {
                    devices,
                    device_events,
                    mixer,
                    mixer_events,
                    meters,
                    meter_events,
                    calibration,
                    calibration_events,
                    health,
                    drain_issues: None,
                }),
                host: None,
                queued: VecDeque::new(),
                desired: None,
                stopping: false,
                stopped: false,
            };
            // Release before NativeBackend's panic cleanup attempts its drain.
            let _release_before_backend_drop = Release(release.clone());
            let mut sticky = Vec::new();
            for attempt in 0..2 {
                let start = Instant::now();
                let failure = backend.shutdown().expect_err("held child is not drained");
                assert!(
                    start.elapsed() < Duration::from_secs(4),
                    "unbounded meter drain"
                );
                assert!(failure.issues.iter().any(|issue| issue.target == "meters"));
                let independent: Vec<_> = failure
                    .issues
                    .iter()
                    .filter(|issue| issue.target != "meters")
                    .cloned()
                    .collect();
                if attempt == 0 {
                    assert_eq!(independent.len(), usize::from(failed_worker.is_some()));
                    if let Some(target) = failed_worker {
                        assert_eq!(independent[0].target, target);
                    }
                    sticky = independent;
                } else {
                    assert_eq!(independent, sticky);
                }
                assert!(child_path.exists(), "held owned child was abandoned");
                assert!(
                    graph.sink_present.load(Ordering::Acquire),
                    "intake removed before reader drained"
                );
                assert_eq!(
                    graph.unload_attempts.load(Ordering::Acquire),
                    0,
                    "final mixer cleanup started early"
                );
                assert_eq!(
                    Lease::installation_exclusive(&identity).unwrap_err().code,
                    ErrorCode::Busy
                );
            }
            release.store(true, Ordering::Release);
            let failure = backend
                .shutdown()
                .expect_err("intake cleanup still pending");
            assert!(
                !child_path.exists(),
                "completed meter retry did not reap child"
            );
            assert!(!failure.issues.iter().any(|issue| issue.target == "meters"));
            assert!(failure.issues.iter().any(|issue| issue.target == "mixer"));
            assert_eq!(
                failure
                    .issues
                    .iter()
                    .filter(|issue| issue.target != "mixer")
                    .cloned()
                    .collect::<Vec<_>>(),
                sticky,
            );
            assert!(graph.unload_attempts.load(Ordering::Acquire) > 0);
            assert!(
                graph.sink_present.load(Ordering::Acquire),
                "failed cleanup discarded intake"
            );
            graph.refuse_unload.store(false, Ordering::Release);
            assert_eq!(
                Lease::installation_exclusive(&identity).unwrap_err().code,
                ErrorCode::Busy
            );
            let result = backend.shutdown();
            assert!(
                !graph.sink_present.load(Ordering::Acquire),
                "retry left intake owned"
            );
            if failed_worker.is_some() {
                let failure = result.unwrap_err();
                assert_eq!(failure.issues, sticky);
                assert_eq!(backend.shutdown(), Err(failure));
                assert_eq!(
                    Lease::installation_exclusive(&identity).unwrap_err().code,
                    ErrorCode::Busy
                );
            } else {
                result.unwrap();
                drop(Lease::installation_exclusive(&identity).unwrap());
                backend.shutdown().unwrap();
            }
        }
    }
}

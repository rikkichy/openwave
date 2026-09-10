use crate::process::OwnedChild;
use openwave_core::model::{NodeIdentity, OperationError, Result};
use std::{
    collections::{HashMap, HashSet},
    io::Read,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, PartialEq)]
pub struct MeterTarget {
    pub key: String,
    pub node_name: String,
    pub identity: NodeIdentity,
    pub raw: bool,
    pub channels: u32,
}
#[derive(Clone, Debug)]
pub struct MeterEvent {
    pub key: String,
    pub identity: NodeIdentity,
    pub generation: u64,
    pub peak: f64,
}

#[derive(Default)]
struct ReadinessBank {
    next: u64,
    taps: HashMap<u64, Flow>,
}
struct Flow {
    identity: NodeIdentity,
    started: Instant,
    last_data: Instant,
    received: bool,
    grace: Duration,
    active: bool,
}
/// Byte observations are scoped to a tap and the exact server/node generation.
#[derive(Clone, Default)]
pub struct CaptureReadiness {
    inner: Arc<Mutex<ReadinessBank>>,
}
impl CaptureReadiness {
    pub fn ready(&self, identity: &NodeIdentity) -> bool {
        self.inner.lock().is_ok_and(|bank| {
            bank.taps
                .values()
                .any(|v| &v.identity == identity && v.received)
        })
    }
    pub fn gaps(&self) -> HashMap<NodeIdentity, Duration> {
        self.gaps_at(Instant::now())
    }
    pub fn gaps_at(&self, now: Instant) -> HashMap<NodeIdentity, Duration> {
        let mut gaps: HashMap<NodeIdentity, Duration> = HashMap::new();
        let Ok(bank) = self.inner.lock() else {
            return gaps;
        };
        for flow in bank.taps.values() {
            if !flow.active || now.saturating_duration_since(flow.started) < flow.grace {
                continue;
            }
            let age = now.saturating_duration_since(flow.last_data);
            gaps.entry(flow.identity.clone())
                .and_modify(|old| *old = (*old).min(age))
                .or_insert(age);
        }
        gaps
    }
    /// Keep the returned registration only for the lifetime of its actual reader.
    pub fn register(
        &self,
        identity: NodeIdentity,
        now: Instant,
        grace: Duration,
    ) -> Result<CaptureTap> {
        let mut bank = self
            .inner
            .lock()
            .map_err(|_| OperationError::unavailable("capture readiness bank poisoned"))?;
        bank.next = bank
            .next
            .checked_add(1)
            .ok_or_else(|| OperationError::unavailable("capture tap counter exhausted"))?;
        let id = bank.next;
        bank.taps.insert(
            id,
            Flow {
                identity,
                started: now,
                last_data: now,
                received: false,
                grace,
                active: false,
            },
        );
        Ok(CaptureTap {
            bank: self.clone(),
            id,
        })
    }
}
pub struct CaptureTap {
    bank: CaptureReadiness,
    id: u64,
}
impl CaptureTap {
    fn registered(&self) -> bool {
        self.bank
            .inner
            .lock()
            .is_ok_and(|bank| bank.taps.contains_key(&self.id))
    }
    pub fn started(&self, now: Instant, previous_flow: Option<Instant>) -> Result<()> {
        let mut bank = self
            .bank
            .inner
            .lock()
            .map_err(|_| OperationError::unavailable("capture readiness bank poisoned"))?;
        if let Some(flow) = bank.taps.get_mut(&self.id) {
            flow.started = now;
            flow.last_data = previous_flow.unwrap_or(now);
            flow.active = true;
        }
        Ok(())
    }
    /// Any bytes count as flow, including zeros and an incomplete PCM frame.
    pub fn received(&self, now: Instant) -> Result<()> {
        let mut bank = self
            .bank
            .inner
            .lock()
            .map_err(|_| OperationError::unavailable("capture readiness bank poisoned"))?;
        if let Some(flow) = bank.taps.get_mut(&self.id) {
            flow.last_data = now;
            flow.received = true;
            flow.active = true;
        }
        Ok(())
    }
    pub fn invalidate(&self) {
        self.bank
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .taps
            .remove(&self.id);
    }
}
impl Drop for CaptureTap {
    fn drop(&mut self) {
        self.invalidate();
    }
}

/// Streaming little-endian s16 decoder. A single trailing byte is retained;
/// widening before abs also handles i16::MIN without overflow.
#[derive(Default)]
pub struct PcmPeak {
    pending: Option<u8>,
}
impl PcmPeak {
    pub fn push(&mut self, bytes: &[u8]) -> Option<f64> {
        let mut peak = 0u32;
        let mut samples = 0usize;
        let mut offset = 0;
        if let (Some(lo), Some(&hi)) = (self.pending, bytes.first()) {
            peak = i32::from(i16::from_le_bytes([lo, hi])).unsigned_abs();
            self.pending = None;
            offset = 1;
            samples = 1;
        }
        for pair in bytes[offset..].chunks_exact(2) {
            peak = peak.max(i32::from(i16::from_le_bytes([pair[0], pair[1]])).unsigned_abs());
            samples += 1;
        }
        if (bytes.len() - offset) % 2 != 0 {
            self.pending = bytes.last().copied();
        }
        (samples != 0).then_some(f64::from(peak) / 32768.0)
    }
}

pub(crate) fn nonblocking(stdout: &std::process::ChildStdout) -> Result<()> {
    let flags = rustix::fs::fcntl_getfl(stdout).map_err(std::io::Error::from)?;
    rustix::fs::fcntl_setfl(stdout, flags | rustix::fs::OFlags::NONBLOCK)
        .map_err(std::io::Error::from)?;
    Ok(())
}

type Targets = HashMap<String, (MeterTarget, u64, Arc<CaptureTap>)>;

// Only unit fixtures may substitute an exclusively owned reader child.
#[derive(Clone, Default)]
struct ReaderSpawner {
    #[cfg(test)]
    fixture: Option<Arc<dyn Fn(&[String]) -> Result<OwnedChild> + Send + Sync>>,
}
impl ReaderSpawner {
    fn spawn(&self, args: &[String]) -> Result<OwnedChild> {
        #[cfg(test)]
        if let Some(fixture) = &self.fixture {
            return fixture(args);
        }
        OwnedChild::spawn("pw-cat", args, Stdio::piped())
    }
}

const STOP_WAIT: Duration = Duration::from_secs(2);
struct Worker {
    token: u64,
    cancel: Arc<AtomicBool>,
    tap: Arc<CaptureTap>,
    join: JoinHandle<Result<()>>,
}
pub struct MeterMonitor {
    targets: Arc<Mutex<Targets>>,
    next: AtomicU64,
    wake: mpsc::Sender<()>,
    cancel: Arc<AtomicBool>,
    readiness: CaptureReadiness,
    join: Option<JoinHandle<Result<()>>>,
    stop_error: Option<OperationError>,
}
impl MeterMonitor {
    pub fn start() -> Result<(Self, mpsc::Receiver<MeterEvent>)> {
        Self::start_with_spawner(ReaderSpawner::default())
    }
    #[cfg(test)]
    pub(crate) fn start_with_reader(
        spawn: impl Fn(&[String]) -> Result<OwnedChild> + Send + Sync + 'static,
    ) -> Result<(Self, mpsc::Receiver<MeterEvent>)> {
        Self::start_with_spawner(ReaderSpawner {
            fixture: Some(Arc::new(spawn)),
        })
    }
    fn start_with_spawner(spawner: ReaderSpawner) -> Result<(Self, mpsc::Receiver<MeterEvent>)> {
        let targets = Arc::new(Mutex::new(Targets::new()));
        let cancel = Arc::new(AtomicBool::new(false));
        let readiness = CaptureReadiness::default();
        let (wake, commands) = mpsc::channel();
        let (events, receiver) = mpsc::channel();
        let (worker_targets, worker_cancel) = (targets.clone(), cancel.clone());
        let join = thread::Builder::new()
            .name("openwave-meters".into())
            .spawn(move || meter_loop(worker_targets, worker_cancel, commands, events, spawner))?;
        Ok((
            Self {
                targets,
                next: AtomicU64::new(1),
                wake,
                cancel,
                readiness,
                join: Some(join),
                stop_error: None,
            },
            receiver,
        ))
    }
    pub fn set_targets(&self, targets: Vec<MeterTarget>) -> Result<()> {
        let mut keys = HashSet::new();
        for target in &targets {
            if target.key.is_empty()
                || target.node_name.is_empty()
                || !matches!(target.channels, 1 | 2)
                || !keys.insert(&target.key)
            {
                return Err(OperationError::invalid(
                    "meter targets require unique keys, node names and mono/stereo channels",
                ));
            }
        }
        drop(keys);
        if self.cancel.load(Ordering::Acquire) {
            return Err(OperationError::unavailable("meters stopped"));
        }
        let mut current = self
            .targets
            .lock()
            .map_err(|_| OperationError::unavailable("meter target bank poisoned"))?;
        let mut next = HashMap::with_capacity(targets.len());
        for target in targets {
            let (token, tap) = if let Some((_, token, tap)) = current
                .get(&target.key)
                .filter(|(old, _, tap)| old == &target && tap.registered())
            {
                (*token, tap.clone())
            } else {
                (
                    self.next.fetch_add(1, Ordering::Relaxed),
                    Arc::new(self.readiness.register(
                        target.identity.clone(),
                        Instant::now(),
                        Duration::ZERO,
                    )?),
                )
            };
            next.insert(target.key.clone(), (target, token, tap));
        }
        for (key, (_, token, tap)) in current.iter() {
            if next
                .get(key)
                .is_none_or(|(_, next_token, _)| next_token != token)
            {
                tap.invalidate();
            }
        }
        *current = next;
        self.wake
            .send(())
            .map_err(|_| OperationError::unavailable("meter worker exited"))
    }
    pub fn readiness(&self) -> CaptureReadiness {
        self.readiness.clone()
    }
    /// Check again when consuming queued events: retirement cannot retract bytes
    /// already delivered to std::mpsc, and consumers must reject old identities.
    pub fn accepts(&self, event: &MeterEvent) -> bool {
        !self.cancel.load(Ordering::Acquire)
            && self.targets.lock().is_ok_and(|targets| {
                targets.get(&event.key).is_some_and(|(target, token, tap)| {
                    target.identity == event.identity
                        && *token == event.generation
                        && (tap.registered() || event.peak == 0.0)
                })
            })
    }
    pub fn stop(&mut self) -> Result<()> {
        self.cancel.store(true, Ordering::Release);
        let mut targets = self.targets.lock().unwrap_or_else(|e| e.into_inner());
        for (_, _, tap) in targets.values() {
            tap.invalidate();
        }
        targets.clear();
        drop(targets);
        let _ = self.wake.send(());
        let deadline = Instant::now() + STOP_WAIT;
        while self.join.as_ref().is_some_and(|join| !join.is_finished()) {
            if Instant::now() >= deadline {
                return Err(OperationError::unavailable("meter readers still draining"));
            }
            thread::sleep(Duration::from_millis(10));
        }
        match self.join.take() {
            Some(join) => {
                let result = join
                    .join()
                    .map_err(|_| OperationError::unavailable("meter coordinator panicked"))
                    .and_then(|result| result);
                self.stop_error = result.as_ref().err().cloned();
                result
            }
            None => self.stop_error.clone().map_or(Ok(()), Err),
        }
    }
}
impl Drop for MeterMonitor {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn stop_worker(worker: Worker) -> Result<()> {
    worker.cancel.store(true, Ordering::Release);
    worker.tap.invalidate();
    worker
        .join
        .join()
        .map_err(|_| OperationError::unavailable("meter reader panicked"))?
}
fn meter_loop(
    targets: Arc<Mutex<Targets>>,
    cancel: Arc<AtomicBool>,
    commands: mpsc::Receiver<()>,
    events: mpsc::Sender<MeterEvent>,
    spawner: ReaderSpawner,
) -> Result<()> {
    let mut workers: HashMap<String, Worker> = HashMap::new();
    let mut errors = Vec::new();
    while !cancel.load(Ordering::Acquire) {
        let desired = match targets.lock() {
            Ok(targets) => targets.clone(),
            Err(_) => {
                errors.push("meter target bank poisoned".into());
                break;
            }
        };
        let retired: Vec<_> = workers
            .iter()
            .filter(|(key, worker)| {
                desired
                    .get(*key)
                    .is_none_or(|(_, token, _)| *token != worker.token)
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in retired {
            if let Err(error) = stop_worker(workers.remove(&key).unwrap()) {
                log::warn!("Meter {key}: {error}");
                errors.push(error.to_string());
            }
        }
        for (key, (target, token, tap)) in desired {
            if workers.contains_key(&key) || cancel.load(Ordering::Acquire) {
                continue;
            }
            let stopped = Arc::new(AtomicBool::new(false));
            let (reader_target, reader_targets, reader_cancel, reader_tap, reader_events) = (
                target.clone(),
                targets.clone(),
                stopped.clone(),
                tap.clone(),
                events.clone(),
            );
            let reader_spawner = spawner.clone();
            match thread::Builder::new()
                .name(format!("openwave-meter-{key}"))
                .spawn(move || {
                    meter_reader(
                        reader_target,
                        token,
                        reader_targets,
                        reader_cancel,
                        reader_tap,
                        reader_events,
                        reader_spawner,
                    )
                }) {
                Ok(join) => {
                    workers.insert(
                        key,
                        Worker {
                            token,
                            cancel: stopped,
                            tap,
                            join,
                        },
                    );
                }
                Err(error) => {
                    tap.invalidate();
                    log::error!("Starting meter {key}: {error}");
                }
            }
        }
        if commands.recv().is_err() {
            break;
        }
        while commands.try_recv().is_ok() {}
    }
    for (_, worker) in workers {
        if let Err(error) = stop_worker(worker) {
            errors.push(error.to_string());
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(OperationError::unavailable(errors.join("; ")))
    }
}
fn meter_reader(
    target: MeterTarget,
    token: u64,
    targets: Arc<Mutex<Targets>>,
    cancel: Arc<AtomicBool>,
    tap: Arc<CaptureTap>,
    events: mpsc::Sender<MeterEvent>,
    spawner: ReaderSpawner,
) -> Result<()> {
    let props = serde_json::json!({"node.name": format!("openwave_meter_{}", target.key), "node.description": format!("OpenWave level meter ({})", target.key), "application.name":"OpenWave", "media.name":format!("OpenWave meter: {}", target.key), "node.dont-fallback":true, "node.dont-reconnect":true, "node.dont-move":true, "stream.capture.sink": !target.raw});
    let args = vec![
        "--record".into(),
        "--target".into(),
        target.identity.object_serial.clone(),
        "--properties".into(),
        props.to_string(),
        "--rate".into(),
        "8000".into(),
        "--channels".into(),
        // Match the Python meter: let PipeWire downmix before s16 decoding.
        // Calibration deliberately keeps its independent channel capture.
        "1".into(),
        "--format".into(),
        "s16".into(),
        "-".into(),
    ];
    let result = (|| -> Result<()> {
        if cancel.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut child = spawner.spawn(&args)?;
        let reading = (|| -> Result<()> {
            if target.raw {
                tap.started(Instant::now(), None)?;
            }
            let mut stdout = child
                .take_stdout()
                .ok_or_else(|| OperationError::unavailable("meter stdout unavailable"))?;
            nonblocking(&stdout)?;
            let mut buffer = [0u8; 1024];
            let mut decoder = PcmPeak::default();
            let mut tail = 0;
            let mut settled = false;
            while !cancel.load(Ordering::Acquire) {
                match stdout.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(size) => {
                        let active = targets.lock().map_err(|_| {
                            OperationError::unavailable("meter target bank poisoned")
                        })?;
                        if active
                            .get(&target.key)
                            .is_none_or(|(_, active_token, _)| *active_token != token)
                        {
                            break;
                        }
                        if target.raw {
                            tap.received(Instant::now())?;
                        }
                        if let Some(mut peak) = decoder.push(&buffer[..size]) {
                            if peak >= 0.004 {
                                tail = 20;
                                settled = false;
                            } else if tail > 0 {
                                tail -= 1;
                            } else if settled {
                                continue;
                            } else {
                                peak = 0.0;
                                settled = true;
                            }
                            let _ = events.send(MeterEvent {
                                key: target.key.clone(),
                                identity: target.identity.clone(),
                                generation: token,
                                peak,
                            });
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error.into()),
                }
            }
            Ok(())
        })();
        // EOF and cancellation retire byte readiness immediately, even if a
        // bounded termination attempt leaves the owned child awaiting reap.
        tap.invalidate();
        while let Err(error) = child.terminate() {
            log::warn!("Meter {} still draining: {error}", target.key);
            thread::sleep(Duration::from_millis(10));
        }
        reading
    })();
    tap.invalidate();
    if let Err(error) = &result {
        log::warn!("Meter {}: {error}", target.key);
    }
    let active = targets
        .lock()
        .map_err(|_| OperationError::unavailable("meter target bank poisoned"))?;
    if !cancel.load(Ordering::Acquire)
        && active
            .get(&target.key)
            .is_some_and(|(_, active_token, _)| *active_token == token)
    {
        let _ = events.send(MeterEvent {
            key: target.key,
            identity: target.identity,
            generation: token,
            peak: 0.0,
        });
    }
    result
}

#[cfg(test)]
mod lifecycle {
    use super::*;

    fn target(key: &str, serial: &str) -> MeterTarget {
        MeterTarget {
            key: key.into(),
            node_name: format!("capture_{serial}"),
            identity: NodeIdentity {
                server_cookie: 1,
                object_serial: serial.into(),
            },
            raw: true,
            channels: 1,
        }
    }

    struct Spawned {
        input: std::fs::File,
        pid: u32,
    }

    struct Rig {
        monitor: MeterMonitor,
        events: mpsc::Receiver<MeterEvent>,
        spawned: mpsc::Receiver<Spawned>,
        release: Arc<AtomicBool>,
        _directory: tempfile::TempDir,
    }
    impl Rig {
        fn new(held: bool) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().to_owned();
            let (spawned_tx, spawned) = mpsc::channel();
            let release = Arc::new(AtomicBool::new(!held));
            let gate = release.clone();
            let next = AtomicU64::new(0);
            let spawner = ReaderSpawner {
                fixture: Some(Arc::new(move |_| {
                    let fifo = root.join(next.fetch_add(1, Ordering::Relaxed).to_string());
                    rustix::fs::mknodat(
                        rustix::fs::CWD,
                        &fifo,
                        rustix::fs::FileType::Fifo,
                        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
                        0,
                    )
                    .map_err(std::io::Error::from)?;
                    let input = std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(&fifo)?;
                    // The ignored disposition survives exec: shutdown must escalate
                    // and reap this exact owned child, not merely send SIGTERM.
                    let child = OwnedChild::spawn(
                        "sh",
                        &[
                            "-c".into(),
                            "trap '' TERM; exec cat \"$1\"".into(),
                            "meter-fixture".into(),
                            fifo.to_string_lossy().into_owned(),
                        ],
                        Stdio::piped(),
                    )?;
                    spawned_tx
                        .send(Spawned {
                            input,
                            pid: child.id(),
                        })
                        .map_err(|_| OperationError::unavailable("fixture receiver closed"))?;
                    // Rig's panic cleanup releases even a spawn held after
                    // acquiring its exclusively owned child.
                    while !gate.load(Ordering::Acquire) {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Ok(child)
                })),
            };
            let (monitor, events) = MeterMonitor::start_with_spawner(spawner).unwrap();
            Self {
                monitor,
                events,
                spawned,
                release,
                _directory: directory,
            }
        }
        fn child(&self) -> Spawned {
            self.spawned
                .recv_timeout(Duration::from_secs(5))
                .expect("reader spawned")
        }
        fn event(&self) -> MeterEvent {
            self.events
                .recv_timeout(Duration::from_secs(5))
                .expect("reader event")
        }
        fn stop(&mut self, pids: &[u32]) {
            let start = Instant::now();
            self.monitor.stop().expect("owned readers reaped");
            assert!(start.elapsed() < STOP_WAIT + Duration::from_secs(1));
            for pid in pids {
                assert!(
                    !std::path::Path::new(&format!("/proc/{pid}")).exists(),
                    "owned child {pid} not reaped"
                );
            }
            assert!(self.monitor.readiness().gaps().is_empty());
        }
    }
    impl Drop for Rig {
        fn drop(&mut self) {
            self.release.store(true, Ordering::Release);
            let _ = self.monitor.stop();
        }
    }
    fn await_condition(mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !predicate() {
            assert!(Instant::now() < deadline, "reader condition timed out");
            thread::sleep(Duration::from_millis(5));
        }
    }
    fn bytes(child: &mut Spawned, data: &[u8]) {
        use std::io::Write;
        child.input.write_all(data).unwrap();
    }

    #[test]
    fn actual_reader_replacement_fences_events_and_partial_byte_readiness() {
        if rustix::process::geteuid().is_root() {
            return;
        }
        let mut rig = Rig::new(false);
        let original = target("src:a", "same-serial");
        rig.monitor.set_targets(vec![original.clone()]).unwrap();
        let mut old = rig.child();
        assert!(!rig.monitor.readiness().ready(&original.identity));
        bytes(&mut old, &[0]);
        await_condition(|| rig.monitor.readiness().ready(&original.identity));
        assert!(
            rig.events.try_recv().is_err(),
            "partial sample emitted a peak"
        );
        bytes(&mut old, &[64]);
        let old_peak = rig.event();
        assert_eq!(old_peak.peak, 0.5);
        assert!(rig.monitor.accepts(&old_peak));
        let other = target("src:b", "independent");
        rig.monitor
            .set_targets(vec![original.clone(), other.clone()])
            .unwrap();
        let mut independent = rig.child();
        bytes(&mut independent, &[0, 16]);
        let other_peak = rig.event();
        assert_eq!(other_peak.identity, other.identity);
        assert_eq!(other_peak.peak, 0.125);
        let mut replacement = original.clone();
        replacement.identity.server_cookie += 1;
        rig.monitor
            .set_targets(vec![replacement.clone(), other.clone()])
            .unwrap();
        assert!(!rig.monitor.accepts(&old_peak));
        assert!(!rig.monitor.readiness().ready(&original.identity));
        assert!(rig.monitor.readiness().ready(&other.identity));
        assert!(rig.monitor.accepts(&other_peak));
        let mut new = rig.child();
        assert!(!std::path::Path::new(&format!("/proc/{}", old.pid)).exists());
        assert!(!rig.monitor.readiness().ready(&replacement.identity));
        bytes(&mut new, &[0, 0]);
        let silent = rig.event();
        assert_eq!(silent.peak, 0.0);
        assert!(rig.monitor.accepts(&silent));
        assert!(rig.monitor.readiness().ready(&replacement.identity));
        let before = Instant::now();
        bytes(&mut new, &[0]);
        // Silent visual suppression must not suppress incomplete-byte flow.
        await_condition(|| {
            rig.monitor
                .readiness()
                .gaps()
                .get(&replacement.identity)
                .is_some_and(|age| *age < before.elapsed())
        });
        assert!(rig.events.try_recv().is_err());
        bytes(&mut new, &[32]);
        let peak = rig.event();
        assert_eq!(peak.peak, 0.25);
        assert!(rig.monitor.accepts(&peak));
        rig.stop(&[old.pid, new.pid, independent.pid]);
        assert!(!rig.monitor.accepts(&peak));
        assert!(!rig.monitor.accepts(&silent));
    }

    #[test]
    fn actual_reader_exit_clears_readiness_and_restart_fences_final_zero() {
        if rustix::process::geteuid().is_root() {
            return;
        }
        let mut rig = Rig::new(false);
        let capture = target("src:a", "capture");
        rig.monitor.set_targets(vec![capture.clone()]).unwrap();
        let mut first = rig.child();
        bytes(&mut first, &[0, 64]);
        let peak = rig.event();
        let first_pid = first.pid;
        drop(first); // EOF, not coordinator cancellation.
        let zero = rig.event();
        assert_eq!(zero.peak, 0.0);
        assert!(rig.monitor.accepts(&zero));
        assert!(!rig.monitor.accepts(&peak));
        assert!(!rig.monitor.readiness().ready(&capture.identity));
        assert!(!std::path::Path::new(&format!("/proc/{first_pid}")).exists());
        rig.monitor.set_targets(vec![capture.clone()]).unwrap();
        assert!(!rig.monitor.accepts(&zero));
        let mut second = rig.child();
        assert!(!rig.monitor.readiness().ready(&capture.identity));
        bytes(&mut second, &[0, 32]);
        let restarted = rig.event();
        assert_eq!(restarted.peak, 0.25);
        assert_ne!(restarted.generation, peak.generation);
        assert!(rig.monitor.accepts(&restarted));
        rig.stop(&[first_pid, second.pid]);
    }

    #[test]
    fn held_spawn_shutdown_is_bounded_and_retry_waits_for_owned_reap() {
        if rustix::process::geteuid().is_root() {
            return;
        }
        let mut rig = Rig::new(true);
        let capture = target("src:a", "capture");
        rig.monitor.set_targets(vec![capture.clone()]).unwrap();
        let child = rig.child();
        for _ in 0..2 {
            let start = Instant::now();
            assert!(rig.monitor.stop().is_err(), "held child reported drained");
            assert!(start.elapsed() < STOP_WAIT + Duration::from_secs(1));
            assert!(std::path::Path::new(&format!("/proc/{}", child.pid)).exists());
            assert!(!rig.monitor.readiness().ready(&capture.identity));
        }
        rig.release.store(true, Ordering::Release);
        rig.stop(&[child.pid]);
        assert!(
            rig.events.try_recv().is_err(),
            "retired spawn emitted a final zero"
        );
        rig.monitor.stop().unwrap();
    }
    #[test]
    fn poisoned_readiness_is_unknown_not_a_healthy_age() {
        let bank = CaptureReadiness::default();
        let id = target("src:a", "old").identity;
        let tap = bank
            .register(id.clone(), Instant::now(), Duration::ZERO)
            .expect("tap");
        tap.received(Instant::now()).expect("bytes");
        let _ = std::panic::catch_unwind(|| {
            let _guard = bank.inner.lock().expect("bank");
            panic!("simulated interrupted bank mutation");
        });
        assert!(!bank.ready(&id));
        assert!(bank.gaps().is_empty());
        assert!(bank.register(id, Instant::now(), Duration::ZERO).is_err());
        assert!(tap.received(Instant::now()).is_err());
        tap.invalidate();
    }
}

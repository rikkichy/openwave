// Serialized, incarnation-scoped USB owners. Callers must hold the installation
// and vendor-control leases before starting discovery or opening a raw device.
use crate::process::CommandRunner;
use openwave_core::{
    model::{
        DeviceSetting, ErrorCode, Observation, OperationError, OperationIssue, Result, UnitId,
        UnitSnapshot,
    },
    profiles::ProfileId,
    protocol::{
        self, AlsaCardIdentity, AlsaControl, AlsaControls, ConfigBuffer, DeviceInfo, DeviceState,
        MeterLevels,
    },
};
use rusb::UsbContext;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const POLL: Duration = Duration::from_millis(100);
const SCAN: Duration = Duration::from_secs(2);
const ALSA_POLL: Duration = Duration::from_millis(500);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(3);
static INCARNATION: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub enum DeviceEvent {
    Connected(UnitSnapshot),
    Observed(UnitSnapshot),
    Retired(UnitId),
    Completed {
        job: u64,
        unit: UnitId,
        result: Result<DeviceState>,
    },
    Error {
        unit: Option<UnitId>,
        error: OperationError,
    },
}

trait Transport: Send {
    fn read(&mut self, selector: u16, bytes: &mut [u8]) -> Result<usize>;
    fn write(&mut self, selector: u16, bytes: &[u8]) -> Result<usize>;
    fn unresponsive(&self) -> bool;
}
struct UsbTransport {
    handle: rusb::DeviceHandle<rusb::Context>,
    profile: ProfileId,
    timed_out: bool,
}
impl UsbTransport {
    fn error(&mut self, error: rusb::Error) -> OperationError {
        self.timed_out = error == rusb::Error::Timeout;
        OperationError::unavailable(format!("USB control transfer: {error}"))
    }
}
impl Transport for UsbTransport {
    fn read(&mut self, selector: u16, bytes: &mut [u8]) -> Result<usize> {
        let count = self
            .handle
            .read_control(
                protocol::RT_CLASS_IN,
                protocol::BREQUEST_READ,
                selector,
                self.profile.profile().windex,
                bytes,
                protocol::TRANSFER_TIMEOUT,
            )
            .map_err(|e| self.error(e))?;
        self.timed_out = false;
        Ok(count)
    }
    fn write(&mut self, selector: u16, bytes: &[u8]) -> Result<usize> {
        let count = self
            .handle
            .write_control(
                protocol::RT_CLASS_OUT,
                protocol::BREQUEST_WRITE,
                selector,
                self.profile.profile().windex,
                bytes,
                protocol::TRANSFER_TIMEOUT,
            )
            .map_err(|e| self.error(e))?;
        self.timed_out = false;
        Ok(count)
    }
    fn unresponsive(&self) -> bool {
        self.timed_out
    }
}

/// Raw, known-profile transport for explicitly authorized probing/diagnostics.
/// None of these reads synchronize ALSA or write USB. No interface is claimed,
/// detached, reset or reconfigured. A raw write requires separate caller consent.
pub struct VendorDevice {
    pub unit: UnitId,
    transport: Box<dyn Transport>,
}
impl VendorDevice {
    pub fn scan() -> Result<Vec<(ProfileId, u8, u8)>> {
        let context = rusb::Context::new().map_err(usb_error)?;
        let devices = context.devices().map_err(usb_error)?;
        let mut identities = Vec::new();
        for device in devices.iter() {
            // An unreadable descriptor is unknown discovery, not an unplug.
            let descriptor = device.device_descriptor().map_err(usb_error)?;
            identities.push(protocol::UsbIdentity {
                vid: descriptor.vendor_id(),
                pid: descriptor.product_id(),
                bus: device.bus_number(),
                address: device.address(),
            });
        }
        Ok(protocol::supported_devices(identities))
    }
    pub fn open(unit: UnitId) -> Result<Self> {
        let context = rusb::Context::new().map_err(usb_error)?;
        let devices = context.devices().map_err(usb_error)?;
        let p = unit.profile.profile();
        for device in devices.iter() {
            if device.bus_number() != unit.bus || device.address() != unit.address {
                continue;
            }
            let descriptor = device.device_descriptor().map_err(usb_error)?;
            if descriptor.vendor_id() != p.vid || descriptor.product_id() != p.pid {
                return Err(OperationError::new(
                    ErrorCode::Identity,
                    "USB address now belongs to another device",
                ));
            }
            let handle = device.open().map_err(usb_error)?;
            return Ok(Self {
                unit,
                transport: Box::new(UsbTransport {
                    handle,
                    profile: unit.profile,
                    timed_out: false,
                }),
            });
        }
        Err(OperationError::unavailable("Captured USB device is absent"))
    }
    /// Returns the actual transfer length, including valid short diagnostic dumps.
    pub fn read_raw(&mut self, selector: u16, bytes: &mut [u8]) -> Result<usize> {
        if bytes.is_empty() || bytes.len() > u16::MAX as usize {
            return Err(OperationError::invalid(
                "USB read length must be in 1..65535",
            ));
        }
        let count = self.transport.read(selector, bytes)?;
        if count > bytes.len() {
            return Err(OperationError::invalid("USB transfer exceeded buffer"));
        }
        Ok(count)
    }
    /// Writes a real unchanged or patched profile config, never an arbitrary block.
    pub fn write_config_raw(&mut self, bytes: &[u8]) -> Result<()> {
        ConfigBuffer::decode(self.unit.profile, bytes)?;
        let count = self
            .transport
            .write(self.unit.profile.profile().wvalue_config, bytes)?;
        if count != bytes.len() {
            return Err(short_transfer(count, bytes.len()));
        }
        Ok(())
    }
    pub fn read_config(&mut self) -> Result<ConfigBuffer> {
        let mut bytes = [0; protocol::MAX_CONFIG_LEN];
        let p = self.unit.profile.profile();
        let count = self.read_raw(p.wvalue_config, &mut bytes[..p.config_len])?;
        ConfigBuffer::decode(self.unit.profile, &bytes[..count])
    }
    pub fn read_info(&mut self) -> Result<DeviceInfo> {
        let mut bytes = [0; protocol::MAX_INFO_LEN];
        let p = self.unit.profile.profile();
        let count = self.read_raw(p.wvalue_devinfo, &mut bytes[..p.devinfo_len])?;
        protocol::decode_device_info(self.unit.profile, &bytes[..count])
    }
    pub fn read_meters(&mut self) -> Result<MeterLevels> {
        let mut bytes = [0; protocol::MAX_METER_LEN];
        let p = self.unit.profile.profile();
        let count = self.read_raw(p.wvalue_meter, &mut bytes[..p.meter_len])?;
        protocol::decode_meters(self.unit.profile, &bytes[..count])
    }
}
fn usb_error(error: rusb::Error) -> OperationError {
    OperationError::unavailable(format!("USB discovery/open: {error}"))
}
fn short_transfer(actual: usize, expected: usize) -> OperationError {
    OperationError::unavailable(format!("Incomplete USB write ({actual}/{expected} bytes)"))
}

fn card_identities(root: &Path, unit: UnitId) -> Result<Vec<AlsaCardIdentity>> {
    let mut cards = Vec::new();
    let profile = unit.profile.profile();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(card) = name
            .to_str()
            .and_then(|s| s.strip_prefix("card"))
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let id_path = entry.path().join("usbid");
        let id = match fs::read_to_string(id_path) {
            Ok(id) => id,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        // Only proven unrelated products may omit bus metadata. A malformed
        // possible target must still prevent authority from being established.
        if AlsaCardIdentity::parse_usb_id(&id)? != (profile.vid, profile.pid) {
            continue;
        }
        let bus = fs::read_to_string(entry.path().join("usbbus"))?;
        cards.push(AlsaCardIdentity::parse(card, &id, &bus)?);
    }
    Ok(cards)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Mute,
    Headphone,
    Gain,
}
impl Field {
    const ALL: [Self; 3] = [Self::Mute, Self::Headphone, Self::Gain];
    fn index(self) -> usize {
        match self {
            Self::Mute => 0,
            Self::Headphone => 1,
            Self::Gain => 2,
        }
    }
    fn enabled(self, profile: ProfileId) -> bool {
        let p = profile.profile();
        match self {
            Self::Mute => p.sync_alsa_mute,
            Self::Headphone => p.sync_alsa_hp,
            Self::Gain => p.sync_alsa_gain,
        }
    }
    fn firmware(self, state: &DeviceState, profile: ProfileId) -> i32 {
        match self {
            Self::Mute => i32::from(state.muted),
            Self::Headphone => (state.hp_volume_db * f64::from(profile.profile().hp_scale)) as i32,
            Self::Gain => i32::from(state.gain_raw),
        }
    }
    fn alsa(self, firmware: i32, profile: ProfileId) -> i32 {
        match self {
            Self::Mute => firmware,
            Self::Headphone => protocol::fw_hp_to_alsa(profile, firmware as i16),
            Self::Gain => protocol::fw_gain_to_alsa(profile, firmware as u16),
        }
    }
    fn setting(self, alsa: i32, profile: ProfileId) -> DeviceSetting {
        match self {
            Self::Mute => DeviceSetting::Mute(alsa != 0),
            Self::Headphone => DeviceSetting::HeadphoneDb(
                f64::from(protocol::alsa_hp_to_fw(profile, alsa))
                    / f64::from(profile.profile().hp_scale),
            ),
            Self::Gain => DeviceSetting::GainRaw(
                (f64::from(alsa) * 0.5 * f64::from(profile.profile().gain_scale)) as u16,
            ),
        }
    }
}
trait Alsa: Send {
    /// Rediscover incomplete roles; true requires reasserting firmware intent.
    fn refresh(&mut self, _now: Instant) -> Result<bool> {
        Ok(false)
    }
    fn expected(&self, field: Field, value: i32) -> Result<i32>;
    fn read(&mut self, field: Field) -> Result<i32>;
    /// Success means the returned control value confirms the clamped write.
    fn write(&mut self, field: Field, value: i32) -> Result<()>;
}
struct CardControls {
    unit: UnitId,
    card: u32,
    controls: AlsaControls,
    runner: CommandRunner,
    root: PathBuf,
    last_discovery: Instant,
}
impl CardControls {
    fn open(unit: UnitId) -> Result<Self> {
        Self::open_at(unit, Path::new("/proc/asound"), CommandRunner::default())
    }
    fn open_at(unit: UnitId, root: &Path, runner: CommandRunner) -> Result<Self> {
        let cards = card_identities(root, unit)?;
        let card = protocol::card_for_unit(unit, &cards).ok_or_else(|| {
            OperationError::unavailable("Exact USB ALSA interface is not ready or is ambiguous")
        })?;
        let mut result = Self {
            unit,
            card,
            controls: AlsaControls::default(),
            runner,
            root: root.to_owned(),
            last_discovery: Instant::now(),
        };
        result.discover()?;
        Ok(result)
    }
    fn control(&self, field: Field) -> Result<AlsaControl> {
        match field {
            Field::Mute => self.controls.mute,
            Field::Headphone => self.controls.hp_volume,
            Field::Gain => self.controls.gain,
        }
        .ok_or_else(|| {
            OperationError::unavailable(format!("ALSA {field:?} control is unavailable"))
        })
    }
    fn complete(&self) -> bool {
        Field::ALL.into_iter().all(|field| {
            if !field.enabled(self.unit.profile) {
                return true;
            }
            let control = match field {
                Field::Mute => self.controls.mute,
                Field::Headphone => self.controls.hp_volume,
                Field::Gain => self.controls.gain,
            };
            control.is_some_and(|control| {
                field == Field::Mute || control.range.is_some_and(|range| range.min <= range.max)
            })
        })
    }
    fn discover(&mut self) -> Result<bool> {
        let contents = self.command(vec!["contents".into()])?;
        // Do not admit output collected across a card replacement.
        self.validate_identity()?;
        let controls = protocol::parse_alsa_controls(&contents);
        let changed = controls != self.controls;
        self.controls = controls;
        Ok(changed)
    }
    fn validate_identity(&self) -> Result<()> {
        if protocol::card_for_unit(self.unit, &card_identities(&self.root, self.unit)?)
            != Some(self.card)
        {
            return Err(OperationError::new(
                ErrorCode::Identity,
                "ALSA card identity changed",
            ));
        }
        Ok(())
    }
    fn command(&self, args: Vec<String>) -> Result<String> {
        // Revalidate before every command; a recycled card number is not authority.
        self.validate_identity()?;
        let mut full = vec!["-c".into(), self.card.to_string()];
        full.extend(args);
        let output = self.runner.run("amixer", &full, COMMAND_TIMEOUT)?;
        String::from_utf8(output.stdout)
            .map_err(|_| OperationError::invalid("Invalid ALSA control output"))
    }
    fn value(field: Field, output: &str) -> Result<i32> {
        match field {
            Field::Mute => protocol::parse_alsa_mute(output).map(i32::from),
            _ => protocol::parse_alsa_volume(output),
        }
        .ok_or_else(|| OperationError::unavailable("ALSA control value is unknown"))
    }
}
impl Alsa for CardControls {
    fn refresh(&mut self, now: Instant) -> Result<bool> {
        if self.complete() || now.duration_since(self.last_discovery) < ALSA_POLL {
            return Ok(false);
        }
        self.last_discovery = now;
        self.discover()
    }
    fn expected(&self, field: Field, value: i32) -> Result<i32> {
        let control = self.control(field)?;
        if field == Field::Mute {
            Ok(value)
        } else {
            control.clamp(value)
        }
    }
    fn read(&mut self, field: Field) -> Result<i32> {
        let control = self.control(field)?;
        Self::value(
            field,
            &self.command(vec!["cget".into(), format!("numid={}", control.numid)])?,
        )
    }
    fn write(&mut self, field: Field, value: i32) -> Result<()> {
        let control = self.control(field)?;
        let value = self.expected(field, value)?;
        let argument = if field == Field::Mute {
            if value != 0 {
                "off".into()
            } else {
                "on".into()
            }
        } else {
            value.to_string()
        };
        let output = self.command(vec![
            "cset".into(),
            format!("numid={}", control.numid),
            argument,
        ])?;
        if Self::value(field, &output)? != value {
            return Err(OperationError::unavailable(
                "ALSA mirror write was not confirmed",
            ));
        }
        Ok(())
    }
}

#[derive(Default)]
struct Mirror {
    last: Option<DeviceState>,
    pending: [Option<i32>; 3],
    last_read: Option<Instant>,
}
impl Mirror {
    fn observe(
        &mut self,
        config: &mut ConfigBuffer,
        alsa: &mut dyn Alsa,
        now: Instant,
    ) -> (bool, Vec<OperationIssue>) {
        let profile = config.profile();
        let state = config.state();
        let read_due = self
            .last_read
            .is_none_or(|last| now.duration_since(last) >= ALSA_POLL);
        if read_due {
            self.last_read = Some(now);
        }
        let mut dirty = false;
        let mut errors = Vec::new();
        let rediscovered = match alsa.refresh(now) {
            Ok(changed) => changed,
            Err(error) => {
                // A failed discovery cannot discharge firmware mirror intent,
                // even when this is the first otherwise successful vendor poll.
                for field in Field::ALL {
                    if field.enabled(profile) {
                        self.pending[field.index()] =
                            Some(field.alsa(field.firmware(&state, profile), profile));
                    }
                }
                errors.push(OperationIssue {
                    target: "ALSA controls".into(),
                    message: error.to_string(),
                });
                return (false, errors);
            }
        };
        for field in Field::ALL {
            if !field.enabled(profile) {
                continue;
            }
            let value = field.firmware(&state, profile);
            let changed = self
                .last
                .as_ref()
                .is_none_or(|last| field.firmware(last, profile) != value);
            if changed || rediscovered {
                self.pending[field.index()] = Some(field.alsa(value, profile));
            }
            let result = if let Some(pending) = self.pending[field.index()] {
                // Never consume a stale read in the poll which resolves a mirror.
                alsa.write(field, pending).map(|()| {
                    self.pending[field.index()] = None;
                })
            } else if read_due && field != Field::Gain {
                alsa.read(field).and_then(|observed| {
                    if observed != alsa.expected(field, field.alsa(value, profile))? {
                        config.apply(field.setting(observed, profile))?;
                        dirty = true;
                    }
                    Ok(())
                })
            } else {
                Ok(())
            };
            if let Err(error) = result {
                errors.push(OperationIssue {
                    target: format!("ALSA {field:?}"),
                    message: error.to_string(),
                });
            }
        }
        (dirty, errors)
    }
    fn committed(&mut self, state: DeviceState) {
        self.last = Some(state);
    }
    fn requested(&mut self, state: &DeviceState, profile: ProfileId, settings: &[DeviceSetting]) {
        for setting in settings {
            let field = match setting {
                DeviceSetting::Mute(_) => Field::Mute,
                DeviceSetting::HeadphoneDb(_) => Field::Headphone,
                DeviceSetting::GainRaw(_) => Field::Gain,
                _ => continue,
            };
            if field.enabled(profile) {
                self.pending[field.index()] =
                    Some(field.alsa(field.firmware(state, profile), profile));
            }
        }
    }
}
trait UnitBackend: Send {
    fn info(&mut self) -> Result<DeviceInfo>;
    fn poll(&mut self) -> Result<(DeviceState, Vec<OperationIssue>)>;
    fn apply(&mut self, settings: &[DeviceSetting]) -> Result<DeviceState>;
    fn unresponsive(&self) -> bool;
}
struct SyncedDevice {
    vendor: VendorDevice,
    alsa: Box<dyn Alsa>,
    mirror: Mirror,
}
impl UnitBackend for SyncedDevice {
    fn info(&mut self) -> Result<DeviceInfo> {
        self.vendor.read_info()
    }
    fn poll(&mut self) -> Result<(DeviceState, Vec<OperationIssue>)> {
        let mut config = self.vendor.read_config()?;
        let (dirty, errors) = self
            .mirror
            .observe(&mut config, self.alsa.as_mut(), Instant::now());
        if dirty {
            self.vendor.write_config_raw(config.as_bytes())?;
        }
        let state = config.state();
        self.mirror.committed(state.clone());
        Ok((state, errors))
    }
    fn apply(&mut self, settings: &[DeviceSetting]) -> Result<DeviceState> {
        // All validation precedes any transfer; one reserved-byte-preserving RMW.
        for setting in settings {
            protocol::validate_setting(self.vendor.unit.profile, *setting)?;
        }
        let mut config = self.vendor.read_config()?;
        for setting in settings {
            config.apply(*setting)?;
        }
        self.vendor.write_config_raw(config.as_bytes())?;
        let state = config.state();
        self.mirror
            .requested(&state, self.vendor.unit.profile, settings);
        self.mirror.committed(state.clone());
        // Next ordinary poll retries pending fields and reports degraded sync.
        Ok(state)
    }
    fn unresponsive(&self) -> bool {
        self.vendor.transport.unresponsive()
    }
}
trait DeviceFactory: Send + Sync {
    fn scan(&self) -> Result<Vec<(ProfileId, u8, u8)>>;
    fn open(&self, unit: UnitId) -> Result<Box<dyn UnitBackend>>;
}
struct NativeFactory;
impl DeviceFactory for NativeFactory {
    fn scan(&self) -> Result<Vec<(ProfileId, u8, u8)>> {
        VendorDevice::scan()
    }
    fn open(&self, unit: UnitId) -> Result<Box<dyn UnitBackend>> {
        let vendor = VendorDevice::open(unit)?;
        let alsa = CardControls::open(unit)?;
        Ok(Box::new(SyncedDevice {
            vendor,
            alsa: Box::new(alsa),
            mirror: Mirror::default(),
        }))
    }
}

struct Job {
    id: u64,
    settings: Vec<DeviceSetting>,
}
#[derive(Default)]
struct QueueState {
    ready: bool,
    retiring: bool,
    unplugged: bool,
    jobs: VecDeque<Job>,
}
#[derive(Default)]
struct Queue {
    state: Mutex<QueueState>,
    wake: Condvar,
}
impl Queue {
    fn lock_state(&self) -> std::sync::MutexGuard<'_, QueueState> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                // Recovery permits only cancellation/draining, never more I/O.
                let mut state = poisoned.into_inner();
                state.ready = false;
                state.retiring = true;
                self.wake.notify_all();
                state
            }
        }
    }
    fn retire(&self) {
        let mut state = self.lock_state();
        state.ready = false;
        state.retiring = true;
        self.wake.notify_all();
    }
}
#[derive(Default)]
struct Registry {
    stopped: bool,
    units: HashMap<UnitId, Arc<Queue>>,
}
fn lock_registry(registry: &Mutex<Registry>) -> std::sync::MutexGuard<'_, Registry> {
    match registry.lock() {
        Ok(state) => state,
        Err(poisoned) => {
            let mut state = poisoned.into_inner();
            state.stopped = true;
            for queue in state.units.values() {
                queue.retire();
            }
            state
        }
    }
}
fn poisoned_owner() -> OperationError {
    OperationError::unavailable("USB owner state was poisoned; work retired")
}
enum ManagerCommand {
    Rescan,
    Stop,
}
pub struct DeviceManager {
    registry: Arc<Mutex<Registry>>,
    sender: mpsc::Sender<ManagerCommand>,
    thread: Option<JoinHandle<Result<()>>>,
}
impl DeviceManager {
    pub fn start() -> Result<(Self, mpsc::Receiver<DeviceEvent>)> {
        Self::start_with(Arc::new(NativeFactory))
    }
    /// Run the real discovery owner with no hardware candidates.
    #[cfg(test)]
    pub(crate) fn start_empty_with_scan(
        scan: impl Fn() + Send + Sync + 'static,
    ) -> Result<(Self, mpsc::Receiver<DeviceEvent>)> {
        struct EmptyFactory<F>(F);
        impl<F: Fn() + Send + Sync> DeviceFactory for EmptyFactory<F> {
            fn scan(&self) -> Result<Vec<(ProfileId, u8, u8)>> {
                (self.0)();
                Ok(Vec::new())
            }
            fn open(&self, _: UnitId) -> Result<Box<dyn UnitBackend>> {
                unreachable!("empty discovery cannot open a device")
            }
        }
        Self::start_with(Arc::new(EmptyFactory(scan)))
    }
    fn start_with(factory: Arc<dyn DeviceFactory>) -> Result<(Self, mpsc::Receiver<DeviceEvent>)> {
        let registry = Arc::new(Mutex::new(Registry::default()));
        let (sender, receiver) = mpsc::channel();
        let (events, output) = mpsc::channel();
        let shared = registry.clone();
        let thread = thread::Builder::new()
            .name("openwave-usb-discovery".into())
            .spawn(move || discovery(factory, shared, receiver, events))?;
        Ok((
            Self {
                registry,
                sender,
                thread: Some(thread),
            },
            output,
        ))
    }
    pub fn submit(&self, unit: UnitId, job: u64, settings: Vec<DeviceSetting>) -> Result<()> {
        if settings.is_empty() {
            return Err(OperationError::invalid("Device job has no settings"));
        }
        let settings = settings
            .into_iter()
            .map(|s| protocol::validate_setting(unit.profile, s))
            .collect::<Result<Vec<_>>>()?;
        let registry = lock_registry(&self.registry);
        if self.registry.is_poisoned() {
            return Err(poisoned_owner());
        }
        if registry.stopped {
            return Err(cancelled());
        }
        let queue = registry
            .units
            .get(&unit)
            .ok_or_else(|| OperationError::unavailable("Captured device incarnation is absent"))?;
        let mut state = queue.lock_state();
        if state.retiring || !state.ready {
            return Err(OperationError::unavailable(
                "Captured device is not accepting work",
            ));
        }
        state.jobs.push_back(Job { id: job, settings });
        queue.wake.notify_one();
        Ok(())
    }
    pub fn rescan(&self) -> Result<()> {
        let stopped = lock_registry(&self.registry).stopped;
        if self.registry.is_poisoned() {
            return Err(poisoned_owner());
        }
        if stopped {
            return Err(cancelled());
        }
        self.sender
            .send(ManagerCommand::Rescan)
            .map_err(|_| cancelled())
    }
    pub fn stop(&mut self) -> Result<()> {
        {
            let mut registry = lock_registry(&self.registry);
            registry.stopped = true;
            for queue in registry.units.values() {
                queue.retire();
            }
        }
        let _ = self.sender.send(ManagerCommand::Stop);
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| OperationError::unavailable("USB discovery worker panicked"))??;
        }
        if self.registry.is_poisoned() {
            return Err(poisoned_owner());
        }
        Ok(())
    }
}
impl Drop for DeviceManager {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
fn cancelled() -> OperationError {
    OperationError::new(
        ErrorCode::Cancelled,
        "Device job cancelled during retirement",
    )
}
fn snapshot(
    unit: UnitId,
    info: &DeviceInfo,
    state: DeviceState,
    errors: Vec<OperationIssue>,
) -> UnitSnapshot {
    // Vendor u32 meter units have no verified PCM normalization. These legacy
    // snapshot fields are unavailable (zero); PCM meters live in the meter bank.
    UnitSnapshot {
        id: unit,
        info: info.clone(),
        state: Observation::Known(state),
        desired_mute: None,
        input_peak: 0.0,
        output_peak: 0.0,
        errors,
    }
}
fn unit_worker(
    factory: Arc<dyn DeviceFactory>,
    unit: UnitId,
    queue: Arc<Queue>,
    events: mpsc::Sender<DeviceEvent>,
) -> bool {
    let mut unresponsive = false;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
        {
            let state = queue.lock_state();
            if queue.state.is_poisoned() {
                return Err(poisoned_owner());
            }
            if state.retiring {
                return Ok(());
            }
        }
        let mut device = factory.open(unit)?;
        let run = (|| -> Result<()> {
            let info = device.info()?;
            let (state, errors) = device.poll()?;
            {
                let mut state_queue = queue.lock_state();
                if queue.state.is_poisoned() {
                    return Err(poisoned_owner());
                }
                if state_queue.retiring {
                    return Ok(());
                }
                state_queue.ready = true;
            }
            let _ = events.send(DeviceEvent::Connected(snapshot(unit, &info, state, errors)));
            let mut next_poll = Instant::now() + POLL;
            let mut failures = 0;
            loop {
                let job = {
                    let mut state = queue.lock_state();
                    while !state.retiring && state.jobs.is_empty() && Instant::now() < next_poll {
                        let timeout = next_poll.saturating_duration_since(Instant::now());
                        state = match queue.wake.wait_timeout(state, timeout) {
                            Ok((state, _)) => state,
                            Err(poisoned) => {
                                let (mut state, _) = poisoned.into_inner();
                                state.ready = false;
                                state.retiring = true;
                                return Err(poisoned_owner());
                            }
                        };
                    }
                    if queue.state.is_poisoned() {
                        return Err(poisoned_owner());
                    }
                    if state.retiring {
                        break;
                    }
                    // A due poll cannot starve behind a stream of writes.
                    if Instant::now() >= next_poll {
                        None
                    } else {
                        state.jobs.pop_front()
                    }
                };
                if let Some(job) = job {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        device.apply(&job.settings)
                    }))
                    .unwrap_or_else(|_| {
                        Err(OperationError::unavailable(
                            "USB transaction panicked; unit retired",
                        ))
                    });
                    let failed = result.as_ref().err().cloned();
                    let _ = events.send(DeviceEvent::Completed {
                        job: job.id,
                        unit,
                        result,
                    });
                    if let Some(error) = failed {
                        return Err(error);
                    }
                } else {
                    match device.poll() {
                        Ok((state, errors)) => {
                            failures = 0;
                            let _ = events
                                .send(DeviceEvent::Observed(snapshot(unit, &info, state, errors)));
                        }
                        Err(error) => {
                            failures += 1;
                            if failures >= 3 {
                                return Err(error);
                            }
                            let _ = events.send(DeviceEvent::Error {
                                unit: Some(unit),
                                error,
                            });
                        }
                    }
                    next_poll = Instant::now() + POLL;
                }
            }
            Ok(())
        })();
        unresponsive = device.unresponsive();
        // The handle is dropped here only after the running operation returned.
        run
    }))
    .unwrap_or_else(|_| {
        Err(OperationError::unavailable(
            "USB owner panicked; unit retired",
        ))
    });
    queue.retire();
    let jobs = {
        let mut state = queue.lock_state();
        std::mem::take(&mut state.jobs)
    };
    for job in jobs {
        let _ = events.send(DeviceEvent::Completed {
            job: job.id,
            unit,
            result: Err(cancelled()),
        });
    }
    if let Err(error) = result {
        let _ = events.send(DeviceEvent::Error {
            unit: Some(unit),
            error,
        });
    }
    let _ = events.send(DeviceEvent::Retired(unit));
    unresponsive
}
fn discovery(
    factory: Arc<dyn DeviceFactory>,
    registry: Arc<Mutex<Registry>>,
    commands: mpsc::Receiver<ManagerCommand>,
    events: mpsc::Sender<DeviceEvent>,
) -> Result<()> {
    let mut workers: HashMap<UnitId, JoinHandle<bool>> = HashMap::new();
    let mut suppressed = HashSet::new();
    let mut next_scan = Instant::now();
    let mut errors = Vec::new();
    loop {
        let finished: Vec<_> = workers
            .iter()
            .filter(|(_, worker)| worker.is_finished())
            .map(|(unit, _)| *unit)
            .collect();
        for unit in finished {
            let worker = workers.remove(&unit).unwrap();
            let unplugged = lock_registry(&registry)
                .units
                .remove(&unit)
                .is_some_and(|q| q.lock_state().unplugged);
            match worker.join() {
                Ok(true) if !unplugged => {
                    suppressed.insert((unit.profile, unit.bus, unit.address));
                }
                Ok(_) => {}
                Err(_) => errors.push("USB unit worker panicked".to_string()),
            }
        }
        if lock_registry(&registry).stopped {
            break;
        }
        if Instant::now() >= next_scan {
            match factory.scan() {
                Ok(mut found) => {
                    found.sort_by_key(|(_, bus, address)| (*bus, *address));
                    found.dedup();
                    let wanted: HashSet<_> = found.iter().copied().collect();
                    suppressed.retain(|key| wanted.contains(key));
                    let mut shared = lock_registry(&registry);
                    for (unit, queue) in &shared.units {
                        if !wanted.contains(&(unit.profile, unit.bus, unit.address)) {
                            queue.lock_state().unplugged = true;
                            queue.retire();
                        }
                    }
                    if !shared.stopped {
                        for (profile, bus, address) in found {
                            if suppressed.contains(&(profile, bus, address))
                                || shared.units.keys().any(|u| {
                                    (u.profile, u.bus, u.address) == (profile, bus, address)
                                })
                            {
                                continue;
                            }
                            let incarnation = match INCARNATION.fetch_update(
                                Ordering::Relaxed,
                                Ordering::Relaxed,
                                |v| v.checked_add(1),
                            ) {
                                Ok(value) => value,
                                Err(_) => {
                                    errors.push("USB incarnation space exhausted".into());
                                    shared.stopped = true;
                                    break;
                                }
                            };
                            let unit = UnitId {
                                profile,
                                bus,
                                address,
                                incarnation,
                            };
                            let queue = Arc::new(Queue::default());
                            let (f, q, e) = (factory.clone(), queue.clone(), events.clone());
                            match thread::Builder::new()
                                .name(format!("openwave-usb-{bus}-{address}"))
                                .spawn(move || unit_worker(f, unit, q, e))
                            {
                                Ok(worker) => {
                                    shared.units.insert(unit, queue);
                                    workers.insert(unit, worker);
                                }
                                Err(error) => {
                                    let _ = events.send(DeviceEvent::Error {
                                        unit: Some(unit),
                                        error: error.into(),
                                    });
                                }
                            }
                        }
                    }
                }
                Err(error) => {
                    let _ = events.send(DeviceEvent::Error { unit: None, error });
                }
            }
            next_scan = Instant::now() + SCAN;
        }
        match commands.recv_timeout(Duration::from_millis(25)) {
            Ok(ManagerCommand::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(ManagerCommand::Rescan) => next_scan = Instant::now(),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
    {
        let mut shared = lock_registry(&registry);
        shared.stopped = true;
        for queue in shared.units.values() {
            queue.retire();
        }
    }
    for (_, worker) in workers {
        if worker.join().is_err() {
            errors.push("USB unit worker panicked".into());
        }
    }
    lock_registry(&registry).units.clear();
    if registry.is_poisoned() {
        errors.push(poisoned_owner().to_string());
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(OperationError::unavailable(errors.join("; ")))
    }
}

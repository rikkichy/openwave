//! Keeps playback off a reappearing Wave XLR output until its capture runs.
//!
//! The Wave XLR (0fd9:007d) runs capture and playback from one firmware
//! clock. When playback starts before capture, capture delivers digital
//! silence until the unit loses power or is reset. When the unit reappears
//! (a wake through a monitor hub, a replug, a USB reset), PipeWire reconnects
//! already-open playback streams such as a browser tab to the new sink at
//! once, seconds before the keepalive opens capture. Suspending the sink
//! doesn't stop a linked stream from starting it, and moving streams after
//! the sink appears still loses the race when a stream connects between two
//! checks; both were tried on hardware.
//!
//! So the guard acts on the disappearance: it records which streams play to
//! the Wave XLR sink, and the moment that sink is removed it moves them to a
//! temporary `openwave-hold` null sink. They are already elsewhere when the
//! unit comes back, and stay there until the keepalive reports capture data
//! for the same unit; then they go back. A sink that appears without a
//! preceding removal is held the same way, and streams landing on a held sink
//! are moved off every 50 ms as a backstop. The hold sink only exists while
//! something is held. Streams that followed the default sink follow it again
//! afterwards.
//!
//! Only the Wave XLR is guarded: it is the only unit this was observed and
//! tested on. The XLR Dock and Wave:3 share the clock design and can be added
//! once confirmed.

use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    io::BufReader,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub const HOLD_SINK: &str = "openwave-hold";
const GUARDED_PREFIX: &str = "Elgato_Systems_Elgato_Wave_XLR_";
const TICK: Duration = Duration::from_millis(50);
const TRACK_EVERY: Duration = Duration::from_secs(1);
const MAX_HOLD: Duration = Duration::from_secs(10);
const MAX_WAIT_RETURN: Duration = Duration::from_secs(30);

/// `usb-<vendor_product_serial>-00` for `alsa_{input,output}.<key>.<profile>`,
/// shared by a unit's source and sink.
pub fn device_key(node_name: &str) -> Option<&str> {
    let rest = node_name
        .strip_prefix("alsa_output.")
        .or_else(|| node_name.strip_prefix("alsa_input."))?;
    rest.rsplit_once('.').map(|(key, _)| key)
}

pub fn is_guarded_sink(node_name: &str) -> bool {
    node_name
        .strip_prefix("alsa_output.usb-")
        .is_some_and(|rest| rest.starts_with(GUARDED_PREFIX))
}

/// The USB serial of a guarded unit from a node name, e.g.
/// `alsa_input.usb-Elgato_Systems_Elgato_Wave_XLR_A01DA401224UGC-00.mono-fallback`
/// gives `A01DA401224UGC`.
pub fn node_serial(node_name: &str) -> Option<&str> {
    let rest = device_key(node_name)?
        .strip_prefix("usb-")?
        .strip_prefix(GUARDED_PREFIX)?;
    Some(rest.rsplit_once('-').map_or(rest, |(serial, _)| serial))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkEvent {
    pub name: String,
    pub removed: bool,
    pub initial: bool,
}

/// One `pw-dump --monitor` batch to sink events; `seen` maps ids to sink names.
pub fn sink_events(
    batch: &Value,
    seen: &mut HashMap<u64, String>,
    initial: bool,
) -> Vec<SinkEvent> {
    let mut events = Vec::new();
    for object in batch.as_array().into_iter().flatten() {
        let Some(id) = object["id"].as_u64() else {
            continue;
        };
        if object["info"].is_null() {
            if let Some(name) = seen.remove(&id) {
                events.push(SinkEvent {
                    name,
                    removed: true,
                    initial: false,
                });
            }
            continue;
        }
        if seen.contains_key(&id) || object["type"] != "PipeWire:Interface:Node" {
            continue;
        }
        let props = &object["info"]["props"];
        if props["media.class"] != "Audio/Sink" {
            continue;
        }
        let Some(name) = props["node.name"].as_str() else {
            continue;
        };
        seen.insert(id, name.to_owned());
        events.push(SinkEvent {
            name: name.to_owned(),
            removed: false,
            initial,
        });
    }
    events
}

/// Sink name to pactl index, from `pactl list short sinks`.
pub fn parse_short_sinks(text: &str) -> HashMap<String, u64> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.split('\t');
            let index = f.next()?.parse().ok()?;
            Some((f.next()?.to_owned(), index))
        })
        .collect()
}

/// A playback stream from `pactl -f json list sink-inputs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkInput {
    pub index: u64,
    pub sink: u64,
    /// PipeWire node id, for clearing the explicit target a move leaves.
    pub object_id: Option<u64>,
}

pub fn parse_sink_inputs(json: &str) -> Vec<SinkInput> {
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(json) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            Some(SinkInput {
                index: item["index"].as_u64()?,
                sink: item["sink"].as_u64()?,
                object_id: item["properties"]["object.id"]
                    .as_str()
                    .and_then(|id| id.parse().ok()),
            })
        })
        .collect()
}

fn run_tool(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program)
        .args(args)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn pactl(args: &[&str]) -> Option<String> {
    run_tool("pactl", args)
}

fn sink_index(sink: &str) -> Option<u64> {
    parse_short_sinks(&pactl(&["list", "short", "sinks"])?)
        .get(sink)
        .copied()
}

fn sink_inputs() -> Vec<SinkInput> {
    pactl(&["-f", "json", "list", "sink-inputs"])
        .map_or_else(Vec::new, |json| parse_sink_inputs(&json))
}

fn default_sink() -> Option<String> {
    pactl(&["get-default-sink"]).map(|name| name.trim().to_owned())
}

fn move_stream(index: u64, sink: &str) -> bool {
    pactl(&["move-sink-input", &index.to_string(), sink]).is_some()
}

/// Undo the explicit target a move leaves, so the stream follows the default
/// sink again.
fn follow_default(object_id: u64) {
    let id = object_id.to_string();
    for key in ["target.object", "target.node"] {
        let _ = run_tool("pw-metadata", &["-d", &id, key]);
    }
}

struct Hold {
    since: Instant,
    returned: Option<Instant>,
    moved: usize,
    /// The held sink was the default sink when the hold began.
    was_default: bool,
}

enum Message {
    Sink(SinkEvent),
    CaptureRunning(String),
}

pub struct PlaybackGuard {
    tx: mpsc::Sender<Message>,
    cancel: Arc<AtomicBool>,
    monitor: Arc<Mutex<Option<Child>>>,
    worker: Option<JoinHandle<()>>,
}

impl PlaybackGuard {
    pub fn start() -> Self {
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let monitor = Arc::new(Mutex::new(None));
        let worker = {
            let (tx, cancel, monitor) = (tx.clone(), cancel.clone(), monitor.clone());
            thread::Builder::new()
                .name("openwave-guard".into())
                .spawn(move || Guard::default().run(rx, tx, cancel, monitor))
                .ok()
        };
        Self {
            tx,
            cancel,
            monitor,
            worker,
        }
    }

    /// The keepalive for `capture_node` has data: its unit's output may start.
    pub fn capture_running(&self, capture_node: &str) {
        let _ = self
            .tx
            .send(Message::CaptureRunning(capture_node.to_owned()));
    }
}

impl Drop for PlaybackGuard {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(child) = self
            .monitor
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_mut()
        {
            let _ = child.kill();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn spawn_monitor(
    tx: mpsc::Sender<Message>,
    cancel: Arc<AtomicBool>,
    slot: Arc<Mutex<Option<Child>>>,
) {
    thread::spawn(move || {
        while !cancel.load(Ordering::Acquire) {
            let child = Command::new("pw-dump")
                .args(["--monitor", "--no-colors"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();
            let Ok(mut child) = child else {
                log::warn!("Playback guard cannot watch PipeWire; retrying");
                thread::sleep(Duration::from_secs(2));
                continue;
            };
            let Some(stdout) = child.stdout.take() else {
                let _ = child.kill();
                continue;
            };
            *slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(child);
            let mut seen = HashMap::new();
            let stream =
                serde_json::Deserializer::from_reader(BufReader::new(stdout)).into_iter::<Value>();
            for (index, batch) in stream.enumerate() {
                let Ok(batch) = batch else { break };
                for event in sink_events(&batch, &mut seen, index == 0) {
                    if tx.send(Message::Sink(event)).is_err() {
                        return;
                    }
                }
            }
            if let Some(mut child) = slot.lock().unwrap_or_else(|p| p.into_inner()).take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            thread::sleep(Duration::from_secs(2));
        }
    });
}

#[derive(Default)]
struct Guard {
    /// Present guarded sinks and their pactl index.
    present: HashMap<String, u64>,
    /// Last known streams on each present, unheld guarded sink.
    on_sink: HashMap<String, Vec<SinkInput>>,
    /// The guarded sinks that were the default sink at the last track.
    default: HashSet<String>,
    held: HashMap<String, Hold>,
    module: Option<String>,
}

impl Guard {
    fn run(
        mut self,
        rx: mpsc::Receiver<Message>,
        tx: mpsc::Sender<Message>,
        cancel: Arc<AtomicBool>,
        monitor: Arc<Mutex<Option<Child>>>,
    ) {
        spawn_monitor(tx, cancel.clone(), monitor);
        let mut last_track = Instant::now();
        while !cancel.load(Ordering::Acquire) {
            match rx.recv_timeout(TICK) {
                Ok(Message::Sink(event)) if is_guarded_sink(&event.name) => self.sink_event(event),
                Ok(Message::CaptureRunning(capture)) => self.capture_running(&capture),
                Ok(Message::Sink(_)) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            self.evict_backstop();
            self.expire();
            if last_track.elapsed() >= TRACK_EVERY {
                last_track = Instant::now();
                self.track();
            }
        }
        let held: Vec<String> = self.held.keys().cloned().collect();
        for sink in held {
            self.release(&sink, "shutdown");
        }
    }

    fn sink_event(&mut self, event: SinkEvent) {
        let name = event.name;
        if event.removed {
            self.present.remove(&name);
            let streams = self.on_sink.remove(&name).unwrap_or_default();
            if self.held.contains_key(&name) || !self.ensure_hold_sink() {
                return;
            }
            let moved = streams
                .iter()
                .filter(|s| move_stream(s.index, HOLD_SINK))
                .count();
            log::info!(
                "Output {name} disappeared; holding {moved} of {} streams until capture runs",
                streams.len()
            );
            let was_default = self.default.contains(&name);
            self.held.insert(
                name,
                Hold {
                    since: Instant::now(),
                    returned: None,
                    moved,
                    was_default,
                },
            );
            return;
        }
        let Some(index) = sink_index(&name) else {
            return;
        };
        self.present.insert(name.clone(), index);
        if let Some(hold) = self.held.get_mut(&name) {
            hold.returned = Some(Instant::now());
            log::info!(
                "Output {name} is back after {} ms; its streams are still held",
                hold.since.elapsed().as_millis()
            );
        } else if !event.initial && self.ensure_hold_sink() {
            let was_default = default_sink().as_deref() == Some(name.as_str());
            let now = Instant::now();
            self.held.insert(
                name.clone(),
                Hold {
                    since: now,
                    returned: Some(now),
                    moved: 0,
                    was_default,
                },
            );
            log::info!("Output {name} appeared; holding playback until capture runs");
        }
    }

    fn capture_running(&mut self, capture: &str) {
        let Some(key) = device_key(capture) else {
            return;
        };
        let ready: Vec<String> = self
            .held
            .keys()
            .filter(|sink| device_key(sink) == Some(key) && self.present.contains_key(*sink))
            .cloned()
            .collect();
        for sink in ready {
            self.release(&sink, "capture running");
        }
    }

    /// Anything that lands on a held, present sink goes to the hold sink.
    fn evict_backstop(&mut self) {
        let targets: HashMap<u64, String> = self
            .held
            .keys()
            .filter_map(|sink| Some((*self.present.get(sink)?, sink.clone())))
            .collect();
        if targets.is_empty() {
            return;
        }
        for stream in sink_inputs() {
            if let Some(sink) = targets.get(&stream.sink)
                && move_stream(stream.index, HOLD_SINK)
                && let Some(hold) = self.held.get_mut(sink)
            {
                hold.moved += 1;
            }
        }
    }

    fn expire(&mut self) {
        let expired: Vec<(String, &'static str)> = self
            .held
            .iter()
            .filter_map(|(sink, hold)| match hold.returned {
                None if hold.since.elapsed() > MAX_WAIT_RETURN => {
                    Some((sink.clone(), "output did not come back"))
                }
                Some(at) if at.elapsed() > MAX_HOLD => Some((sink.clone(), "timeout")),
                _ => None,
            })
            .collect();
        for (sink, reason) in expired {
            self.release(&sink, reason);
        }
    }

    /// Record the streams on each present, unheld guarded sink: one pactl
    /// call a second, and none while no Wave XLR is present.
    fn track(&mut self) {
        let unheld: Vec<(String, u64)> = self
            .present
            .iter()
            .filter(|(sink, _)| !self.held.contains_key(*sink))
            .map(|(sink, index)| (sink.clone(), *index))
            .collect();
        if unheld.is_empty() {
            return;
        }
        let streams = sink_inputs();
        let default = default_sink();
        self.default.clear();
        for (sink, index) in unheld {
            if default.as_deref() == Some(sink.as_str()) {
                self.default.insert(sink.clone());
            }
            self.on_sink.insert(
                sink,
                streams
                    .iter()
                    .filter(|s| s.sink == index)
                    .cloned()
                    .collect(),
            );
        }
    }

    fn ensure_hold_sink(&mut self) -> bool {
        if sink_index(HOLD_SINK).is_some() {
            return true;
        }
        match pactl(&[
            "load-module",
            "module-null-sink",
            &format!("sink_name={HOLD_SINK}"),
            "sink_properties=device.description=OpenWave-holding-playback",
        ]) {
            Some(id) => {
                self.module = Some(id.trim().to_owned());
                true
            }
            None => {
                log::warn!("Playback guard cannot create {HOLD_SINK}; the output is not held");
                false
            }
        }
    }

    /// Move everything on the hold sink back to `sink`, or to the default
    /// sink if `sink` doesn't exist (the unit never came back).
    fn release(&mut self, sink: &str, reason: &str) {
        let Some(hold) = self.held.remove(sink) else {
            return;
        };
        let (mut back, mut failed) = (0, 0);
        if let Some(hold_index) = sink_index(HOLD_SINK) {
            let follow = hold.was_default && default_sink().as_deref() == Some(sink);
            for stream in sink_inputs().into_iter().filter(|s| s.sink == hold_index) {
                if move_stream(stream.index, sink) {
                    back += 1;
                    if follow && let Some(id) = stream.object_id {
                        follow_default(id);
                    }
                } else if move_stream(stream.index, "@DEFAULT_SINK@") {
                    back += 1;
                } else {
                    failed += 1;
                }
            }
        }
        log::info!(
            "Released output {sink} ({reason}) after {} ms; moved {} streams away, {back} back, {failed} failed",
            hold.since.elapsed().as_millis(),
            hold.moved
        );
        if self.held.is_empty()
            && let Some(module) = self.module.take()
        {
            let _ = pactl(&["unload-module", &module]);
        }
    }
}

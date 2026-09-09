use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use indexmap::IndexMap;
use serde_json::Value;

use crate::model::{NodeIdentity, Observation, OperationError, Result};

pub const CHECK_INTERVAL: Duration = Duration::from_secs(10);
pub const GLITCH_XRUNS_PER_CHECK: u64 = 50;
pub const GLITCH_CONFIRM_CHECKS: u32 = 2;
pub const STALL_SECONDS: Duration = Duration::from_secs(8);
pub const COOLDOWN: Duration = Duration::from_secs(60);
pub const MAX_ATTEMPTS: u32 = 2;
pub const CLEAN_REFILL: Duration = Duration::from_secs(300);
pub const STALL_CLEAN_REFILL_CHECKS: u32 = 6;

#[derive(Debug, Clone)]
struct GlitchState {
    identity: NodeIdentity,
    previous: u64,
    streak: u32,
    delta: Option<u64>,
}
#[derive(Debug)]
pub struct GlitchWatch {
    pub threshold: u64,
    pub confirm: u32,
    states: HashMap<String, GlitchState>,
}
impl Default for GlitchWatch {
    fn default() -> Self {
        Self::new(GLITCH_XRUNS_PER_CHECK, GLITCH_CONFIRM_CHECKS)
    }
}
impl GlitchWatch {
    pub fn new(threshold: u64, confirm: u32) -> Self {
        Self {
            threshold,
            confirm: confirm.max(1),
            states: HashMap::new(),
        }
    }
    pub fn pause(&mut self, name: &str) {
        self.states.remove(name);
    }
    pub fn observe(&mut self, name: &str, identity: &NodeIdentity, xruns: u64) -> bool {
        if let Some(state) = self.states.get_mut(name) {
            if state.identity == *identity && xruns >= state.previous {
                let delta = xruns - state.previous;
                state.previous = xruns;
                state.delta = Some(delta);
                state.streak = if delta >= self.threshold {
                    state.streak.saturating_add(1)
                } else {
                    0
                };
                return delta >= self.threshold;
            }
        }
        self.states.insert(
            name.into(),
            GlitchState {
                identity: identity.clone(),
                previous: xruns,
                streak: 0,
                delta: None,
            },
        );
        false
    }
    pub fn glitching(&self, name: &str) -> bool {
        self.states
            .get(name)
            .is_some_and(|s| s.streak >= self.confirm)
    }
    pub fn just_confirmed(&self, name: &str) -> bool {
        self.states
            .get(name)
            .is_some_and(|s| s.streak == self.confirm)
    }
    pub fn last_delta(&self, name: &str) -> Option<u64> {
        self.states.get(name).and_then(|s| s.delta)
    }
}

#[derive(Debug, Default)]
struct CaptureIncident {
    attempts: u32,
    last_attempt: Option<Duration>,
    clean: Option<(NodeIdentity, Duration)>,
}
/// The stable node name owns a single shared no-data/xrun incident budget.
/// `pause` is used for absence, mute, unknown data and collector failures: none
/// of those observations permits a refund or a continuing healthy interval.
#[derive(Debug)]
pub struct StallWatch {
    pub stall_seconds: Duration,
    pub cooldown: Duration,
    pub max_attempts: u32,
    pub clean_refill: Duration,
    incidents: HashMap<String, CaptureIncident>,
}
impl Default for StallWatch {
    fn default() -> Self {
        Self::new(STALL_SECONDS, COOLDOWN, MAX_ATTEMPTS, CLEAN_REFILL)
    }
}
impl StallWatch {
    pub fn new(
        stall_seconds: Duration,
        cooldown: Duration,
        max_attempts: u32,
        clean_refill: Duration,
    ) -> Self {
        Self {
            stall_seconds,
            cooldown,
            max_attempts,
            clean_refill,
            incidents: HashMap::new(),
        }
    }
    pub fn pause(&mut self, name: &str) {
        if let Some(state) = self.incidents.get_mut(name) {
            state.clean = None;
        }
    }
    pub fn spent(&self, name: &str) -> u32 {
        self.incidents.get(name).map_or(0, |s| s.attempts)
    }
    pub fn can_recover(&self, name: &str, now: Duration) -> bool {
        self.spent(name) < self.max_attempts
            && self
                .incidents
                .get(name)
                .and_then(|s| s.last_attempt)
                .is_none_or(|last| {
                    now.checked_sub(last)
                        .is_some_and(|elapsed| elapsed >= self.cooldown)
                })
    }
    pub fn should_recover(
        &mut self,
        name: &str,
        present: bool,
        byte_age: Option<Duration>,
        now: Duration,
    ) -> bool {
        if !present || byte_age.is_none() {
            self.pause(name);
            return false;
        }
        if byte_age.is_some_and(|age| age >= self.stall_seconds) {
            self.pause(name);
            return self.can_recover(name, now);
        }
        false
    }
    /// Spend before attempting a remedy, even when the command later fails.
    pub fn record_attempt(&mut self, name: &str, now: Duration) {
        let state = self.incidents.entry(name.into()).or_default();
        state.attempts = state.attempts.saturating_add(1);
        state.last_attempt = Some(now);
        state.clean = None;
    }
    /// Call only for known flowing bytes and a healthy, non-baseline xrun window.
    pub fn record_recovered(&mut self, name: &str, identity: &NodeIdentity, now: Duration) {
        let state = self.incidents.entry(name.into()).or_default();
        let since = match &state.clean {
            Some((generation, since)) if generation == identity && now >= *since => *since,
            _ => {
                state.clean = Some((identity.clone(), now));
                now
            }
        };
        if now - since >= self.clean_refill {
            state.attempts = 0;
            state.last_attempt = None;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackState {
    Running,
    Xrun,
    Other,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaybackStatus {
    pub hw_ptr: u64,
    pub state: PlaybackState,
}
#[derive(Debug, Clone)]
pub struct SinkSample {
    pub identity: NodeIdentity,
    pub running: bool,
    pub muted: Observation<bool>,
    pub playback: Observation<PlaybackStatus>,
}
#[derive(Debug, Default)]
struct SinkIncident {
    baseline: Option<(NodeIdentity, u64)>,
    stalled: bool,
    was_stalled: bool,
    clean: u32,
    attempts: u32,
    last_attempt: Option<Duration>,
}
#[derive(Debug)]
pub struct SinkStallWatch {
    pub cooldown: Duration,
    pub max_attempts: u32,
    pub clean_refill: u32,
    incidents: HashMap<String, SinkIncident>,
}
impl Default for SinkStallWatch {
    fn default() -> Self {
        Self::new(COOLDOWN, MAX_ATTEMPTS, STALL_CLEAN_REFILL_CHECKS)
    }
}
impl SinkStallWatch {
    pub fn new(cooldown: Duration, max_attempts: u32, clean_refill: u32) -> Self {
        Self {
            cooldown,
            max_attempts,
            clean_refill: clean_refill.max(1),
            incidents: HashMap::new(),
        }
    }
    pub fn pause(&mut self, name: &str) {
        if let Some(state) = self.incidents.get_mut(name) {
            state.baseline = None;
            state.stalled = false;
            state.was_stalled = false;
            state.clean = 0;
        }
    }
    pub fn observe(&mut self, name: &str, sample: &Observation<SinkSample>) -> bool {
        let sample = match sample {
            Observation::Known(sample)
                if sample.running && sample.muted.known() == Some(&false) =>
            {
                sample
            }
            _ => {
                self.pause(name);
                return false;
            }
        };
        let playback = match sample.playback.known() {
            Some(p) if p.state != PlaybackState::Other => p,
            _ => {
                self.pause(name);
                return false;
            }
        };
        let state = self.incidents.entry(name.into()).or_default();
        state.was_stalled = state.stalled;
        let previous = state
            .baseline
            .replace((sample.identity.clone(), playback.hw_ptr));
        let previous =
            previous.filter(|(id, ptr)| *id == sample.identity && *ptr <= playback.hw_ptr);
        if previous.is_none() {
            state.clean = 0;
            state.was_stalled = false;
        }
        state.stalled = playback.state == PlaybackState::Xrun
            || previous
                .as_ref()
                .is_some_and(|(_, ptr)| *ptr == playback.hw_ptr);
        if state.stalled {
            state.clean = 0;
        } else if previous.is_some() {
            state.clean = state.clean.saturating_add(1);
            if state.clean >= self.clean_refill {
                state.attempts = 0;
            }
        }
        state.stalled
    }
    pub fn just_stalled(&self, name: &str) -> bool {
        self.incidents
            .get(name)
            .is_some_and(|s| s.stalled && !s.was_stalled)
    }
    pub fn spent(&self, name: &str) -> u32 {
        self.incidents.get(name).map_or(0, |s| s.attempts)
    }
    pub fn should_recover(&self, name: &str, now: Duration) -> bool {
        self.incidents.get(name).is_some_and(|s| {
            s.stalled
                && s.attempts < self.max_attempts
                && s.last_attempt.is_none_or(|last| {
                    now.checked_sub(last)
                        .is_some_and(|elapsed| elapsed >= self.cooldown)
                })
        })
    }
    pub fn record_attempt(&mut self, name: &str, now: Duration) {
        let state = self.incidents.entry(name.into()).or_default();
        state.attempts = state.attempts.saturating_add(1);
        state.last_attempt = Some(now);
        state.baseline = None;
        state.clean = 0;
    }
}

#[derive(Debug, Clone)]
pub struct CaptureSample {
    pub identity: NodeIdentity,
    pub running: bool,
    pub muted: Observation<bool>,
    pub xruns: Observation<u64>,
    /// Age of received bytes, including all-zero PCM, from this exact generation.
    pub byte_age: Observation<Duration>,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaptureDecision {
    pub no_data: bool,
    pub just_no_data: bool,
    pub glitching: bool,
    pub just_glitching: bool,
    pub healthy: bool,
    /// Budget eligibility only; runtime still needs opt-in and exact identity.
    pub recovery_due: bool,
}
#[derive(Debug, Default)]
pub struct CaptureWatch {
    pub glitch: GlitchWatch,
    pub budget: StallWatch,
    no_data: HashSet<String>,
}
impl CaptureWatch {
    pub fn pause(&mut self, name: &str) {
        self.glitch.pause(name);
        self.budget.pause(name);
        self.no_data.remove(name);
    }
    pub fn observe(
        &mut self,
        name: &str,
        sample: &Observation<CaptureSample>,
        now: Duration,
    ) -> CaptureDecision {
        let sample = match sample {
            Observation::Known(sample)
                if sample.running && sample.muted.known() == Some(&false) =>
            {
                sample
            }
            _ => {
                self.pause(name);
                return CaptureDecision::default();
            }
        };
        match sample.xruns.known() {
            Some(count) => {
                self.glitch.observe(name, &sample.identity, *count);
            }
            None => self.glitch.pause(name),
        }
        let no_data = sample
            .byte_age
            .known()
            .is_some_and(|age| *age >= self.budget.stall_seconds);
        let just_no_data = if no_data {
            self.no_data.insert(name.into())
        } else {
            self.no_data.remove(name);
            false
        };
        let healthy = self
            .glitch
            .last_delta(name)
            .is_some_and(|delta| delta < self.glitch.threshold)
            && sample
                .byte_age
                .known()
                .is_some_and(|age| *age < self.budget.stall_seconds);
        if healthy {
            self.budget.record_recovered(name, &sample.identity, now);
        } else {
            self.budget.pause(name);
        }
        let glitching = self.glitch.glitching(name);
        CaptureDecision {
            no_data,
            just_no_data,
            glitching,
            just_glitching: self.glitch.just_confirmed(name),
            healthy,
            recovery_due: (no_data || glitching) && self.budget.can_recover(name, now),
        }
    }
    pub fn record_attempt(&mut self, name: &str, now: Duration) {
        self.budget.record_attempt(name, now);
        self.glitch.pause(name);
    }
}

#[derive(Debug, Default)]
pub struct HealthSamples {
    pub captures: IndexMap<String, Observation<CaptureSample>>,
    pub sinks: IndexMap<String, Observation<SinkSample>>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SinkDecision {
    pub stalled: bool,
    pub just_stalled: bool,
    pub recovery_due: bool,
}
#[derive(Debug, Default)]
pub struct HealthReport {
    pub captures: IndexMap<String, CaptureDecision>,
    pub sinks: IndexMap<String, SinkDecision>,
    pub error: Option<OperationError>,
}
#[derive(Debug, Default)]
pub struct HealthWatch {
    pub capture: CaptureWatch,
    pub sink: SinkStallWatch,
    captures: HashSet<String>,
    sinks: HashSet<String>,
}
impl HealthWatch {
    /// Runtime passes collector failure as Unknown, including unexpected parse
    /// errors. An unknown or omitted node ends clean intervals, never budgets.
    pub fn observe(&mut self, samples: &Observation<HealthSamples>, now: Duration) -> HealthReport {
        let samples = match samples {
            Observation::Known(samples) => samples,
            Observation::Unknown(error) => {
                for name in &self.captures {
                    self.capture.pause(name);
                }
                for name in &self.sinks {
                    self.sink.pause(name);
                }
                return HealthReport {
                    error: Some(error.clone()),
                    ..HealthReport::default()
                };
            }
        };
        for name in &self.captures {
            if !samples.captures.contains_key(name) {
                self.capture.pause(name);
            }
        }
        for name in &self.sinks {
            if !samples.sinks.contains_key(name) {
                self.sink.pause(name);
            }
        }
        self.captures = samples.captures.keys().cloned().collect();
        self.sinks = samples.sinks.keys().cloned().collect();
        let mut report = HealthReport::default();
        for (name, sample) in &samples.captures {
            report
                .captures
                .insert(name.clone(), self.capture.observe(name, sample, now));
        }
        for (name, sample) in &samples.sinks {
            let stalled = self.sink.observe(name, sample);
            report.sinks.insert(
                name.clone(),
                SinkDecision {
                    stalled,
                    just_stalled: self.sink.just_stalled(name),
                    recovery_due: self.sink.should_recover(name, now),
                },
            );
        }
        report
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XrunCount {
    pub node_id: u32,
    pub node_name: String,
    pub count: u64,
}
/// Preserve duplicate names by node ID; later pw-top iterations win.
pub fn parse_pw_top(text: &str) -> IndexMap<u32, XrunCount> {
    let mut counts = IndexMap::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let _state = fields.next();
        let Some(node_id) = fields.next().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Some(count) = fields.nth(6).and_then(|s| s.parse::<u64>().ok()) else {
            continue;
        };
        let Some(name) = fields.last() else {
            continue;
        };
        counts.insert(
            node_id,
            XrunCount {
                node_id,
                node_name: name.into(),
                count,
            },
        );
    }
    counts
}

pub fn parse_playback_status(text: &str) -> Result<PlaybackStatus> {
    let mut pointer = None;
    let mut state = None;
    for line in text.lines() {
        if let Some((key, value)) = line.split_once(':') {
            match key.trim() {
                "hw_ptr" => pointer = value.trim().parse::<u64>().ok(),
                "state" => {
                    state = Some(match value.trim() {
                        "RUNNING" => PlaybackState::Running,
                        "XRUN" => PlaybackState::Xrun,
                        _ => PlaybackState::Other,
                    })
                }
                _ => (),
            }
        }
    }
    Ok(PlaybackStatus {
        hw_ptr: pointer
            .ok_or_else(|| OperationError::unavailable("Missing ALSA playback pointer"))?,
        state: state.ok_or_else(|| OperationError::unavailable("Missing ALSA playback state"))?,
    })
}

pub fn parse_mutes(value: &Value) -> Result<IndexMap<String, bool>> {
    let entries = value
        .as_array()
        .ok_or_else(|| OperationError::unavailable("Invalid Pulse mute listing"))?;
    let mut mutes = IndexMap::new();
    for entry in entries {
        let name = entry
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| OperationError::unavailable("Missing Pulse node name"))?;
        let mute = entry
            .get("mute")
            .and_then(Value::as_bool)
            .ok_or_else(|| OperationError::unavailable("Unknown Pulse mute"))?;
        if name.is_empty() || mutes.insert(name.into(), mute).is_some() {
            return Err(OperationError::unavailable("Ambiguous Pulse node name"));
        }
    }
    Ok(mutes)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlsaPlayback {
    pub card: u32,
    pub device: u32,
    pub subdevice: u32,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchedNode {
    pub identity: NodeIdentity,
    pub node_id: u32,
    pub running: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchedSink {
    pub node: WatchedNode,
    pub playback: AlsaPlayback,
}
#[derive(Debug, Default)]
pub struct HealthGraph {
    pub captures: IndexMap<String, WatchedNode>,
    pub sinks: IndexMap<String, WatchedSink>,
}

fn uint(value: &Value) -> Option<u32> {
    value
        .as_u64()
        .and_then(|n| n.try_into().ok())
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
}
fn graph_error() -> OperationError {
    OperationError::unavailable("Malformed or identity-incomplete health graph")
}

/// Parse a complete graph. Its Core cookie is mandatory; a malformed node/link
/// invalidates the observation rather than manufacturing an empty healthy graph.
/// Output targets come only from actual links, never target.object hints.
pub fn parse_health_graph(value: &Value) -> Result<HealthGraph> {
    let objects = value.as_array().ok_or_else(graph_error)?;
    let mut cookie = None;
    for object in objects {
        let object = object.as_object().ok_or_else(graph_error)?;
        if object.get("type").and_then(Value::as_str) == Some("PipeWire:Interface:Core") {
            let observed = object
                .get("info")
                .and_then(|v| v.get("cookie"))
                .and_then(uint)
                .ok_or_else(graph_error)?;
            if cookie.replace(observed).is_some() {
                return Err(graph_error());
            }
        }
    }
    let cookie = cookie.ok_or_else(graph_error)?;
    let mut nodes = IndexMap::new();
    let mut names = HashSet::new();
    let mut identities = HashSet::new();
    for object in objects {
        if object.get("type").and_then(Value::as_str) != Some("PipeWire:Interface:Node") {
            continue;
        }
        let id = object.get("id").and_then(uint).ok_or_else(graph_error)?;
        let info = object
            .get("info")
            .and_then(Value::as_object)
            .ok_or_else(graph_error)?;
        let props = info
            .get("props")
            .and_then(Value::as_object)
            .ok_or_else(graph_error)?;
        let name = props
            .get("node.name")
            .and_then(Value::as_str)
            .ok_or_else(graph_error)?;
        let class = props
            .get("media.class")
            .and_then(Value::as_str)
            .unwrap_or("");
        let state = info
            .get("state")
            .and_then(Value::as_str)
            .ok_or_else(graph_error)?;
        let serial = match props.get("object.serial") {
            Some(Value::String(serial)) if !serial.is_empty() => serial.clone(),
            Some(Value::Number(serial)) if serial.as_u64().is_some() => serial.to_string(),
            None => id.to_string(),
            _ => return Err(graph_error()),
        };
        let node = WatchedNode {
            identity: NodeIdentity {
                server_cookie: cookie,
                object_serial: serial,
            },
            node_id: id,
            running: state == "running",
        };
        if !identities.insert(node.identity.clone())
            || nodes.insert(id, (name, class, node, props)).is_some()
        {
            return Err(graph_error());
        }
        // Duplicate stream names are valid; duplicate hardware names cannot be
        // used as authority for stable-name recovery budgets.
        if (class == "Audio/Sink" || class == "Audio/Source") && !names.insert(name) {
            return Err(graph_error());
        }
    }
    let mut graph = HealthGraph::default();
    for (name, class, node, _) in nodes.values() {
        if *class == "Audio/Source"
            && (name.starts_with("alsa_input.usb-Elgato_Systems_Elgato_Wave_")
                || name.starts_with("alsa_input.usb-Elgato_Systems_Elgato_XLR_Dock_"))
        {
            graph.captures.insert((*name).into(), node.clone());
        }
    }
    for object in objects {
        if object.get("type").and_then(Value::as_str) != Some("PipeWire:Interface:Link") {
            continue;
        }
        let info = object
            .get("info")
            .and_then(Value::as_object)
            .ok_or_else(graph_error)?;
        let source = info
            .get("output-node-id")
            .and_then(uint)
            .ok_or_else(graph_error)?;
        let target = info
            .get("input-node-id")
            .and_then(uint)
            .ok_or_else(graph_error)?;
        let (source_name, _, _, _) = nodes.get(&source).ok_or_else(graph_error)?;
        let (name, class, node, props) = nodes.get(&target).ok_or_else(graph_error)?;
        if !source_name.starts_with("openwave_loop_output_")
            || source_name.ends_with("_cap")
            || !name.starts_with("alsa_output.")
            || *class != "Audio/Sink"
        {
            continue;
        }
        let number = |primary: &str, fallback: &str| {
            props
                .get(primary)
                .or_else(|| props.get(fallback))
                .and_then(uint)
        };
        let playback = AlsaPlayback {
            card: number("api.alsa.pcm.card", "alsa.card").ok_or_else(graph_error)?,
            device: number("api.alsa.pcm.device", "alsa.device").ok_or_else(graph_error)?,
            subdevice: match props
                .get("api.alsa.pcm.subdevice")
                .or_else(|| props.get("alsa.subdevice"))
            {
                None => 0,
                Some(value) => uint(value).ok_or_else(graph_error)?,
            },
        };
        graph.sinks.insert(
            (*name).into(),
            WatchedSink {
                node: node.clone(),
                playback,
            },
        );
    }
    Ok(graph)
}

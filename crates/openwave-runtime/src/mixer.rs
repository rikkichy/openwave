//! Single-owner PipeWire reconciliation. Only stop may retry cleanup on its caller.
use crate::{
    meter::{CaptureReadiness, MeterTarget},
    paths::RuntimePaths,
    process::{CommandRunner, OwnedChild},
};
use indexmap::IndexMap;
use openwave_core::{
    effects,
    model::*,
    routing::{self, NormalizedMatcher, NormalizedStream, OutputDecision},
};
use serde_json::{Map, Value, json};
use std::{
    collections::{HashMap, HashSet},
    io::Write,
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
    process::Stdio,
    sync::{Arc, Condvar, Mutex, atomic::AtomicBool, mpsc},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_secs(1);
const FX_RETRY: Duration = Duration::from_secs(5);
const CLEANUP_ATTEMPTS: usize = 3;
const CLEANUP_SETTLE: Duration = Duration::from_millis(100);
fn unavailable(message: impl Into<String>) -> OperationError {
    OperationError::new(ErrorCode::Unavailable, message)
}
fn identity_error(message: impl Into<String>) -> OperationError {
    OperationError::new(ErrorCode::Identity, message)
}
fn issue(target: impl Into<String>, error: impl std::fmt::Display) -> OperationIssue {
    OperationIssue {
        target: target.into(),
        message: error.to_string(),
    }
}
fn text(value: Option<&Value>) -> Option<String> {
    value.and_then(|v| match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    })
}
fn number(value: Option<&Value>) -> Option<u32> {
    value.and_then(|v| {
        v.as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .or_else(|| v.as_str()?.parse().ok())
    })
}
fn required_number(value: Option<&Value>, label: &str) -> Result<u32> {
    number(value).ok_or_else(|| unavailable(format!("Malformed graph: missing {label}")))
}
fn required_text(value: Option<&Value>, label: &str) -> Result<String> {
    text(value)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| unavailable(format!("Malformed graph: missing {label}")))
}
fn wave_name(name: &str) -> bool {
    name.contains("Elgato_Wave_") || name.contains("Elgato_XLR_Dock")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureBinding {
    pub epoch: u64,
    pub owners: Vec<SourceId>,
}
fn same_binding(left: &CaptureBinding, right: &CaptureBinding) -> bool {
    left.epoch == right.epoch
        && left.owners.len() == right.owners.len()
        && left.owners.iter().all(|owner| right.owners.contains(owner))
}
#[derive(Clone, Debug)]
pub struct MixerObservation {
    pub observation: Observation<()>,
    pub captures: Vec<CaptureSnapshot>,
    pub streams: Vec<StreamSnapshot>,
    pub outputs: Vec<OutputSnapshot>,
    pub default_sink: Option<String>,
    pub meter_targets: Vec<MeterTarget>,
    pub errors: Vec<OperationIssue>,
    pub mix_identities: IndexMap<MixId, NodeIdentity>,
    /// Exact device captures whose muted rows have no unsilenced owned routes.
    /// Valid only for this known observation and its desired-state revision.
    pub silent_sources: HashMap<SourceId, NodeIdentity>,
    pub revision: u64,
}
impl Default for MixerObservation {
    fn default() -> Self {
        Self {
            observation: Observation::Unknown(unavailable("Graph has not been observed")),
            captures: Vec::new(),
            streams: Vec::new(),
            outputs: Vec::new(),
            default_sink: None,
            meter_targets: Vec::new(),
            errors: Vec::new(),
            mix_identities: IndexMap::new(),
            silent_sources: HashMap::new(),
            revision: 0,
        }
    }
}
#[derive(Clone, Debug)]
pub enum MixerEvent {
    Observed(MixerObservation),
    MasterObserved {
        mix: MixId,
        revision: u64,
        identity: NodeIdentity,
        level: f64,
        muted: bool,
    },
}

/// A node collection is deliberately not keyed by node.name: stream names need not be unique.
#[derive(Clone, Debug)]
pub struct GraphNode {
    pub id: u32,
    pub identity: NodeIdentity,
    pub name: String,
    pub properties: Map<String, Value>,
}
#[derive(Clone, Debug)]
pub struct GraphPort {
    pub id: u32,
    pub node: u32,
    pub output: bool,
    pub channel: String,
}
#[derive(Clone, Debug)]
pub struct GraphSink {
    pub node: u32,
    pub pulse_index: u32,
    pub level: f64,
    pub muted: bool,
}
#[derive(Clone, Debug)]
pub struct GraphSnapshot {
    pub cookie: u32,
    pub nodes: Vec<GraphNode>,
    pub ports: Vec<GraphPort>,
    pub links: HashSet<(u32, u32)>,
    pub sinks: HashMap<String, GraphSink>,
    pub captures: Vec<CaptureSnapshot>,
    pub streams: Vec<StreamSnapshot>,
    pub outputs: Vec<OutputSnapshot>,
    pub default_sink: Option<String>,
    names: HashMap<String, Vec<usize>>,
    identities: HashMap<NodeIdentity, usize>,
}
impl GraphSnapshot {
    pub fn node(&self, name: &str) -> Option<&GraphNode> {
        let entries = self.names.get(name)?;
        if entries.len() == 1 {
            self.nodes.get(entries[0])
        } else {
            None
        }
    }
    pub fn identity(&self, id: &NodeIdentity) -> Option<&GraphNode> {
        self.identities.get(id).and_then(|i| self.nodes.get(*i))
    }
    fn owned(&self, name: &str, owner: &str) -> Option<&GraphNode> {
        self.node(name).filter(|node| {
            node.properties
                .get("openwave.owner")
                .and_then(Value::as_str)
                == Some(owner)
        })
    }
    pub fn core_cookie(objects: &Value) -> Result<u32> {
        let objects = objects
            .as_array()
            .ok_or_else(|| unavailable("pw-dump did not return an array"))?;
        let mut cores = objects
            .iter()
            .filter(|o| o.get("type").and_then(Value::as_str) == Some("PipeWire:Interface:Core"));
        let cookie = required_number(
            cores
                .next()
                .and_then(|o| o.get("info"))
                .and_then(|i| i.get("cookie")),
            "Core cookie",
        )?;
        if cores.next().is_some() {
            return Err(identity_error("Multiple PipeWire Core identities"));
        }
        Ok(cookie)
    }
    /// Parse one coherent server observation. Missing/malformed state is not an empty graph.
    pub fn parse(
        objects: &Value,
        sinks: &Value,
        sources: &Value,
        inputs: &Value,
        modules: &Value,
        default_sink: Option<String>,
    ) -> Result<Self> {
        let cookie = Self::core_cookie(objects)?;
        fn array(value: &Value) -> Result<&[Value]> {
            value
                .as_array()
                .map(Vec::as_slice)
                .ok_or_else(|| unavailable("Expected Pulse graph array"))
        }
        let pulse_sinks = array(sinks)?;
        let pulse_sources = array(sources)?;
        let pulse_inputs = array(inputs)?;
        let _modules = array(modules)?;
        let mut graph = Self {
            cookie,
            nodes: Vec::new(),
            ports: Vec::new(),
            links: HashSet::new(),
            sinks: HashMap::new(),
            captures: Vec::new(),
            streams: Vec::new(),
            outputs: Vec::new(),
            default_sink,
            names: HashMap::new(),
            identities: HashMap::new(),
        };
        let mut devices = HashMap::new();
        let mut object_ids = HashSet::new();
        for object in objects.as_array().expect("validated array") {
            let id = required_number(object.get("id"), "object id")?;
            if !object_ids.insert(id) {
                return Err(identity_error("Duplicate PipeWire object id"));
            }
            let typ = required_text(object.get("type"), "object type")?;
            if !matches!(
                typ.rsplit(':').next(),
                Some("Node" | "Port" | "Link" | "Device")
            ) {
                continue;
            }
            let info = object
                .get("info")
                .and_then(Value::as_object)
                .ok_or_else(|| unavailable("Missing graph object info"))?;
            if typ.ends_with(":Link") {
                graph.links.insert((
                    required_number(info.get("output-port-id"), "output port")?,
                    required_number(info.get("input-port-id"), "input port")?,
                ));
                continue;
            }
            let props = info
                .get("props")
                .and_then(Value::as_object)
                .ok_or_else(|| unavailable("Missing graph properties"))?;
            if typ.ends_with(":Device") {
                devices.insert(id, props.clone());
            } else if typ.ends_with(":Node") {
                let name = required_text(props.get("node.name"), "node.name")?;
                let identity = NodeIdentity {
                    server_cookie: cookie,
                    object_serial: text(props.get("object.serial"))
                        .unwrap_or_else(|| id.to_string()),
                };
                if graph
                    .identities
                    .insert(identity.clone(), graph.nodes.len())
                    .is_some()
                {
                    return Err(identity_error("Duplicate node runtime identity"));
                }
                graph
                    .names
                    .entry(name.clone())
                    .or_default()
                    .push(graph.nodes.len());
                graph.nodes.push(GraphNode {
                    id,
                    identity,
                    name,
                    properties: props.clone(),
                });
            } else {
                let direction = required_text(props.get("port.direction"), "port.direction")?;
                if direction != "in" && direction != "out" {
                    return Err(unavailable("Invalid port direction"));
                }
                graph.ports.push(GraphPort {
                    id,
                    node: required_number(props.get("node.id"), "port node")?,
                    output: direction == "out",
                    channel: text(props.get("audio.channel")).unwrap_or_default(),
                });
            }
        }
        let node_ids: HashSet<_> = graph.nodes.iter().map(|n| n.id).collect();
        if graph.ports.iter().any(|p| !node_ids.contains(&p.node)) {
            return Err(unavailable("Partial graph: port has no node"));
        }
        let port_ids: HashSet<_> = graph.ports.iter().map(|p| p.id).collect();
        if graph
            .links
            .iter()
            .any(|(a, b)| !port_ids.contains(a) || !port_ids.contains(b))
        {
            return Err(unavailable("Partial graph: link has no port"));
        }
        let mut sink_indices = HashMap::new();
        for sink in pulse_sinks {
            let name = required_text(sink.get("name"), "sink name")?;
            let node = graph
                .node(&name)
                .ok_or_else(|| unavailable(format!("Partial or ambiguous sink {name}")))?
                .clone();
            check_pulse_identity(sink, &node)?;
            let index = required_number(sink.get("index"), "sink index")?;
            let volumes = sink
                .get("volume")
                .and_then(Value::as_object)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| unavailable("Missing sink volume"))?;
            let mut level = 0.0_f64;
            // Preserve the normalized Pulse/wpctl volume coordinate used by saved state.
            for entry in volumes.values() {
                level = level.max(
                    f64::from(required_number(entry.get("value"), "channel volume")?) / 65536.0,
                );
            }
            if !level.is_finite() {
                return Err(unavailable("Invalid sink volume"));
            }
            let muted = sink
                .get("mute")
                .and_then(Value::as_bool)
                .ok_or_else(|| unavailable("Missing sink mute"))?;
            if sink_indices.insert(index, name.clone()).is_some() || graph.sinks.contains_key(&name)
            {
                return Err(identity_error("Duplicate Pulse sink identity"));
            }
            graph.sinks.insert(
                name.clone(),
                GraphSink {
                    node: node.id,
                    pulse_index: index,
                    level: level.clamp(0.0, 1.0),
                    muted,
                },
            );
            if !name.starts_with("openwave_") {
                graph.outputs.push(OutputSnapshot {
                    identity: node.identity,
                    node_id: node.id,
                    node_name: name.clone(),
                    name: text(sink.get("description")).unwrap_or_else(|| name.clone()),
                    priority: text(node.properties.get("priority.session"))
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0),
                    is_wave: wave_name(&name),
                    properties: node.properties,
                });
            }
        }
        let mut pulse_by_serial = HashMap::new();
        for input in pulse_inputs {
            let serial = required_text(
                input.get("properties").and_then(|p| p.get("object.serial")),
                "sink input object.serial",
            )?;
            if pulse_by_serial.insert(serial, input).is_some() {
                return Err(identity_error("Duplicate Pulse stream serial"));
            }
        }
        let mut source_mutes = HashMap::new();
        for source in pulse_sources {
            let name = required_text(source.get("name"), "source name")?;
            let muted = source
                .get("mute")
                .and_then(Value::as_bool)
                .ok_or_else(|| unavailable("Missing capture mute"))?;
            if source_mutes.insert(name.clone(), (muted, source)).is_some() {
                return Err(identity_error("Duplicate Pulse capture name"));
            }
        }
        for node in &graph.nodes {
            let props = &node.properties;
            let class = props
                .get("media.class")
                .and_then(Value::as_str)
                .unwrap_or("");
            if node.name.starts_with("openwave_") {
                continue;
            }
            if class == "Stream/Output/Audio" {
                let pulse = pulse_by_serial.get(&node.identity.object_serial);
                let pulse_index = pulse
                    .map(|p| required_number(p.get("index"), "sink input index"))
                    .transpose()?;
                let sink =
                    if let Some(pulse) = pulse {
                        let index = required_number(pulse.get("sink"), "stream destination")?;
                        Some(sink_indices.get(&index).cloned().ok_or_else(|| {
                            unavailable("Partial graph: stream destination absent")
                        })?)
                    } else {
                        None
                    };
                graph.streams.push(StreamSnapshot {
                    identity: node.identity.clone(),
                    node_id: node.id,
                    node_name: node.name.clone(),
                    app_name: text(props.get("application.name"))
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| node.name.clone()),
                    binary: text(props.get("application.process.binary")).unwrap_or_default(),
                    sink,
                    pulse_index,
                });
            }
            if class == "Audio/Source" && !node.name.ends_with(".monitor") {
                let mut properties = number(props.get("device.id"))
                    .and_then(|id| devices.get(&id))
                    .cloned()
                    .unwrap_or_default();
                for key in [
                    "object.serial",
                    "object.id",
                    "node.name",
                    "audio.channels",
                    "audio.position",
                    "media.class",
                ] {
                    properties.remove(key);
                }
                properties.extend(props.clone());
                let channels = props
                    .get("audio.channels")
                    .map(|v| required_number(Some(v), "capture channels"))
                    .transpose()?;
                let muted = match source_mutes.get(&node.name) {
                    Some((mute, pulse)) => {
                        check_pulse_identity(pulse, node)?;
                        Observation::Known(*mute)
                    }
                    None => {
                        Observation::Unknown(unavailable("Capture mute observation unavailable"))
                    }
                };
                graph.captures.push(CaptureSnapshot {
                    identity: node.identity.clone(),
                    node_id: node.id,
                    node_name: node.name.clone(),
                    name: text(props.get("node.description")).unwrap_or_else(|| node.name.clone()),
                    muted,
                    channels,
                    properties,
                });
            }
        }
        graph.ports.sort_by_key(|p| p.id);
        graph.outputs.sort_by(|a, b| {
            b.priority
                .cmp(&a.priority)
                .then_with(|| a.node_name.cmp(&b.node_name))
        });
        graph.captures.sort_by(|a, b| {
            a.node_name
                .cmp(&b.node_name)
                .then_with(|| a.node_id.cmp(&b.node_id))
        });
        Ok(graph)
    }
}
fn check_pulse_identity(pulse: &Value, node: &GraphNode) -> Result<()> {
    if let Some(serial) = text(pulse.get("properties").and_then(|p| p.get("object.serial"))) {
        if serial != node.identity.object_serial {
            return Err(identity_error("Pulse/PipeWire generation mismatch"));
        }
    }
    Ok(())
}

/// Injectable process boundary, also used by deterministic graph lifecycle regressions.
/// Implementations must report failures, not infer success from a live process.
pub trait RoutingChild: Send {
    fn running(&mut self) -> Result<bool>;
    fn terminate(&mut self) -> Result<()>;
}
impl RoutingChild for OwnedChild {
    fn running(&mut self) -> Result<bool> {
        Ok(self.try_wait()?.is_none())
    }
    fn terminate(&mut self) -> Result<()> {
        OwnedChild::terminate(self)
    }
}
pub trait GraphBackend: Send {
    fn snapshot(&mut self) -> Result<GraphSnapshot>;
    fn write_definitions(&mut self, mixes: &Mixes) -> Result<()>;
    fn create_sink(&mut self, name: &str, description: &str, owner: &str) -> Result<u32>;
    fn unload_module(&mut self, module: u32) -> Result<()>;
    fn destroy_node(&mut self, node: u32) -> Result<()>;
    fn move_stream(&mut self, stream: &StreamSnapshot, sink: &str) -> Result<()>;
    fn link(&mut self, source: u32, target: u32) -> Result<()>;
    fn set_level(&mut self, node: u32, level: f64, muted: bool) -> Result<()>;
    fn set_capture_mute(&mut self, node: &CaptureSnapshot, muted: bool) -> Result<()>;
    fn spawn_loopback(
        &mut self,
        name: &str,
        owner: &str,
        description: Option<&str>,
    ) -> Result<Box<dyn RoutingChild>>;
    fn spawn_filter(&mut self, config: &Path) -> Result<Box<dyn RoutingChild>>;
}

pub struct SubprocessPipeWire {
    paths: RuntimePaths,
    runner: CommandRunner,
}
impl SubprocessPipeWire {
    pub fn new(paths: RuntimePaths, cancel: Arc<AtomicBool>) -> Self {
        Self {
            paths,
            runner: CommandRunner::new(cancel),
        }
    }
    fn run(&self, program: &str, args: &[String]) -> Result<Vec<u8>> {
        Ok(self.runner.run(program, args, COMMAND_TIMEOUT)?.stdout)
    }
    fn json(&self, program: &str, args: &[String]) -> Result<Value> {
        serde_json::from_slice(&self.run(program, args)?)
            .map_err(|e| unavailable(format!("{program}: {e}")))
    }
    fn command(&self, program: &str, args: &[String]) -> Result<()> {
        self.run(program, args).map(|_| ())
    }
}
impl GraphBackend for SubprocessPipeWire {
    fn snapshot(&mut self) -> Result<GraphSnapshot> {
        let objects = self.json("pw-dump", &[])?;
        let cookie = GraphSnapshot::core_cookie(&objects)?;
        let mut pulse = Vec::with_capacity(4);
        for kind in ["sinks", "sources", "sink-inputs", "modules"] {
            pulse.push(self.json(
                "pactl",
                &["--format=json".into(), "list".into(), kind.into()],
            )?);
        }
        let default = if pulse[0].as_array().is_some_and(|v| !v.is_empty()) {
            let raw = self.run("pactl", &["get-default-sink".into()])?;
            Some(
                String::from_utf8(raw)
                    .map_err(|_| unavailable("Invalid default sink text"))?
                    .trim()
                    .to_owned(),
            )
        } else {
            None
        };
        let after = self.json("pw-dump", &[])?;
        if GraphSnapshot::core_cookie(&after)? != cookie {
            return Err(identity_error("PipeWire server restarted during discovery"));
        }
        let graph = GraphSnapshot::parse(
            &objects, &pulse[0], &pulse[1], &pulse[2], &pulse[3], default,
        )?;
        // A cookie alone cannot detect a node recycled between the independent clients.
        let later_nodes: HashMap<_, _> = after
            .as_array()
            .expect("validated array")
            .iter()
            .filter(|o| o.get("type").and_then(Value::as_str) == Some("PipeWire:Interface:Node"))
            .map(|o| {
                (
                    number(o.get("id")),
                    text(
                        o.get("info")
                            .and_then(|i| i.get("props"))
                            .and_then(|p| p.get("object.serial")),
                    ),
                )
            })
            .collect();
        for node in &graph.nodes {
            let expected = text(node.properties.get("object.serial"));
            if later_nodes.get(&Some(node.id)) != Some(&expected) {
                return Err(identity_error("PipeWire nodes changed during discovery"));
            }
        }
        Ok(graph)
    }
    fn write_definitions(&mut self, mixes: &Mixes) -> Result<()> {
        crate::setup::write_mixes_conf(&self.paths, mixes)
    }
    fn create_sink(&mut self, name: &str, description: &str, owner: &str) -> Result<u32> {
        let props = routing::pulse_properties(json!({"node.description":description,"media.name":name,"openwave.owner":owner,"priority.session":0,"monitor.channel-volumes":true,"state.restore-props":false}).as_object().expect("object"))?;
        let output = self.run(
            "pactl",
            &[
                "load-module".into(),
                "module-null-sink".into(),
                format!("sink_name={name}"),
                "channels=2".into(),
                "channel_map=front-left,front-right".into(),
                format!("sink_properties={props}"),
            ],
        )?;
        std::str::from_utf8(&output)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .ok_or_else(|| unavailable("Invalid Pulse module handle"))
    }
    fn unload_module(&mut self, module: u32) -> Result<()> {
        self.command("pactl", &["unload-module".into(), module.to_string()])
    }
    fn destroy_node(&mut self, node: u32) -> Result<()> {
        self.command("pw-cli", &["destroy".into(), node.to_string()])
    }
    fn move_stream(&mut self, stream: &StreamSnapshot, sink: &str) -> Result<()> {
        let index = stream
            .pulse_index
            .ok_or_else(|| unavailable(format!("No move capability for {}", stream.node_name)))?;
        self.command(
            "pactl",
            &["move-sink-input".into(), index.to_string(), sink.into()],
        )
    }
    fn link(&mut self, source: u32, target: u32) -> Result<()> {
        self.command("pw-link", &[source.to_string(), target.to_string()])
    }
    fn set_level(&mut self, node: u32, level: f64, muted: bool) -> Result<()> {
        // Set attenuation before unmuting. On a mute request mute first, even if volume then fails.
        if muted {
            self.command("wpctl", &["set-mute".into(), node.to_string(), "1".into()])?;
        }
        // Saved source/send/master levels already use wpctl's normalized volume scale.
        self.command(
            "wpctl",
            &["set-volume".into(), node.to_string(), level.to_string()],
        )?;
        if !muted {
            self.command("wpctl", &["set-mute".into(), node.to_string(), "0".into()])?;
        }
        Ok(())
    }
    fn set_capture_mute(&mut self, node: &CaptureSnapshot, muted: bool) -> Result<()> {
        self.command(
            "wpctl",
            &[
                "set-mute".into(),
                node.node_id.to_string(),
                if muted { "1" } else { "0" }.into(),
            ],
        )
    }
    fn spawn_loopback(
        &mut self,
        name: &str,
        owner: &str,
        description: Option<&str>,
    ) -> Result<Box<dyn RoutingChild>> {
        let capture_name = format!("{name}_cap");
        let capture = json!({"node.name":capture_name,"media.name":capture_name,"node.autoconnect":false,"audio.channels":2,"audio.position":["FL","FR"],"openwave.owner":owner});
        let mut playback = json!({"node.name":name,"media.name":name,"node.autoconnect":false,"audio.channels":2,"audio.position":["FL","FR"],"openwave.owner":owner});
        if let Some(description) = description {
            playback.as_object_mut().expect("object").extend(json!({"media.class":"Audio/Source","node.virtual":true,"node.description":description}).as_object().expect("object").clone());
        }
        let args = [
            format!(
                "--capture-props={}",
                routing::properties(capture.as_object().expect("object"))?
            ),
            format!(
                "--playback-props={}",
                routing::properties(playback.as_object().expect("object"))?
            ),
        ];
        Ok(Box::new(OwnedChild::spawn_in(
            &self.paths,
            "pw-loopback",
            &args,
            Stdio::null(),
        )?))
    }
    fn spawn_filter(&mut self, config: &Path) -> Result<Box<dyn RoutingChild>> {
        let config = config
            .to_str()
            .ok_or_else(|| OperationError::invalid("Non-UTF8 filter config path"))?;
        Ok(Box::new(OwnedChild::spawn_in(
            &self.paths,
            "pipewire",
            &["-c".into(), config.into()],
            Stdio::null(),
        )?))
    }
}

#[derive(Clone)]
struct CaptureMuteRequest {
    binding: CaptureBinding,
    muted: bool,
    written: Option<NodeIdentity>,
}
#[derive(Default)]
struct Control {
    revision: u64,
    desired: Option<Arc<DesiredState>>,
    bindings: IndexMap<String, CaptureBinding>,
    capture_requests: IndexMap<String, CaptureMuteRequest>,
    wake: bool,
    stopped: bool,
}
struct Shared {
    control: Mutex<Control>,
    wake: Condvar,
}
pub struct Mixer {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<(Reconciler, thread::Result<Result<()>>)>>,
    cleanup: Option<Reconciler>,
    failure: Option<OperationError>,
}
impl Mixer {
    pub fn start(
        paths: RuntimePaths,
        readiness: CaptureReadiness,
    ) -> Result<(Self, mpsc::Receiver<MixerEvent>)> {
        // Commands finish within their own deadline; shutdown then uses the same boundary
        // to restore streams. Cancelling that boundary would cancel owed restoration.
        Self::start_with_backend(
            Box::new(SubprocessPipeWire::new(
                paths,
                Arc::new(AtomicBool::new(false)),
            )),
            readiness,
            POLL_INTERVAL,
        )
    }
    pub fn start_with_backend(
        backend: Box<dyn GraphBackend>,
        readiness: CaptureReadiness,
        interval: Duration,
    ) -> Result<(Self, mpsc::Receiver<MixerEvent>)> {
        if interval.is_zero() {
            return Err(OperationError::invalid("Mixer interval must be positive"));
        }
        let shared = Arc::new(Shared {
            control: Mutex::new(Control::default()),
            wake: Condvar::new(),
        });
        let worker_shared = shared.clone();
        let (sender, receiver) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("openwave-mixer".into())
            .spawn(move || {
                let mut worker = Reconciler::new(backend, readiness, worker_shared, sender);
                let result = catch_unwind(AssertUnwindSafe(|| worker.run(interval)));
                (worker, result)
            })?;
        Ok((
            Self {
                shared,
                thread: Some(thread),
                cleanup: None,
                failure: None,
            },
            receiver,
        ))
    }
    pub fn set_desired(
        &self,
        revision: u64,
        desired: Arc<DesiredState>,
        bindings: IndexMap<String, CaptureBinding>,
    ) -> Result<()> {
        let mut control = self
            .shared
            .control
            .lock()
            .map_err(|_| unavailable("Mixer control lock poisoned"))?;
        if control.stopped {
            return Err(OperationError::new(ErrorCode::Frozen, "Mixer stopped"));
        }
        if control.desired.is_some() && revision < control.revision {
            return Err(OperationError::invalid("Stale mixer revision"));
        }
        for (name, binding) in &bindings {
            let owners: HashSet<_> = desired
                .sources
                .iter()
                .filter(|(_, s)| s.kind == SourceKind::Device && s.node_name == *name)
                .map(|(id, _)| id)
                .collect();
            if owners.is_empty()
                || owners.len() != binding.owners.len()
                || !owners.iter().all(|id| binding.owners.contains(id))
            {
                return Err(identity_error(
                    "Capture binding does not match exact source owners",
                ));
            }
        }
        control.capture_requests.retain(|name, request| {
            bindings
                .get(name)
                .is_some_and(|current| same_binding(current, &request.binding))
        });
        control.revision = revision;
        control.desired = Some(desired);
        control.bindings = bindings;
        control.wake = true;
        self.shared.wake.notify_one();
        Ok(())
    }
    pub fn set_capture_mute(
        &self,
        node_name: String,
        binding: CaptureBinding,
        muted: bool,
    ) -> Result<()> {
        let mut control = self
            .shared
            .control
            .lock()
            .map_err(|_| unavailable("Mixer control lock poisoned"))?;
        if control.stopped {
            return Err(OperationError::new(ErrorCode::Frozen, "Mixer stopped"));
        }
        if !control
            .bindings
            .get(&node_name)
            .is_some_and(|current| same_binding(current, &binding))
        {
            return Err(identity_error("Capture binding expired"));
        }
        control.capture_requests.insert(
            node_name,
            CaptureMuteRequest {
                binding,
                muted,
                written: None,
            },
        );
        control.wake = true;
        self.shared.wake.notify_one();
        Ok(())
    }
    /// Drain the worker once, retaining its exact graph ownership until cleanup succeeds.
    /// Retries run on this caller; callers must keep shutdown off the UI thread.
    /// A worker panic remains an error even if a later attempt cleans its remnants.
    pub fn stop(&mut self) -> Result<()> {
        {
            let mut control = self.shared.control.lock().unwrap_or_else(|poisoned| {
                self.failure
                    .get_or_insert_with(|| unavailable("Mixer control lock poisoned"));
                poisoned.into_inner()
            });
            control.stopped = true;
            control.capture_requests.clear();
            self.shared.wake.notify_one();
        }
        let outcome = if let Some(thread) = self.thread.take() {
            match thread.join() {
                Ok((worker, result)) => {
                    self.cleanup = Some(worker);
                    result
                }
                Err(error) => Err(error),
            }
        } else if let Some(worker) = self.cleanup.as_mut() {
            catch_unwind(AssertUnwindSafe(|| worker.teardown()))
        } else {
            Ok(Ok(()))
        };
        let result = match outcome {
            Ok(result) => result,
            Err(_) => {
                let error =
                    unavailable("Mixer worker panicked; cleanup completion cannot be guaranteed");
                self.failure.get_or_insert_with(|| error.clone());
                Err(error)
            }
        };
        if result.is_ok() {
            self.cleanup = None;
        }
        match (&self.failure, result) {
            (Some(failure), Err(error)) => Err(unavailable(format!("{failure}; {error}"))),
            (Some(failure), Ok(())) => Err(failure.clone()),
            (None, result) => result,
        }
    }
}
// Drop makes one final bounded attempt and reports failure. Only explicit stop
// retains a retryable owner; dropping it cannot authorize broader graph cleanup.
impl Drop for Mixer {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            log::warn!("Mixer shutdown: {error}");
        }
    }
}

#[derive(Clone)]
struct OwnedSink {
    cookie: u32,
    module: u32,
    owner: String,
    missing: u8,
}
#[derive(Clone)]
struct PersistentSink {
    identity: NodeIdentity,
    definition: Mix,
}
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum RouteKey {
    Cell(SourceId, MixId),
    Capture(MixId),
    Output(MixId),
}
#[derive(Clone, Debug, PartialEq)]
struct RouteSpec {
    source: NodeIdentity,
    target: Option<NodeIdentity>,
    description: Option<String>,
}
struct Route {
    child: Box<dyn RoutingChild>,
    spec: RouteSpec,
    name: String,
    owner: String,
    applied: Option<(NodeIdentity, f64, bool)>,
    missing: u8,
}
#[derive(Clone, Debug, PartialEq)]
struct EffectSpec {
    raw: NodeIdentity,
    channels: u32,
    source: Source,
}
struct Effect {
    child: Box<dyn RoutingChild>,
    _file: tempfile::NamedTempFile,
    spec: EffectSpec,
    owner: String,
    name: String,
    started: Instant,
}
#[derive(Clone)]
struct Master {
    identity: NodeIdentity,
    requested: (f64, bool),
    confirmed: bool,
    // Last successfully restored generation remains connected on successful edits.
    restored: bool,
    emitted: Option<(f64, bool)>,
    emitted_revision: Option<u64>,
}
struct Reconciler {
    backend: Box<dyn GraphBackend>,
    readiness: CaptureReadiness,
    shared: Arc<Shared>,
    events: mpsc::Sender<MixerEvent>,
    desired: Arc<DesiredState>,
    revision: Option<u64>,
    matcher: NormalizedMatcher,
    normalized: HashMap<NodeIdentity, (StreamSnapshot, NormalizedStream)>,
    cookie: Option<u32>,
    definitions: Option<Mixes>,
    last: MixerObservation,
    discovery_error: Option<OperationError>,
    sinks: HashMap<String, OwnedSink>,
    persistent: HashMap<String, PersistentSink>,
    removed: HashMap<String, PersistentSink>,
    routes: HashMap<RouteKey, Route>,
    effects: HashMap<SourceId, Effect>,
    effect_retry: HashMap<SourceId, (EffectSpec, Instant)>,
    masters: HashMap<MixId, Master>,
    moved: HashMap<NodeIdentity, Option<String>>,
    pending_masters: Vec<(MixId, NodeIdentity, f64, bool)>,
}
impl Reconciler {
    fn new(
        backend: Box<dyn GraphBackend>,
        readiness: CaptureReadiness,
        shared: Arc<Shared>,
        events: mpsc::Sender<MixerEvent>,
    ) -> Self {
        Self {
            backend,
            readiness,
            shared,
            events,
            desired: Arc::new(DesiredState::default()),
            revision: None,
            matcher: NormalizedMatcher::new(&Sources::new()),
            normalized: HashMap::new(),
            cookie: None,
            definitions: None,
            last: MixerObservation::default(),
            sinks: HashMap::new(),
            persistent: HashMap::new(),
            discovery_error: None,
            removed: HashMap::new(),
            routes: HashMap::new(),
            effects: HashMap::new(),
            effect_retry: HashMap::new(),
            masters: HashMap::new(),
            moved: HashMap::new(),
            pending_masters: Vec::new(),
        }
    }
    fn run(&mut self, interval: Duration) -> Result<()> {
        loop {
            let update = {
                let mut control = self
                    .shared
                    .control
                    .lock()
                    .map_err(|_| unavailable("Mixer control lock poisoned"))?;
                if control.stopped {
                    break;
                }
                control.wake = false;
                control
                    .desired
                    .as_ref()
                    .map(|desired| (control.revision, desired.clone()))
            };
            if let Some((revision, desired)) = update {
                self.update(revision, desired);
                self.cycle();
            }
            let control = self
                .shared
                .control
                .lock()
                .map_err(|_| unavailable("Mixer control lock poisoned"))?;
            if control.stopped {
                break;
            }
            if !control.wake {
                let _guard = self
                    .shared
                    .wake
                    .wait_timeout(control, interval)
                    .map_err(|_| unavailable("Mixer control lock poisoned"))?;
            }
        }
        self.teardown()
    }
    fn update(&mut self, revision: u64, desired: Arc<DesiredState>) {
        if self.revision == Some(revision) && Arc::ptr_eq(&self.desired, &desired) {
            return;
        }
        for (name, evidence) in &self.persistent {
            if !desired.mixes.values().any(|mix| mix.sink == *name) {
                self.removed.insert(name.clone(), evidence.clone());
            }
        }
        self.matcher = NormalizedMatcher::new(&desired.sources);
        self.desired = desired;
        self.revision = Some(revision);
    }
    fn cycle(&mut self) {
        self.pending_masters.clear();
        self.discovery_error = None;
        let result = self.backend.snapshot();
        let mut graph = match result {
            Ok(graph) => graph,
            Err(error) => {
                let mut observation = self.last.clone();
                observation.observation = Observation::Unknown(error.clone());
                observation.silent_sources.clear();
                for capture in &mut observation.captures {
                    capture.muted = Observation::Unknown(error.clone());
                }
                observation.errors = vec![issue("graph", error)];
                let _ = self.events.send(MixerEvent::Observed(observation));
                return; // Unknown means no mutation, including cleanup.
            }
        };
        let mut errors = Vec::new();
        let reconciled = if let Err(error) = self.observe_cookie(graph.cookie) {
            errors.push(issue("graph", error));
            None
        } else {
            match self.reconcile(&mut graph, &mut errors) {
                Ok(blocked) => Some(blocked),
                Err(error) => {
                    errors.push(issue("routing", error));
                    None
                }
            }
        };
        if let Some(error) = self.discovery_error.take() {
            let mut observation = self.last.clone();
            observation.observation = Observation::Unknown(error.clone());
            observation.silent_sources.clear();
            for capture in &mut observation.captures {
                capture.muted = Observation::Unknown(error.clone());
            }
            observation.errors = errors;
            let _ = self.events.send(MixerEvent::Observed(observation));
            return;
        }
        let meter_targets = self.meter_targets(&graph, &mut errors);
        let mix_identities = self
            .desired
            .mixes
            .iter()
            .filter_map(|(id, mix)| {
                self.mix_node(&graph, mix)
                    .map(|node| (id.clone(), node.identity.clone()))
            })
            .collect();
        let silent_sources = reconciled.map_or_else(HashMap::new, |blocked| {
            self.silent_sources(&graph, &blocked)
        });
        // Reconciliation can block while a newer intent arrives. Mask those samples too,
        // not just captures written by this cycle.
        let control = self
            .shared
            .control
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        for capture in &mut graph.captures {
            if control.capture_requests.contains_key(&capture.node_name) {
                capture.muted = Observation::Unknown(unavailable(
                    "Capture mute is awaiting a matching observation",
                ));
            }
        }
        self.last = MixerObservation {
            observation: Observation::Known(()),
            captures: graph.captures.clone(),
            streams: graph.streams.clone(),
            outputs: graph.outputs.clone(),
            default_sink: graph.default_sink.clone(),
            meter_targets,
            mix_identities,
            silent_sources,
            errors,
            revision: self.revision.unwrap_or(0),
        };
        let _ = self.events.send(MixerEvent::Observed(self.last.clone()));
        drop(control);
        for (mix, identity, level, muted) in self.pending_masters.drain(..) {
            let Ok(control) = self.shared.control.lock() else {
                continue;
            };
            let Some(revision) = self.revision else {
                continue;
            };
            if control.stopped
                || control.revision != revision
                || self.last.mix_identities.get(&mix) != Some(&identity)
            {
                continue;
            }
            if self
                .events
                .send(MixerEvent::MasterObserved {
                    mix: mix.clone(),
                    revision,
                    identity,
                    level,
                    muted,
                })
                .is_ok()
            {
                if let Some(master) = self.masters.get_mut(&mix) {
                    master.emitted = Some((level, muted));
                    master.emitted_revision = Some(revision);
                }
            }
        }
    }
    fn silent_sources(
        &self,
        graph: &GraphSnapshot,
        blocked: &HashSet<SourceId>,
    ) -> HashMap<SourceId, NodeIdentity> {
        self.desired
            .sources
            .iter()
            .filter(|(id, source)| {
                source.kind == SourceKind::Device && source.muted && !blocked.contains(*id)
            })
            .filter_map(|(id, source)| {
                let mut captures = graph
                    .captures
                    .iter()
                    .filter(|capture| capture.node_name == source.node_name);
                let capture = captures.next()?;
                if captures.next().is_some() {
                    return None;
                }
                let silent = self.routes.iter().all(|(key, route)| {
                    if !matches!(key, RouteKey::Cell(owner, _) if owner == id) {
                        return true;
                    }
                    route.applied.as_ref().is_some_and(|(identity, _, muted)| {
                        *muted
                            && graph
                                .owned(&route.name, &route.owner)
                                .is_some_and(|node| node.identity == *identity)
                    })
                });
                silent.then(|| (id.clone(), capture.identity.clone()))
            })
            .collect()
    }
    fn observe_cookie(&mut self, cookie: u32) -> Result<()> {
        if self.cookie.is_some_and(|old| old != cookie) {
            // The OS process handles are ours; stale PipeWire/Pulse IDs are not.
            self.stop_routes()?;
            self.stop_effects()?;
            self.sinks.clear();
            self.persistent.clear();
            self.removed.clear();
            self.masters.clear();
            self.moved.clear();
            self.normalized.clear();
        }
        self.cookie = Some(cookie);
        Ok(())
    }
    fn ensure_sink(
        &mut self,
        graph: &GraphSnapshot,
        name: &str,
        description: &str,
        mix: Option<&Mix>,
    ) -> Result<bool> {
        if let Some(node) = graph.node(name) {
            if !graph.sinks.contains_key(name) {
                return Err(unavailable(format!(
                    "Sink {name} is not available to Pulse"
                )));
            }
            if let Some(handle) = self.sinks.get_mut(name) {
                if handle.cookie == graph.cookie
                    && node
                        .properties
                        .get("openwave.owner")
                        .and_then(Value::as_str)
                        == Some(handle.owner.as_str())
                    && number(node.properties.get("pulse.module.id")) == Some(handle.module)
                {
                    handle.missing = 0;
                    return Ok(true);
                }
                return Err(identity_error(format!("Foreign sink occupies {name}")));
            }
            if let Some(mix) = mix {
                let previous = self.persistent.get(name).filter(|p| {
                    p.identity == node.identity && definition_matches(node, &p.definition)
                });
                if previous.is_some() {
                    return Ok(true);
                }
                if definition_matches(node, mix) {
                    self.persistent.insert(
                        name.into(),
                        PersistentSink {
                            identity: node.identity.clone(),
                            definition: mix.clone(),
                        },
                    );
                    return Ok(true);
                }
            }
            return Err(identity_error(format!(
                "Unproven existing sink {name}; generated configuration has been refreshed. Restart the audio session explicitly to load it."
            )));
        }
        if graph.names.contains_key(name) {
            return Err(identity_error(format!("Ambiguous sink name {name}")));
        }
        if let Some(handle) = self.sinks.get_mut(name) {
            handle.missing = handle.missing.saturating_add(1);
            if handle.missing < 3 {
                return Ok(false);
            }
            // Never unload a handle that no longer has current name+owner evidence.
            self.sinks.remove(name);
        }
        let owner = uuid::Uuid::new_v4().simple().to_string();
        let module = self.backend.create_sink(name, description, &owner)?;
        self.sinks.insert(
            name.into(),
            OwnedSink {
                cookie: graph.cookie,
                module,
                owner,
                missing: 0,
            },
        );
        Ok(false)
    }
    fn sync_capture_mutes(
        &mut self,
        graph: &mut GraphSnapshot,
        errors: &mut Vec<OperationIssue>,
    ) -> Result<()> {
        let names: Vec<String> = self
            .shared
            .control
            .lock()
            .map_err(|_| unavailable("Mixer control lock poisoned"))?
            .capture_requests
            .keys()
            .cloned()
            .collect();
        for name in names {
            // Recheck each binding immediately before dispatch, including after a blocked
            // earlier write; a queued request never inherits replacement ownership.
            let request = {
                let control = self
                    .shared
                    .control
                    .lock()
                    .map_err(|_| unavailable("Mixer control lock poisoned"))?;
                if control.stopped {
                    return Ok(());
                }
                control
                    .capture_requests
                    .get(&name)
                    .filter(|request| {
                        control
                            .bindings
                            .get(&name)
                            .is_some_and(|current| same_binding(current, &request.binding))
                    })
                    .cloned()
            };
            let Some(request) = request else {
                continue;
            };
            let mut captures = graph.captures.iter_mut().filter(|c| c.node_name == name);
            let Some(capture) = captures.next() else {
                continue;
            }; // Keep same-epoch intent while offline.
            if captures.next().is_some() {
                errors.push(issue(&name, "Ambiguous capture identity"));
                continue;
            }
            if request.written.as_ref() == Some(&capture.identity)
                && capture.muted.known() == Some(&request.muted)
            {
                let mut control = self
                    .shared
                    .control
                    .lock()
                    .map_err(|_| unavailable("Mixer control lock poisoned"))?;
                if control.capture_requests.get(&name).is_some_and(|current| {
                    current.muted == request.muted
                        && current.written == request.written
                        && same_binding(&current.binding, &request.binding)
                }) {
                    control.capture_requests.shift_remove(&name);
                }
                continue;
            }
            let result = self.backend.set_capture_mute(capture, request.muted);
            // The graph was sampled before this write, successful or not. It cannot
            // authorize feedback until a later sample confirms this exact incarnation.
            capture.muted = Observation::Unknown(unavailable(
                "Capture mute is awaiting a matching observation",
            ));
            match result {
                Ok(()) => {
                    let mut control = self
                        .shared
                        .control
                        .lock()
                        .map_err(|_| unavailable("Mixer control lock poisoned"))?;
                    if let Some(current) =
                        control.capture_requests.get_mut(&name).filter(|current| {
                            current.muted == request.muted
                                && same_binding(&current.binding, &request.binding)
                        })
                    {
                        current.written = Some(capture.identity.clone());
                    }
                }
                Err(error) => errors.push(issue(&name, error)),
            }
        }
        Ok(())
    }
    fn sync_masters(
        &mut self,
        graph: &GraphSnapshot,
        available: &HashSet<MixId>,
        errors: &mut Vec<OperationIssue>,
    ) -> HashSet<MixId> {
        let mut ready = HashSet::new();
        for (id, mix) in &self.desired.mixes {
            if !available.contains(id) {
                self.masters.remove(id);
                continue;
            }
            let (Some(sink), Some(node)) = (graph.sinks.get(&mix.sink), graph.node(&mix.sink))
            else {
                continue;
            };
            let observed = (sink.level, sink.muted);
            let saved = self
                .desired
                .matrix
                .volumes
                .get(id)
                .map(|s| (s.volume, s.muted));
            let desired = saved.unwrap_or(observed);
            if let Some(master) = self
                .masters
                .get_mut(id)
                .filter(|m| m.identity == node.identity)
            {
                // A controller acknowledgement of our observation is not a new fader edit.
                if master.confirmed
                    && master.emitted == Some(desired)
                    && (observed.0 - desired.0).abs() <= 0.001
                    && observed.1 == desired.1
                {
                    master.requested = desired;
                }
                if master.requested == desired {
                    if master.confirmed {
                        ready.insert(id.clone());
                        if master.emitted != Some(observed)
                            || master.emitted_revision != self.revision
                        {
                            self.pending_masters.push((
                                id.clone(),
                                node.identity.clone(),
                                observed.0,
                                observed.1,
                            ));
                        }
                        continue;
                    }
                    if (observed.0 - desired.0).abs() <= 0.001 && observed.1 == desired.1 {
                        master.confirmed = true;
                        master.restored = true;
                        ready.insert(id.clone());
                        self.pending_masters.push((
                            id.clone(),
                            node.identity.clone(),
                            observed.0,
                            observed.1,
                        ));
                        continue;
                    }
                }
            }
            let restored = self
                .masters
                .get(id)
                .is_some_and(|m| m.identity == node.identity && m.restored);
            match self.backend.set_level(node.id, desired.0, desired.1) {
                Ok(()) => {
                    self.masters.insert(
                        id.clone(),
                        Master {
                            identity: node.identity.clone(),
                            requested: desired,
                            confirmed: false,
                            restored,
                            emitted: None,
                            emitted_revision: None,
                        },
                    );
                    if restored {
                        ready.insert(id.clone());
                    }
                }
                Err(error) => {
                    // A failed edit must not manufacture confirmation on the next observation.
                    self.masters.remove(id);
                    errors.push(issue(format!("master:{id}"), error));
                }
            }
        }
        self.masters
            .retain(|id, _| self.desired.mixes.contains_key(id));
        ready
    }
    fn link_nodes(
        &mut self,
        graph: &mut GraphSnapshot,
        source: &NodeIdentity,
        target: &NodeIdentity,
    ) -> Result<bool> {
        let (Some(source), Some(target)) = (graph.identity(source), graph.identity(target)) else {
            return Ok(false);
        };
        let outputs: Vec<_> = graph
            .ports
            .iter()
            .filter(|p| p.node == source.id && p.output)
            .cloned()
            .collect();
        let inputs: Vec<_> = graph
            .ports
            .iter()
            .filter(|p| p.node == target.id && !p.output)
            .cloned()
            .collect();
        if outputs.is_empty() || inputs.is_empty() {
            return Ok(false);
        }
        for (index, dest) in inputs.iter().enumerate() {
            let origin = outputs
                .iter()
                .find(|p| !p.channel.is_empty() && p.channel == dest.channel)
                .unwrap_or(&outputs[index % outputs.len()]);
            let edge = (origin.id, dest.id);
            if !graph.links.contains(&edge) {
                self.backend.link(edge.0, edge.1)?;
                graph.links.insert(edge);
            }
        }
        Ok(true)
    }
    fn drop_route(&mut self, key: &RouteKey) -> Result<()> {
        if let Some(route) = self.routes.get_mut(key) {
            route.child.terminate()?;
        }
        self.routes.remove(key);
        Ok(())
    }
    fn stop_routes(&mut self) -> Result<()> {
        let keys: Vec<_> = self.routes.keys().cloned().collect();
        let mut errors = Vec::new();
        for key in keys {
            if let Err(error) = self.drop_route(&key) {
                errors.push(error.to_string());
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(unavailable(errors.join("; ")))
        }
    }
    fn drop_effect(&mut self, id: &SourceId) -> Result<()> {
        if let Some(effect) = self.effects.get_mut(id) {
            effect.child.terminate()?;
        }
        self.effects.remove(id); // The private configuration lives exactly as long as its child.
        Ok(())
    }
    fn stop_effects(&mut self) -> Result<()> {
        let ids: Vec<_> = self.effects.keys().cloned().collect();
        let mut errors = Vec::new();
        for id in ids {
            if let Err(error) = self.drop_effect(&id) {
                errors.push(error.to_string());
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(unavailable(errors.join("; ")))
        }
    }
    fn route(
        &mut self,
        graph: &mut GraphSnapshot,
        key: RouteKey,
        spec: RouteSpec,
        name: String,
        level: f64,
        muted: bool,
    ) -> Result<bool> {
        let replace = match self.routes.get_mut(&key) {
            Some(route) => route.spec != spec || !route.child.running()?,
            None => false,
        };
        if replace {
            self.drop_route(&key)?;
        }
        if !self.routes.contains_key(&key) {
            if graph.names.contains_key(&name) || graph.names.contains_key(&format!("{name}_cap")) {
                return Err(identity_error(format!(
                    "Existing unowned route {name}; waiting for its exact owner to release it"
                )));
            }
            let owner = uuid::Uuid::new_v4().simple().to_string();
            let child = self
                .backend
                .spawn_loopback(&name, &owner, spec.description.as_deref())?;
            self.routes.insert(
                key,
                Route {
                    child,
                    spec,
                    name,
                    owner,
                    applied: None,
                    missing: 0,
                },
            );
            return Ok(false);
        }
        let route = self.routes.get_mut(&key).expect("inserted route");
        let cap_name = format!("{}_cap", route.name);
        let (Some(node), Some(cap)) = (
            graph.owned(&route.name, &route.owner),
            graph.owned(&cap_name, &route.owner),
        ) else {
            route.missing = route.missing.saturating_add(1);
            if route.missing >= 3 {
                self.drop_route(&key)?;
            }
            return Ok(false);
        };
        let node_identity = node.identity.clone();
        let capture_identity = cap.identity.clone();
        let applied = (node_identity.clone(), level, muted);
        route.missing = 0;
        if route.applied.as_ref() != Some(&applied) {
            self.backend.set_level(node.id, level, muted)?;
            route.applied = Some(applied);
        }
        let spec = route.spec.clone();
        // Always repair links, including on a process that never exited.
        if let Some(target) = &spec.target {
            if !self.link_nodes(graph, &node_identity, target)? {
                return Ok(false);
            }
        }
        self.link_nodes(graph, &spec.source, &capture_identity)
    }
    fn sync_effects(
        &mut self,
        graph: &mut GraphSnapshot,
        errors: &mut Vec<OperationIssue>,
    ) -> Result<HashMap<SourceId, Option<NodeIdentity>>> {
        let desired = self.desired.clone();
        let mut effective = HashMap::new();
        let mut wanted = HashSet::new();
        for (id, source) in &desired.sources {
            if source.kind != SourceKind::Device
                || !source.fx.as_ref().is_some_and(|fx| fx.active())
            {
                continue;
            }
            effective.insert(id.clone(), None);
            let captures: Vec<_> = graph
                .captures
                .iter()
                .filter(|c| c.node_name == source.node_name)
                .collect();
            if captures.len() != 1 {
                continue;
            }
            let capture = captures[0];
            let channels = match effects::capture_channels(capture.channels) {
                Ok(channels) => channels,
                Err(error) => {
                    errors.push(issue(format!("fx:{id}"), error));
                    continue;
                }
            };
            wanted.insert(id.clone());
            let mut effect_source = source.clone();
            // Trim/mute/group and persisted channel metadata do not restart a filter.
            effect_source.level = 1.0;
            effect_source.muted = false;
            effect_source.group.clear();
            effect_source.extra.clear();
            let spec = EffectSpec {
                raw: capture.identity.clone(),
                channels,
                source: effect_source,
            };
            let replace = match self.effects.get_mut(id) {
                Some(effect) => effect.spec != spec || !effect.child.running()?,
                None => false,
            };
            if replace {
                self.drop_effect(id)?;
            }
            if !self.effects.contains_key(id) {
                if self
                    .effect_retry
                    .get(id)
                    .is_some_and(|(old, deadline)| old == &spec && Instant::now() < *deadline)
                {
                    errors.push(issue(
                        format!("fx:{id}"),
                        "Effects unavailable; retrying shortly",
                    ));
                    continue;
                }
                let name = effects::fx_node_name(id);
                if graph.names.contains_key(&name)
                    || graph.names.contains_key(&format!("{name}_cap"))
                {
                    errors.push(issue(
                        format!("fx:{id}"),
                        "Existing unowned filter blocks startup",
                    ));
                    continue;
                }
                self.effect_retry
                    .insert(id.clone(), (spec.clone(), Instant::now() + FX_RETRY));
                let owner = uuid::Uuid::new_v4().simple().to_string();
                let started = (|| -> Result<Effect> {
                    let rendered = effects::render_fx_config(source, channels, &owner)?
                        .ok_or_else(|| {
                            OperationError::invalid("Active effects rendered no filter")
                        })?;
                    let mut config: Value = serde_json::from_str(&rendered)?;
                    let modules = config["context.modules"]
                        .as_array_mut()
                        .ok_or_else(|| OperationError::invalid("Filter modules missing"))?;
                    let filter = modules
                        .iter_mut()
                        .find(|module| module["name"] == "libpipewire-module-filter-chain")
                        .ok_or_else(|| OperationError::invalid("Filter module missing"))?;
                    let props = &mut filter["args"]["capture.props"];
                    props["node.autoconnect"] = json!(false);
                    props["target.object"] = json!(spec.raw.object_serial);
                    let mut file = tempfile::Builder::new()
                        .prefix("openwave-fx-")
                        .suffix(".conf")
                        .tempfile()?;
                    serde_json::to_writer(&mut file, &config)?;
                    file.flush()?;
                    let child = self.backend.spawn_filter(file.path())?;
                    Ok(Effect {
                        child,
                        _file: file,
                        spec: spec.clone(),
                        owner,
                        name,
                        started: Instant::now(),
                    })
                })();
                match started {
                    Ok(effect) => {
                        self.effects.insert(id.clone(), effect);
                    }
                    Err(error) => {
                        errors.push(issue(format!("fx:{id}"), error));
                        continue;
                    }
                }
            }
            let effect = self.effects.get(id).expect("started effect");
            let name = effect.name.clone();
            let owner = effect.owner.clone();
            let raw = effect.spec.raw.clone();
            let timed_out = effect.started.elapsed() > FX_RETRY;
            let output = graph.owned(&name, &owner).map(|n| n.identity.clone());
            let input = graph
                .owned(&format!("{name}_cap"), &owner)
                .map(|n| n.identity.clone());
            let health = if let (Some(output), Some(input)) = (output, input) {
                self.link_nodes(graph, &raw, &input)
                    .map(|linked| if linked { Some(output) } else { None })
            } else {
                Ok(None)
            };
            match health {
                Ok(Some(output)) => {
                    effective.insert(id.clone(), Some(output));
                }
                other => {
                    let message = match other {
                        Err(error) => error.to_string(),
                        _ => "Effects starting or raw-input link unavailable".into(),
                    };
                    errors.push(issue(format!("fx:{id}"), message));
                    if timed_out {
                        self.drop_effect(id)?;
                    }
                }
            }
        }
        let stale: Vec<_> = self
            .effects
            .keys()
            .filter(|id| !wanted.contains(*id))
            .cloned()
            .collect();
        for id in stale {
            self.drop_effect(&id)?;
        }
        self.effect_retry.retain(|id, _| wanted.contains(id));
        Ok(effective)
    }
    fn reconcile(
        &mut self,
        graph: &mut GraphSnapshot,
        errors: &mut Vec<OperationIssue>,
    ) -> Result<HashSet<SourceId>> {
        let desired = self.desired.clone();
        let mut silence_blocked = HashSet::new();
        if self.definitions.as_ref() != Some(&desired.mixes) {
            self.backend.write_definitions(&desired.mixes)?;
            self.definitions = Some(desired.mixes.clone());
        }
        self.sync_capture_mutes(graph, errors)?;
        let mut wanted_sinks: HashSet<String> =
            desired.mixes.values().map(|m| m.sink.clone()).collect();
        let mut available = HashSet::new();
        for (id, mix) in &desired.mixes {
            match self.ensure_sink(graph, &mix.sink, &mix.description, Some(mix)) {
                Ok(true) => {
                    available.insert(id.clone());
                }
                Ok(false) => {}
                Err(error) => errors.push(issue(format!("mix:{id}"), error)),
            }
        }
        let ready = self.sync_masters(graph, &available, errors);
        let live_streams: HashSet<_> = graph.streams.iter().map(|s| &s.identity).collect();
        self.normalized.retain(|id, _| live_streams.contains(id));
        for stream in &graph.streams {
            let needs_update = self
                .normalized
                .get(&stream.identity)
                .is_none_or(|(old, _)| {
                    old.app_name != stream.app_name
                        || old.node_name != stream.node_name
                        || old.binary != stream.binary
                });
            if needs_update {
                self.normalized.insert(
                    stream.identity.clone(),
                    (stream.clone(), NormalizedStream::new(stream)),
                );
            }
        }
        let mut claims: IndexMap<SourceId, Vec<NodeIdentity>> = desired
            .sources
            .keys()
            .cloned()
            .map(|id| (id, Vec::new()))
            .collect();
        for stream in &graph.streams {
            if let Some((_, normalized)) = self.normalized.get(&stream.identity) {
                if let Some(owner) = self.matcher.owner(normalized) {
                    claims
                        .get_mut(owner)
                        .expect("desired owner")
                        .push(stream.identity.clone());
                }
            }
        }
        let effective = self.sync_effects(graph, errors)?;

        // Silencing is a global barrier: even a failed TERM or mute forbids opening
        // another member. Pending DSP and missing exact captures also retire old audio.
        let old_cells: Vec<_> = self
            .routes
            .keys()
            .filter(|key| matches!(key, RouteKey::Cell(_, _)))
            .cloned()
            .collect();
        for key in old_cells {
            let RouteKey::Cell(sid, mid) = &key else {
                unreachable!()
            };
            let source = desired.sources.get(sid);
            let (volume, muted) = desired
                .matrix
                .cells
                .get(&format!("{sid}.{mid}"))
                .map(|cell| (cell.volume, cell.muted))
                .unwrap_or((0.0, false));
            let raw_missing = source.is_some_and(|s| {
                s.kind == SourceKind::Device
                    && graph
                        .captures
                        .iter()
                        .filter(|c| c.node_name == s.node_name)
                        .count()
                        != 1
            });
            let fx_missing = effective.get(sid).is_some_and(Option::is_none);
            if source.is_none()
                || !ready.contains(mid)
                || volume <= 0.0
                || raw_missing
                || fx_missing
            {
                self.drop_route(&key)?;
                continue;
            }
            let source = source.expect("checked source");
            if source.muted || muted || source.level <= 0.0 {
                let route = self.routes.get_mut(&key).expect("existing cell");
                if !route.child.running()? {
                    self.drop_route(&key)?;
                    continue;
                }
                match graph.owned(&route.name, &route.owner) {
                    Some(node) => {
                        self.backend
                            .set_level(node.id, volume * source.level, true)
                            .map_err(|error| {
                                unavailable(format!(
                                    "Cannot silence {sid}; group handover deferred: {error}"
                                ))
                            })?;
                        route.applied = Some((node.identity.clone(), volume * source.level, true));
                    }
                    None => {
                        self.drop_route(&key)?;
                    }
                }
            }
        }

        let mut wanted_routes = HashSet::new();
        let mut wanted_moves = HashMap::new();
        for (id, source) in &desired.sources {
            let capture = if source.kind == SourceKind::App {
                let name = routing::source_sink_name(id);
                wanted_sinks.insert(name.clone());
                for stream_id in &claims[id] {
                    wanted_moves.insert(stream_id.clone(), name.clone());
                }
                match self.ensure_sink(graph, &name, &source.name, None) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(error) => {
                        errors.push(issue(format!("source:{id}"), error));
                        continue;
                    }
                }
                for stream_id in &claims[id] {
                    let Some(stream) = graph.streams.iter().find(|s| s.identity == *stream_id)
                    else {
                        continue;
                    };
                    if stream.sink.as_deref() != Some(&name) {
                        match self.backend.move_stream(stream, &name) {
                            Ok(()) => {
                                self.moved
                                    .entry(stream.identity.clone())
                                    .or_insert_with(|| {
                                        stream.sink.clone().filter(|s| !s.starts_with("openwave_"))
                                    });
                            }
                            Err(error) => errors.push(issue(
                                format!("stream:{}", stream.identity.object_serial),
                                error,
                            )),
                        }
                    }
                }
                graph.node(&name).map(|n| n.identity.clone())
            } else {
                let mut captures = graph
                    .captures
                    .iter()
                    .filter(|c| c.node_name == source.node_name);
                let raw = captures.next().map(|c| c.identity.clone());
                if captures.next().is_some() {
                    errors.push(issue(format!("source:{id}"), "Ambiguous selected capture"));
                    None
                } else {
                    effective.get(id).cloned().unwrap_or(raw)
                }
            };
            let Some(capture) = capture else {
                continue;
            };
            for (mid, mix) in &desired.mixes {
                if !ready.contains(mid) {
                    continue;
                }
                let (volume, muted) = desired
                    .matrix
                    .cells
                    .get(&format!("{id}.{mid}"))
                    .map(|cell| (cell.volume, cell.muted))
                    .unwrap_or((0.0, false));
                if volume <= 0.0 {
                    continue;
                }
                let Some(target) = graph.node(&mix.sink).map(|n| n.identity.clone()) else {
                    continue;
                };
                let key = RouteKey::Cell(id.clone(), mid.clone());
                wanted_routes.insert(key.clone());
                let spec = RouteSpec {
                    source: capture.clone(),
                    target: Some(target),
                    description: None,
                };
                if let Err(error) = self.route(
                    graph,
                    key,
                    spec,
                    routing::cell_route_name(id, mid),
                    volume * source.level,
                    source.muted || muted || source.level <= 0.0,
                ) {
                    errors.push(issue(format!("cell:{id}.{mid}"), error));
                    silence_blocked.insert(id.clone());
                }
            }
        }
        let preferred_wave = graph
            .outputs
            .iter()
            .filter(|output| {
                routing::eligible_output(output)
                    && output.is_wave
                    && wave_ready(output, &graph.captures, &self.readiness)
            })
            .min_by_key(|output| &output.node_name)
            .map(|output| output.node_name.clone());
        for (id, mix) in &desired.mixes {
            if !ready.contains(id) {
                continue;
            }
            let Some(source) = graph.node(&mix.sink).map(|n| n.identity.clone()) else {
                continue;
            };
            let key = RouteKey::Capture(id.clone());
            wanted_routes.insert(key.clone());
            let spec = RouteSpec {
                source: source.clone(),
                target: None,
                description: Some(mix.description.clone()),
            };
            if let Err(error) =
                self.route(graph, key, spec, routing::mix_capture_name(id), 1.0, false)
            {
                errors.push(issue(format!("capture:{id}"), error));
            }
            let decision = routing::resolve_output_with_wave(
                desired.matrix.output(id),
                &graph.outputs,
                graph.default_sink.as_deref(),
                preferred_wave.as_deref(),
            )?;
            if let OutputDecision::Monitor(output) = decision {
                if output.is_wave && !wave_ready(output, &graph.captures, &self.readiness) {
                    continue;
                }
                let key = RouteKey::Output(id.clone());
                wanted_routes.insert(key.clone());
                let spec = RouteSpec {
                    source,
                    target: Some(output.identity.clone()),
                    description: None,
                };
                if let Err(error) = self.route(
                    graph,
                    key,
                    spec,
                    format!("openwave_loop_output_{id}"),
                    1.0,
                    false,
                ) {
                    errors.push(issue(format!("output:{id}"), error));
                }
            }
        }
        let stale: Vec<_> = self
            .routes
            .keys()
            .filter(|key| !wanted_routes.contains(*key))
            .cloned()
            .collect();
        for key in stale {
            self.drop_route(&key)?;
        }
        wanted_sinks.extend(self.restore_streams(graph, &wanted_moves, errors));
        self.cleanup_sinks(&wanted_sinks, errors, 1)?;
        Ok(silence_blocked)
    }
    fn restore_streams(
        &mut self,
        graph: &GraphSnapshot,
        wanted: &HashMap<NodeIdentity, String>,
        errors: &mut Vec<OperationIssue>,
    ) -> HashSet<String> {
        let mut retained = HashSet::new();
        let moved: Vec<_> = self
            .moved
            .iter()
            .map(|(id, sink)| (id.clone(), sink.clone()))
            .collect();
        for (identity, original) in moved {
            if wanted.contains_key(&identity) {
                continue;
            }
            let Some(stream) = graph.streams.iter().find(|s| s.identity == identity) else {
                self.moved.remove(&identity);
                continue;
            };
            let Some(intake) = stream
                .sink
                .as_deref()
                .filter(|s| s.starts_with("openwave_src_"))
            else {
                self.moved.remove(&identity);
                continue;
            };
            // Never restore a stream out of another owner's replacement intake.
            let proven = self.sinks.get(intake).is_some_and(|handle| {
                graph.cookie == handle.cookie && graph.owned(intake, &handle.owner).is_some()
            });
            if !proven {
                errors.push(issue(intake, "Cannot restore from an unproven intake"));
                retained.insert(intake.into());
                continue;
            }
            let target = original
                .as_deref()
                .filter(|s| {
                    !s.starts_with("openwave_")
                        && graph.outputs.iter().filter(|o| o.node_name == *s).count() == 1
                })
                .map(str::to_owned)
                .or_else(|| {
                    match routing::resolve_output_with_wave(
                        "auto",
                        &graph
                            .outputs
                            .iter()
                            .cloned()
                            .map(|mut o| {
                                o.is_wave = false;
                                o
                            })
                            .collect::<Vec<_>>(),
                        graph.default_sink.as_deref(),
                        None,
                    ) {
                        Ok(OutputDecision::Monitor(output)) => Some(output.node_name.clone()),
                        _ => None,
                    }
                });
            let result = target
                .ok_or_else(|| unavailable("No eligible destination for released stream"))
                .and_then(|target| self.backend.move_stream(stream, &target));
            match result {
                Ok(()) => {
                    self.moved.remove(&identity);
                }
                Err(error) => {
                    retained.insert(intake.into());
                    errors.push(issue(format!("stream:{}", identity.object_serial), error));
                }
            }
        }
        retained
    }
    fn fresh_cleanup_graph(&mut self, attempts: usize) -> Result<GraphSnapshot> {
        let mut remaining = attempts;
        let graph = loop {
            match self.backend.snapshot() {
                Ok(graph) => break graph,
                Err(error) => {
                    self.pending_masters.clear();
                    self.discovery_error = Some(error.clone());
                    remaining -= 1;
                    if remaining == 0 {
                        return Err(error);
                    }
                    // Exiting captured helpers can disappear between pw-dump and pactl.
                    // Retry the entire observation, never weaken its identity checks.
                    thread::sleep(CLEANUP_SETTLE);
                }
            }
        };
        if self.cookie != Some(graph.cookie) {
            self.pending_masters.clear();
            let error = identity_error(
                "Server identity changed before cleanup; all destructive graph cleanup deferred",
            );
            self.discovery_error = Some(error.clone());
            return Err(error);
        }
        Ok(graph)
    }
    fn cleanup_sinks(
        &mut self,
        wanted: &HashSet<String>,
        errors: &mut Vec<OperationIssue>,
        attempts: usize,
    ) -> Result<()> {
        let removed: Vec<_> = self
            .removed
            .iter()
            .map(|(name, evidence)| (name.clone(), evidence.clone()))
            .collect();
        for (name, evidence) in removed {
            if wanted.contains(&name) {
                self.removed.remove(&name);
                continue;
            }
            let current = self.fresh_cleanup_graph(attempts)?;
            if let Some(node) = current.node(&name) {
                if node.identity != evidence.identity
                    || !definition_matches(node, &evidence.definition)
                {
                    errors.push(issue(
                        &name,
                        "Persistent sink identity changed; explicit removal refused",
                    ));
                    continue;
                }
                if current
                    .streams
                    .iter()
                    .any(|s| s.sink.as_deref() == Some(&name))
                {
                    errors.push(issue(&name, "Persistent sink still has streams"));
                    continue;
                }
                if let Err(error) = self.backend.destroy_node(node.id) {
                    errors.push(issue(&name, error));
                    continue;
                }
            } else if current.names.contains_key(&name) {
                errors.push(issue(&name, "Ambiguous sink removal refused"));
                continue;
            }
            self.removed.remove(&name);
            self.persistent.remove(&name);
        }
        let stale: Vec<_> = self
            .sinks
            .iter()
            .filter(|(name, _)| !wanted.contains(*name))
            .map(|(name, handle)| (name.clone(), handle.clone()))
            .collect();
        for (name, handle) in stale {
            let current = self.fresh_cleanup_graph(attempts)?;
            if let Some(node) = current.node(&name) {
                if current.cookie != handle.cookie
                    || current.owned(&name, &handle.owner).is_none()
                    || number(node.properties.get("pulse.module.id")) != Some(handle.module)
                {
                    errors.push(issue(&name, "Sink owner/module changed; unload refused"));
                    continue;
                }
                if current
                    .streams
                    .iter()
                    .any(|s| s.sink.as_deref() == Some(&name))
                {
                    errors.push(issue(
                        &name,
                        "Intake retained until all streams are restored",
                    ));
                    continue;
                }
                if let Err(error) = self.backend.unload_module(handle.module) {
                    errors.push(issue(&name, error));
                    continue;
                }
            } else if current.names.contains_key(&name) {
                errors.push(issue(&name, "Ambiguous owned sink; unload refused"));
                continue;
            }
            self.sinks.remove(&name);
        }
        Ok(())
    }
    fn mix_node<'a>(&self, graph: &'a GraphSnapshot, mix: &Mix) -> Option<&'a GraphNode> {
        let node = graph.node(&mix.sink)?;
        if !graph.sinks.contains_key(&mix.sink) {
            return None;
        }
        let owned = self.sinks.get(&mix.sink).is_some_and(|handle| {
            handle.cookie == graph.cookie
                && graph.owned(&mix.sink, &handle.owner).is_some()
                && number(node.properties.get("pulse.module.id")) == Some(handle.module)
        });
        let persistent = self.persistent.get(&mix.sink).is_some_and(|evidence| {
            evidence.identity == node.identity && definition_matches(node, &evidence.definition)
        });
        if owned || persistent {
            Some(node)
        } else {
            None
        }
    }
    fn meter_targets(
        &self,
        graph: &GraphSnapshot,
        errors: &mut Vec<OperationIssue>,
    ) -> Vec<MeterTarget> {
        let mut targets = Vec::new();
        let mut raw_identities = HashSet::new();
        for (id, source) in &self.desired.sources {
            if source.kind == SourceKind::Device {
                let captures: Vec<_> = graph
                    .captures
                    .iter()
                    .filter(|c| c.node_name == source.node_name)
                    .collect();
                if captures.len() == 1 {
                    let capture = captures[0];
                    match effects::capture_channels(capture.channels) {
                        Ok(channels) => {
                            targets.push(MeterTarget {
                                key: format!("src:{id}"),
                                node_name: capture.node_name.clone(),
                                identity: capture.identity.clone(),
                                raw: true,
                                channels,
                            });
                            raw_identities.insert(capture.identity.clone());
                        }
                        Err(error) => errors.push(issue(format!("src:{id}"), error)),
                    }
                }
            } else {
                let name = routing::source_sink_name(id);
                if let Some(handle) = self.sinks.get(&name) {
                    if let Some(node) = graph.owned(&name, &handle.owner) {
                        targets.push(MeterTarget {
                            key: format!("src:{id}"),
                            node_name: name,
                            identity: node.identity.clone(),
                            raw: false,
                            channels: 2,
                        });
                    }
                }
            }
        }
        // Readiness cannot wait for a matrix binding or the output route it gates.
        for capture in &graph.captures {
            if wave_name(&capture.node_name)
                && !raw_identities.contains(&capture.identity)
                && graph.node(&capture.node_name).is_some()
            {
                match effects::capture_channels(capture.channels) {
                    Ok(channels) => targets.push(MeterTarget {
                        key: format!("raw:{}", capture.identity.object_serial),
                        node_name: capture.node_name.clone(),
                        identity: capture.identity.clone(),
                        raw: true,
                        channels,
                    }),
                    Err(error) => errors.push(issue(&capture.node_name, error)),
                }
            }
        }
        for (id, mix) in &self.desired.mixes {
            if let Some(node) = self.mix_node(graph, mix) {
                targets.push(MeterTarget {
                    key: format!("mix:{id}"),
                    node_name: mix.sink.clone(),
                    identity: node.identity.clone(),
                    raw: false,
                    channels: 2,
                });
            }
        }
        targets.retain(|target| {
            let proven = graph.identity(&target.identity).is_some_and(|node|
                text(node.properties.get("object.serial")).as_deref() == Some(target.identity.object_serial.as_str()));
            if !proven { errors.push(issue(&target.key, "Meter unavailable: actual node object.serial missing; node ID cannot authorize a pw-cat target")); }
            proven
        });
        targets
    }
    fn teardown(&mut self) -> Result<()> {
        if self.cookie.is_none()
            && self.sinks.is_empty()
            && self.routes.is_empty()
            && self.effects.is_empty()
        {
            return Ok(());
        }
        let mut errors = Vec::new();
        if let Err(error) = self.stop_routes() {
            errors.push(issue("routes", error));
        }
        if let Err(error) = self.stop_effects() {
            errors.push(issue("effects", error));
        }
        if !self.routes.is_empty() || !self.effects.is_empty() {
            return Err(unavailable(
                errors
                    .iter()
                    .map(|e| format!("{}: {}", e.target, e.message))
                    .collect::<Vec<_>>()
                    .join("; "),
            ));
        }
        let mut retained = HashSet::new();
        let mut restoration_errors = Vec::new();
        for _ in 0..3 {
            let graph = match self.fresh_cleanup_graph(CLEANUP_ATTEMPTS) {
                Ok(graph) => graph,
                Err(error) => {
                    errors.push(issue("cleanup", error));
                    return Err(unavailable(
                        errors
                            .iter()
                            .map(|e| format!("{}: {}", e.target, e.message))
                            .collect::<Vec<_>>()
                            .join("; "),
                    ));
                }
            };
            restoration_errors.clear();
            retained = self.restore_streams(&graph, &HashMap::new(), &mut restoration_errors);
            if retained.is_empty() {
                break;
            }
        }
        errors.extend(restoration_errors);
        // Shutdown never removes persistent definitions, only proven runtime modules.
        self.removed.clear();
        if let Err(error) = self.cleanup_sinks(&retained, &mut errors, CLEANUP_ATTEMPTS) {
            errors.push(issue("cleanup", error));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(unavailable(
                errors
                    .iter()
                    .map(|e| format!("{}: {}", e.target, e.message))
                    .collect::<Vec<_>>()
                    .join("; "),
            ))
        }
    }
}

fn bool_prop(node: &GraphNode, key: &str, value: bool) -> bool {
    node.properties.get(key).is_some_and(|v| {
        v.as_bool() == Some(value) || v.as_str() == Some(if value { "true" } else { "false" })
    })
}
fn definition_matches(node: &GraphNode, mix: &Mix) -> bool {
    if node.name != mix.sink || node.properties.contains_key("pulse.module.id") {
        return false;
    }
    let marker = crate::setup::mix_definition_token(mix);
    if node
        .properties
        .get("openwave.mix-id")
        .and_then(Value::as_str)
        == Some(mix.id.as_str())
        && node
            .properties
            .get("openwave.definition")
            .and_then(Value::as_str)
            == Some(marker.as_str())
    {
        return true;
    }
    // Bounded upgrade evidence from the old generated definition, not a name sweep.
    node.properties.get("factory.name").and_then(Value::as_str) == Some("support.null-audio-sink")
        && node.properties.get("media.name").and_then(Value::as_str) == Some(mix.sink.as_str())
        && node
            .properties
            .get("node.description")
            .and_then(Value::as_str)
            == Some(mix.description.as_str())
        && node.properties.get("media.class").and_then(Value::as_str) == Some("Audio/Sink")
        && node.properties.get("audio.position") == Some(&json!(["FL", "FR"]))
        && bool_prop(node, "object.linger", true)
        && bool_prop(node, "monitor.channel-volumes", true)
        && bool_prop(node, "state.restore-props", false)
        && number(node.properties.get("priority.session")) == Some(0)
}
fn wave_ready(
    output: &OutputSnapshot,
    captures: &[CaptureSnapshot],
    readiness: &CaptureReadiness,
) -> bool {
    fn stem<'a>(name: &'a str, prefix: &str) -> Option<&'a str> {
        name.strip_prefix(prefix)
            .and_then(|s| s.rsplit_once('.').map(|(stem, _)| stem))
    }
    let Some(output_stem) = stem(&output.node_name, "alsa_output.") else {
        return false;
    };
    let mut matching = captures
        .iter()
        .filter(|capture| stem(&capture.node_name, "alsa_input.") == Some(output_stem));
    let Some(capture) = matching.next() else {
        return false;
    };
    matching.next().is_none() && readiness.ready(&capture.identity)
}

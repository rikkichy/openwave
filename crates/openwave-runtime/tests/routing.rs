use indexmap::IndexMap;
use openwave_core::{effects::FxSettings, model::*, routing};
use openwave_runtime::{
    meter::CaptureReadiness,
    mixer::{CaptureBinding, GraphBackend, GraphSnapshot, Mixer, MixerEvent, RoutingChild},
};
use serde_json::{Map, Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

fn failed() -> OperationError {
    OperationError::new(ErrorCode::Unavailable, "injected graph failure")
}
fn sid(s: &str) -> SourceId {
    SourceId::new(s).unwrap()
}
fn mid(s: &str) -> MixId {
    MixId::new(s).unwrap()
}
fn props(value: Value) -> Map<String, Value> {
    value.as_object().unwrap().clone()
}

#[derive(Default)]
struct World {
    cookie: u32,
    next: u32,
    objects: HashMap<u32, Value>,
    sinks: HashMap<String, Value>,
    sources: HashMap<String, Value>,
    inputs: HashMap<u32, Value>,
    links: HashSet<(u32, u32)>,
    levels: HashMap<u32, (f64, bool)>,
    modules: HashMap<u32, u32>,
    children: HashMap<u32, Vec<u32>>,
    definitions: Mixes,
    fail_snapshot: bool,
    fail_write: bool,
    fail_links: usize,
    fail_moves: usize,
    fail_levels: usize,
    fail_mutes: usize,
    fail_silence: HashSet<String>,
    fail_terminate: HashSet<String>,
    fail_filter: bool,
    writes: usize,
    mutations: usize,
    unloads: Vec<u32>,
    filter_channels: Vec<u32>,
    restart_on_snapshot: Option<usize>,
    snapshots: usize,
    restart_on_terminate: bool,
    fail_snapshots: usize,
    fail_snapshots_on_terminate: usize,
    panic_snapshot: bool,
    mute_gate: Option<(String, mpsc::Sender<()>, mpsc::Receiver<()>)>,
}
impl World {
    fn new() -> Self {
        let mut world = Self {
            cookie: 71,
            next: 10,
            ..Self::default()
        };
        world.sink("headphones", None, None);
        world
    }
    fn id(&mut self) -> u32 {
        let id = self.next;
        self.next += 3;
        id
    }
    fn node(&mut self, name: &str, mut properties: Map<String, Value>) -> u32 {
        let id = self.id();
        properties.insert("node.name".into(), json!(name));
        properties.insert("object.serial".into(), json!(id + 10000));
        self.objects.insert(
            id,
            json!({"id":id,"type":"PipeWire:Interface:Node","info":{"props":properties}}),
        );
        for (offset, direction) in [(1, "in"), (2, "out")] {
            self.objects.insert(id + offset, json!({"id":id + offset,"type":"PipeWire:Interface:Port","info":{"props":{"node.id":id,"port.direction":direction,"audio.channel":"MONO"}}}));
        }
        self.levels.insert(id, (1.0, false));
        id
    }
    fn node_id(&self, name: &str) -> Option<u32> {
        let mut nodes = self.objects.iter().filter(|(_, node)| {
            node["type"] == "PipeWire:Interface:Node" && node["info"]["props"]["node.name"] == name
        });
        let id = nodes.next().map(|(id, _)| *id);
        if nodes.next().is_some() { None } else { id }
    }
    fn name(&self, id: u32) -> String {
        self.objects
            .get(&id)
            .and_then(|o| o["info"]["props"]["node.name"].as_str())
            .unwrap_or("")
            .into()
    }
    fn identity(&self, name: &str) -> NodeIdentity {
        let id = self.node_id(name).unwrap();
        NodeIdentity {
            server_cookie: self.cookie,
            object_serial: (id + 10000).to_string(),
        }
    }
    fn sink(&mut self, name: &str, owned: Option<(u32, &str)>, definition: Option<&Mix>) -> u32 {
        if let Some(id) = self.node_id(name) {
            self.remove_node(id);
        }
        let mut properties = props(json!({"media.class":"Audio/Sink","priority.session":0}));
        if let Some((module, token)) = owned {
            properties.insert("pulse.module.id".into(), json!(module));
            properties.insert("openwave.owner".into(), json!(token));
        }
        if let Some(mix) = definition {
            properties.insert("openwave.mix-id".into(), json!(mix.id.as_str()));
            properties.insert(
                "openwave.definition".into(),
                json!(openwave_runtime::setup::mix_definition_token(mix)),
            );
        }
        let id = self.node(name, properties);
        self.sinks.insert(name.into(), json!({"name":name,"index":id,"description":name,"mute":false,"volume":{"mono":{"value":65536}},"properties":{"object.serial":id+10000}}));
        if let Some((module, _)) = owned {
            self.modules.insert(module, id);
        }
        id
    }
    fn capture(&mut self, name: &str, channels: u32) -> u32 {
        if let Some(id) = self.node_id(name) {
            self.remove_node(id);
        }
        let id = self.node(
            name,
            props(json!({"media.class":"Audio/Source","audio.channels":channels})),
        );
        self.sources.insert(
            name.into(),
            json!({"name":name,"mute":false,"properties":{"object.serial":id+10000}}),
        );
        id
    }
    fn stream(&mut self, name: &str, application: &str, sink: &str) -> u32 {
        let id = self.node(name, props(json!({"media.class":"Stream/Output/Audio","application.name":application,"application.process.binary":"/usr/bin/player"})));
        self.inputs.insert(id, json!({"index":id,"sink":self.sinks[sink]["index"],"properties":{"object.serial":id+10000}}));
        id
    }
    fn destination(&self, stream: u32) -> Option<&str> {
        let index = self.inputs.get(&stream)?.get("sink")?;
        self.sinks
            .iter()
            .find(|(_, sink)| &sink["index"] == index)
            .map(|(name, _)| name.as_str())
    }
    fn remove_node(&mut self, id: u32) {
        let name = self.name(id);
        self.objects.remove(&id);
        self.objects.remove(&(id + 1));
        self.objects.remove(&(id + 2));
        self.links
            .retain(|(a, b)| ![id + 1, id + 2].contains(a) && ![id + 1, id + 2].contains(b));
        self.sinks.remove(&name);
        self.sources.remove(&name);
        self.levels.remove(&id);
    }
    fn route(&self, source: &str, target: &str, expected: Option<(f64, bool)>) -> bool {
        let (Some(source), Some(target)) = (self.node_id(source), self.node_id(target)) else {
            return false;
        };
        self.children.values().any(|ids| {
            ids.len() == 2
                && self.objects.contains_key(&ids[0])
                && self.objects.contains_key(&ids[1])
                && self.links.contains(&(source + 2, ids[1] + 1))
                && self.links.contains(&(ids[0] + 2, target + 1))
                && expected.is_none_or(|level| {
                    self.levels
                        .get(&ids[0])
                        .is_some_and(|v| (v.0 - level.0).abs() < 1e-8 && v.1 == level.1)
                })
        })
    }
    fn snapshot(&self) -> Result<GraphSnapshot> {
        let mut objects: Vec<_> = self.objects.values().cloned().collect();
        objects
            .push(json!({"id":0,"type":"PipeWire:Interface:Core","info":{"cookie":self.cookie}}));
        for (i, (output, input)) in self.links.iter().enumerate() {
            objects.push(json!({"id":1000000+i,"type":"PipeWire:Interface:Link","info":{"output-port-id":output,"input-port-id":input}}));
        }
        GraphSnapshot::parse(
            &json!(objects),
            &json!(self.sinks.values().collect::<Vec<_>>()),
            &json!(self.sources.values().collect::<Vec<_>>()),
            &json!(self.inputs.values().collect::<Vec<_>>()),
            &json!([]),
            Some("headphones".into()),
        )
    }
}
struct FakeChild {
    world: Arc<Mutex<World>>,
    id: u32,
}
impl RoutingChild for FakeChild {
    fn running(&mut self) -> Result<bool> {
        Ok(self
            .world
            .lock()
            .expect("routing fixture lock poisoned")
            .children
            .contains_key(&self.id))
    }
    fn terminate(&mut self) -> Result<()> {
        let mut world = self.world.lock().expect("routing fixture lock poisoned");
        if world.restart_on_terminate {
            world.cookie += 1;
            world.restart_on_terminate = false;
        }
        if let Some(ids) = world.children.get(&self.id) {
            if ids
                .iter()
                .any(|id| world.fail_terminate.contains(&world.name(*id)))
            {
                return Err(failed());
            }
        }
        if let Some(ids) = world.children.remove(&self.id) {
            for id in ids {
                world.remove_node(id);
            }
        }
        if world.fail_snapshots_on_terminate > 0 {
            world.fail_snapshots = std::mem::take(&mut world.fail_snapshots_on_terminate);
        }
        Ok(())
    }
}
struct FakeBackend(Arc<Mutex<World>>);
impl GraphBackend for FakeBackend {
    fn snapshot(&mut self) -> Result<GraphSnapshot> {
        let mut w = self.0.lock().expect("routing fixture lock poisoned");
        w.snapshots += 1;
        if w.restart_on_snapshot == Some(w.snapshots) {
            w.cookie += 1;
        }
        if std::mem::take(&mut w.panic_snapshot) {
            drop(w);
            panic!("injected mixer worker panic");
        }
        if w.fail_snapshots > 0 {
            w.fail_snapshots -= 1;
            return Err(failed());
        }
        if w.fail_snapshot {
            Err(failed())
        } else {
            w.snapshot()
        }
    }
    fn write_definitions(&mut self, mixes: &Mixes) -> Result<()> {
        let mut w = self.0.lock().expect("routing fixture lock poisoned");
        if w.fail_write {
            return Err(failed());
        }
        w.definitions = mixes.clone();
        w.writes += 1;
        Ok(())
    }
    fn create_sink(&mut self, name: &str, _: &str, owner: &str) -> Result<u32> {
        let mut w = self.0.lock().expect("routing fixture lock poisoned");
        let module = w.id();
        w.sink(name, Some((module, owner)), None);
        w.mutations += 1;
        Ok(module)
    }
    fn unload_module(&mut self, module: u32) -> Result<()> {
        let mut w = self.0.lock().expect("routing fixture lock poisoned");
        w.unloads.push(module);
        w.mutations += 1;
        if let Some(id) = w.modules.remove(&module) {
            w.remove_node(id);
        }
        Ok(())
    }
    fn destroy_node(&mut self, node: u32) -> Result<()> {
        let mut w = self.0.lock().expect("routing fixture lock poisoned");
        w.remove_node(node);
        w.mutations += 1;
        Ok(())
    }
    fn move_stream(&mut self, stream: &StreamSnapshot, sink: &str) -> Result<()> {
        let mut w = self.0.lock().expect("routing fixture lock poisoned");
        if w.fail_moves > 0 {
            w.fail_moves -= 1;
            return Err(failed());
        }
        let index = stream.pulse_index.ok_or_else(failed)?;
        let sink_index = w.sinks.get(sink).ok_or_else(failed)?["index"].clone();
        w.inputs.get_mut(&index).ok_or_else(failed)?["sink"] = sink_index;
        w.mutations += 1;
        Ok(())
    }
    fn link(&mut self, source: u32, target: u32) -> Result<()> {
        let mut w = self.0.lock().expect("routing fixture lock poisoned");
        if w.fail_links > 0 {
            w.fail_links -= 1;
            return Err(failed());
        }
        w.links.insert((source, target));
        w.mutations += 1;
        Ok(())
    }
    fn set_level(&mut self, node: u32, level: f64, muted: bool) -> Result<()> {
        let mut w = self.0.lock().expect("routing fixture lock poisoned");
        if w.fail_levels > 0 {
            w.fail_levels -= 1;
            return Err(failed());
        }
        if muted && w.fail_silence.contains(&w.name(node)) {
            return Err(failed());
        }
        w.levels.insert(node, (level, muted));
        for sink in w.sinks.values_mut() {
            if sink["index"] == node {
                sink["mute"] = json!(muted);
                sink["volume"]["mono"]["value"] = json!((level * 65536.0).round() as u32);
            }
        }
        w.mutations += 1;
        Ok(())
    }
    fn set_capture_mute(&mut self, node: &CaptureSnapshot, muted: bool) -> Result<()> {
        let mut w = self.0.lock().expect("routing fixture lock poisoned");
        if w.fail_mutes > 0 {
            w.fail_mutes -= 1;
            return Err(failed());
        }
        w.sources.get_mut(&node.node_name).ok_or_else(failed)?["mute"] = json!(muted);
        w.mutations += 1;
        let gate = if w
            .mute_gate
            .as_ref()
            .is_some_and(|(name, _, _)| name == &node.node_name)
        {
            w.mute_gate.take()
        } else {
            None
        };
        drop(w);
        if let Some((_, arrived, release)) = gate {
            arrived.send(()).map_err(|_| failed())?;
            release
                .recv_timeout(Duration::from_secs(3))
                .map_err(|_| failed())?;
        }
        Ok(())
    }
    fn spawn_loopback(
        &mut self,
        name: &str,
        owner: &str,
        description: Option<&str>,
    ) -> Result<Box<dyn RoutingChild>> {
        let mut w = self.0.lock().expect("routing fixture lock poisoned");
        let playback = w.node(name, props(json!({"openwave.owner":owner,"media.class":if description.is_some() {"Audio/Source"} else {"Stream/Output/Audio"}})));
        let capture = w.node(
            &format!("{name}_cap"),
            props(json!({"openwave.owner":owner,"media.class":"Stream/Input/Audio"})),
        );
        w.children.insert(playback, vec![playback, capture]);
        w.mutations += 1;
        Ok(Box::new(FakeChild {
            world: self.0.clone(),
            id: playback,
        }))
    }
    fn spawn_filter(&mut self, path: &Path) -> Result<Box<dyn RoutingChild>> {
        let config: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let args = &config["context.modules"][4]["args"];
        let mut w = self.0.lock().expect("routing fixture lock poisoned");
        if w.fail_filter {
            return Err(failed());
        }
        w.filter_channels
            .push(args["capture.props"]["audio.channels"].as_u64().unwrap() as u32);
        let output = &args["playback.props"];
        let input = &args["capture.props"];
        let playback = w.node(output["node.name"].as_str().unwrap(), props(output.clone()));
        let capture = w.node(input["node.name"].as_str().unwrap(), props(input.clone()));
        w.children.insert(playback, vec![playback, capture]);
        w.mutations += 1;
        Ok(Box::new(FakeChild {
            world: self.0.clone(),
            id: playback,
        }))
    }
}
struct Fixture {
    world: Arc<Mutex<World>>,
    mixer: Mixer,
    events: mpsc::Receiver<MixerEvent>,
    desired: DesiredState,
    revision: u64,
    bindings: IndexMap<String, CaptureBinding>,
}
impl Fixture {
    fn new() -> Self {
        Self::with_readiness(CaptureReadiness::default())
    }
    fn with_readiness(readiness: CaptureReadiness) -> Self {
        let world = Arc::new(Mutex::new(World::new()));
        let (mixer, events) = Mixer::start_with_backend(
            Box::new(FakeBackend(world.clone())),
            readiness,
            Duration::from_millis(10),
        )
        .unwrap();
        Self {
            world,
            mixer,
            events,
            desired: DesiredState {
                mixes: default_mixes(),
                ..DesiredState::default()
            },
            revision: 0,
            bindings: IndexMap::new(),
        }
    }
    fn apply(&mut self) {
        self.revision += 1;
        self.mixer
            .set_desired(
                self.revision,
                Arc::new(self.desired.clone()),
                self.bindings.clone(),
            )
            .unwrap();
    }
    fn app(&mut self, id: &str, binding: &str) {
        let mut source = Source::new(id.into(), SourceKind::App);
        source.id = sid(id);
        source.match_app_names = vec![binding.into()];
        self.desired.sources.insert(source.id.clone(), source);
    }
    fn device(&mut self, id: &str, name: &str, epoch: u64) {
        let mut source = Source::new(id.into(), SourceKind::Device);
        source.id = sid(id);
        source.node_name = name.into();
        self.desired.sources.insert(source.id.clone(), source);
        self.bindings.insert(
            name.into(),
            CaptureBinding {
                epoch,
                owners: vec![sid(id)],
            },
        );
    }
    fn cell(&mut self, source: &str, mix: &str, level: f64) {
        self.desired.matrix.cells.insert(
            format!("{source}.{mix}"),
            LevelState {
                volume: level,
                muted: false,
                extra: Map::new(),
            },
        );
    }
    fn until(&self, predicate: impl Fn(&World) -> bool) {
        let start = Instant::now();
        loop {
            if predicate(&self.world.lock().expect("routing fixture lock poisoned")) {
                return;
            }
            assert!(
                start.elapsed() < Duration::from_secs(4),
                "routing did not converge"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }
    fn observation(
        &self,
        predicate: impl Fn(&openwave_runtime::mixer::MixerObservation) -> bool,
    ) -> openwave_runtime::mixer::MixerObservation {
        let start = Instant::now();
        loop {
            if let MixerEvent::Observed(observation) =
                self.events.recv_timeout(Duration::from_secs(4)).unwrap()
            {
                if predicate(&observation) {
                    return observation;
                }
            }
            assert!(
                start.elapsed() < Duration::from_secs(4),
                "observation did not arrive"
            );
        }
    }
    fn cycles(&self, count: usize) {
        let start = self
            .world
            .lock()
            .expect("routing fixture lock poisoned")
            .snapshots;
        self.until(|world| world.snapshots >= start + count);
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        {
            let mut w = self.world.lock().expect("routing fixture lock poisoned");
            w.fail_snapshot = false;
            w.fail_moves = 0;
            w.fail_levels = 0;
            w.fail_silence.clear();
            w.fail_terminate.clear();
            w.restart_on_snapshot = None;
        }
        let _ = self.mixer.stop();
    }
}

#[test]
fn pulse_readback_preserves_normalized_volume_and_loudest_channel() {
    let mut world = World::new();
    world.sinks.get_mut("headphones").unwrap()["volume"] =
        json!({"front-left":{"value":16384},"front-right":{"value":32768}});
    assert_eq!(world.snapshot().unwrap().sinks["headphones"].level, 0.5);
    world.sinks.get_mut("headphones").unwrap()["volume"]["front-right"]["value"] = json!(8192);
    assert_eq!(world.snapshot().unwrap().sinks["headphones"].level, 0.25);
}

#[test]
fn duplicate_names_claim_by_serial_and_retry_moves_links_levels_without_copying() {
    let mut f = Fixture::new();
    let (first, second) = {
        let mut w = f.world.lock().expect("routing fixture lock poisoned");
        let first = w.stream("Player", " Music ", "headphones");
        let second = w.stream("Player", "Music", "headphones");
        w.fail_moves = 1;
        w.fail_links = 2;
        w.fail_levels = 1;
        (first, second)
    };
    f.app("z", "Music");
    f.app("b", "music");
    f.app("a", "player");
    f.desired.sources.get_mut(&sid("b")).unwrap().level = 0.5;
    f.cell("b", "chat", 0.6);
    f.apply();
    f.until(|w| {
        w.destination(first) == Some("openwave_src_b")
            && w.destination(second) == Some("openwave_src_b")
            && w.route("openwave_src_b", "openwave_chat_mix", Some((0.3, false)))
    });
    assert!(
        !f.world
            .lock()
            .unwrap()
            .route("openwave_src_a", "openwave_chat_mix", None)
    );
    f.cell("b", "chat", 0.0);
    f.apply();
    f.until(|w| !w.route("openwave_src_b", "openwave_chat_mix", None));
    assert_eq!(
        f.world.lock().unwrap().destination(first),
        Some("openwave_src_b")
    );
    f.mixer.stop().unwrap();
    let w = f.world.lock().expect("routing fixture lock poisoned");
    assert_eq!(w.destination(first), Some("headphones"));
    assert_eq!(w.destination(second), Some("headphones"));
    assert!(w.children.is_empty());
}

#[test]
fn missing_link_is_repaired_without_replacing_route_or_publication() {
    let mut f = Fixture::new();
    f.world
        .lock()
        .expect("routing fixture lock poisoned")
        .stream("Music", "Music", "headphones");
    f.app("music", "Music");
    f.cell("music", "personal", 0.4);
    f.apply();
    f.until(|w| {
        w.route(
            "openwave_src_music",
            "openwave_personal_mix",
            Some((0.4, false)),
        ) && w.node_id("openwave_capture_personal").is_some()
    });
    let (route, publication) = {
        let mut w = f.world.lock().expect("routing fixture lock poisoned");
        let route = w.node_id("openwave_loop_5_music_personal").unwrap();
        let publication = w.node_id("openwave_capture_personal").unwrap();
        w.links.clear();
        (route, publication)
    };
    f.until(|w| {
        w.route(
            "openwave_src_music",
            "openwave_personal_mix",
            Some((0.4, false)),
        )
    });
    let w = f.world.lock().expect("routing fixture lock poisoned");
    assert_eq!(w.node_id("openwave_loop_5_music_personal"), Some(route));
    assert_eq!(w.node_id("openwave_capture_personal"), Some(publication));
}

#[test]
fn failed_silencing_defers_exclusive_handover() {
    let mut f = Fixture::new();
    {
        let mut w = f.world.lock().expect("routing fixture lock poisoned");
        w.capture("mic_a", 1);
        w.capture("mic_b", 1);
    }
    f.device("a", "mic_a", 1);
    f.device("b", "mic_b", 2);
    f.desired.sources.get_mut(&sid("a")).unwrap().group = "Mics".into();
    f.desired.sources.get_mut(&sid("b")).unwrap().group = "Mics".into();
    f.desired.sources.get_mut(&sid("b")).unwrap().muted = true;
    f.cell("a", "chat", 0.8);
    f.cell("b", "chat", 0.8);
    f.apply();
    f.until(|w| {
        w.route("mic_a", "openwave_chat_mix", Some((0.8, false)))
            && w.route("mic_b", "openwave_chat_mix", Some((0.8, true)))
    });
    f.world
        .lock()
        .expect("routing fixture lock poisoned")
        .fail_silence
        .insert("openwave_loop_1_a_chat".into());
    f.desired.sources.get_mut(&sid("a")).unwrap().muted = true;
    f.desired.sources.get_mut(&sid("b")).unwrap().muted = false;
    f.apply();
    f.observation(|o| o.errors.iter().any(|e| e.message.contains("handover")));
    {
        let w = f.world.lock().expect("routing fixture lock poisoned");
        assert!(w.route("mic_b", "openwave_chat_mix", Some((0.8, true))));
        assert!(!w.route("mic_b", "openwave_chat_mix", Some((0.8, false))));
    }
    f.world
        .lock()
        .expect("routing fixture lock poisoned")
        .fail_silence
        .clear();
    f.until(|w| {
        w.route("mic_a", "openwave_chat_mix", Some((0.8, true)))
            && w.route("mic_b", "openwave_chat_mix", Some((0.8, false)))
    });
}

#[test]
fn shared_capture_row_silence_is_acknowledged_only_after_owned_routes_are_muted() {
    let mut f = Fixture::new();
    f.world.lock().unwrap().capture("shared_mic", 1);
    f.device("a", "shared_mic", 1);
    f.device("c", "shared_mic", 1);
    f.bindings.get_mut("shared_mic").unwrap().owners = vec![sid("a"), sid("c")];
    f.desired.sources.get_mut(&sid("a")).unwrap().group = "Mics".into();
    f.desired.sources.get_mut(&sid("c")).unwrap().group = "Other".into();
    f.cell("a", "chat", 0.7);
    f.cell("c", "chat", 0.5);
    let a_route = routing::cell_route_name(&sid("a"), &mid("chat"));
    let c_route = routing::cell_route_name(&sid("c"), &mid("chat"));
    f.apply();
    f.until(|w| {
        w.node_id(&a_route)
            .is_some_and(|id| w.levels[&id] == (0.7, false))
            && w.node_id(&c_route)
                .is_some_and(|id| w.levels[&id] == (0.5, false))
    });
    f.world.lock().unwrap().fail_silence.insert(a_route.clone());
    f.desired.sources.get_mut(&sid("a")).unwrap().muted = true;
    f.apply();
    let failed = f.observation(|o| o.revision == f.revision && !o.errors.is_empty());
    assert!(!failed.silent_sources.contains_key(&sid("a")));
    {
        let w = f.world.lock().unwrap();
        assert_eq!(w.levels[&w.node_id(&a_route).unwrap()], (0.7, false));
        assert_eq!(w.levels[&w.node_id(&c_route).unwrap()], (0.5, false));
        assert_eq!(w.sources["shared_mic"]["mute"], false);
    }
    f.world.lock().unwrap().fail_silence.clear();
    let silent =
        f.observation(|o| o.revision == f.revision && o.silent_sources.contains_key(&sid("a")));
    {
        let w = f.world.lock().unwrap();
        assert_eq!(silent.silent_sources[&sid("a")], w.identity("shared_mic"));
        assert!(!silent.silent_sources.contains_key(&sid("c")));
        assert_eq!(w.levels[&w.node_id(&a_route).unwrap()], (0.7, true));
        assert_eq!(w.levels[&w.node_id(&c_route).unwrap()], (0.5, false));
        assert_eq!(w.sources["shared_mic"]["mute"], false);
    }
    f.world.lock().unwrap().fail_snapshot = true;
    let unknown = f.observation(|o| o.observation.known().is_none());
    assert!(
        unknown.silent_sources.is_empty(),
        "unknown graph retained silence authority"
    );
}

#[test]
fn failed_termination_of_zero_send_also_blocks_new_opening() {
    let mut f = Fixture::new();
    {
        let mut w = f.world.lock().expect("routing fixture lock poisoned");
        w.capture("mic_a", 1);
        w.capture("mic_b", 1);
    }
    f.device("a", "mic_a", 1);
    f.device("b", "mic_b", 2);
    f.cell("a", "chat", 0.8);
    f.apply();
    f.until(|w| w.route("mic_a", "openwave_chat_mix", Some((0.8, false))));
    f.world
        .lock()
        .expect("routing fixture lock poisoned")
        .fail_terminate
        .insert("openwave_loop_1_a_chat".into());
    f.cell("a", "chat", 0.0);
    f.cell("b", "chat", 0.1);
    f.apply();
    f.observation(|o| !o.errors.is_empty());
    assert!(
        !f.world
            .lock()
            .unwrap()
            .route("mic_b", "openwave_chat_mix", Some((0.1, false)))
    );
    f.world
        .lock()
        .expect("routing fixture lock poisoned")
        .fail_terminate
        .clear();
    f.until(|w| {
        !w.route("mic_a", "openwave_chat_mix", None)
            && w.route("mic_b", "openwave_chat_mix", Some((0.1, false)))
    });
}

#[test]
fn recreated_master_restores_then_confirms_without_publishing_unity() {
    let mut f = Fixture::new();
    f.desired.matrix.volumes.insert(
        mid("chat"),
        LevelState {
            volume: 0.25,
            muted: true,
            extra: Map::new(),
        },
    );
    f.apply();
    f.until(|w| w.node_id("openwave_capture_chat").is_some());
    let mix = f.desired.mixes[&mid("chat")].clone();
    {
        let mut w = f.world.lock().expect("routing fixture lock poisoned");
        let id = w.node_id(&mix.sink).unwrap();
        let props = w.objects[&id]["info"]["props"].as_object().unwrap().clone();
        let module = props["pulse.module.id"].as_u64().unwrap() as u32;
        let owner = props["openwave.owner"].as_str().unwrap().to_owned();
        w.sink(&mix.sink, Some((module, &owner)), None);
        w.fail_levels = 100;
    }
    f.observation(|o| o.errors.iter().any(|e| e.target == "master:chat"));
    f.until(|w| w.node_id("openwave_capture_chat").is_none());
    f.world
        .lock()
        .expect("routing fixture lock poisoned")
        .fail_levels = 0;
    f.until(|w| {
        w.node_id(&mix.sink)
            .is_some_and(|id| w.levels.get(&id) == Some(&(0.25, true)))
            && w.node_id("openwave_capture_chat").is_some()
    });
    let mut observed = false;
    while let Ok(event) = f.events.try_recv() {
        if let MixerEvent::MasterObserved {
            mix, level, muted, ..
        } = event
        {
            if mix == mid("chat") {
                assert!((level - 0.25).abs() < 0.001);
                assert!(muted);
                observed = true;
            }
        }
    }
    assert!(observed);
}

#[test]
fn healthy_master_edit_keeps_publication_identity_but_failure_never_claims_restore() {
    let mut f = Fixture::new();
    f.apply();
    f.until(|w| {
        w.route("openwave_personal_mix", "headphones", None)
            && w.node_id("openwave_capture_personal").is_some()
    });
    let publication = f
        .world
        .lock()
        .expect("routing fixture lock poisoned")
        .node_id("openwave_capture_personal")
        .unwrap();
    f.desired.matrix.volumes.insert(
        mid("personal"),
        LevelState {
            volume: 0.4,
            muted: true,
            extra: Map::new(),
        },
    );
    f.apply();
    f.until(|w| {
        w.node_id("openwave_personal_mix")
            .is_some_and(|id| w.levels.get(&id) == Some(&(0.4, true)))
    });
    f.cycles(2);
    assert_eq!(
        f.world.lock().unwrap().node_id("openwave_capture_personal"),
        Some(publication)
    );
    f.world
        .lock()
        .expect("routing fixture lock poisoned")
        .fail_levels = 100;
    f.desired
        .matrix
        .volumes
        .get_mut(&mid("personal"))
        .unwrap()
        .volume = 0.2;
    f.apply();
    f.observation(|o| o.errors.iter().any(|e| e.target == "master:personal"));
    f.until(|w| w.node_id("openwave_capture_personal").is_none());
    f.world
        .lock()
        .expect("routing fixture lock poisoned")
        .fail_levels = 0;
    f.until(|w| {
        w.node_id("openwave_capture_personal").is_some()
            && w.node_id("openwave_personal_mix")
                .is_some_and(|id| w.levels.get(&id) == Some(&(0.2, true)))
    });
}

#[test]
fn restoration_failure_retains_intake_and_uses_original_not_default() {
    let mut f = Fixture::new();
    let stream = {
        let mut w = f.world.lock().expect("routing fixture lock poisoned");
        w.sink("speakers", None, None);
        w.stream("Music", "Music", "speakers")
    };
    f.app("music", "Music");
    f.apply();
    f.until(|w| w.destination(stream) == Some("openwave_src_music"));
    f.world
        .lock()
        .expect("routing fixture lock poisoned")
        .fail_moves = 100;
    f.desired.sources.clear();
    f.apply();
    f.observation(|o| o.errors.iter().any(|e| e.target.starts_with("stream:")));
    {
        let w = f.world.lock().expect("routing fixture lock poisoned");
        assert_eq!(w.destination(stream), Some("openwave_src_music"));
        assert!(w.sinks.contains_key("openwave_src_music"));
    }
    f.world
        .lock()
        .expect("routing fixture lock poisoned")
        .fail_moves = 0;
    f.until(|w| {
        w.destination(stream) == Some("speakers") && !w.sinks.contains_key("openwave_src_music")
    });
}

#[test]
fn teardown_retries_and_preserves_intake_on_persistent_restoration_failure() {
    for failures in [1, 100] {
        let mut f = Fixture::new();
        let stream = f
            .world
            .lock()
            .expect("routing fixture lock poisoned")
            .stream("Music", "Music", "headphones");
        f.app("music", "Music");
        f.apply();
        f.until(|w| w.destination(stream) == Some("openwave_src_music"));
        f.world
            .lock()
            .expect("routing fixture lock poisoned")
            .fail_moves = failures;
        let result = f.mixer.stop();
        let w = f.world.lock().expect("routing fixture lock poisoned");
        assert!(w.children.is_empty());
        if failures == 1 {
            assert!(result.is_ok());
            assert_eq!(w.destination(stream), Some("headphones"));
            assert!(!w.sinks.contains_key("openwave_src_music"));
        } else {
            assert!(result.is_err());
            assert_eq!(w.destination(stream), Some("openwave_src_music"));
            assert!(w.sinks.contains_key("openwave_src_music"));
        }
        drop(w);
        if failures != 1 {
            f.world
                .lock()
                .expect("routing fixture lock poisoned")
                .fail_moves = 0;
            f.mixer.stop().unwrap();
            let w = f.world.lock().expect("routing fixture lock poisoned");
            assert_eq!(w.destination(stream), Some("headphones"));
            assert!(!w.sinks.contains_key("openwave_src_music"));
        }
    }
}

#[test]
fn shutdown_waits_for_owned_helper_graph_to_settle() {
    let mut f = Fixture::new();
    let stream = f
        .world
        .lock()
        .unwrap()
        .stream("Music", "Music", "headphones");
    f.app("music", "Music");
    f.apply();
    f.until(|w| {
        w.destination(stream) == Some("openwave_src_music")
            && w.route("openwave_personal_mix", "headphones", None)
    });
    f.world.lock().unwrap().fail_snapshots_on_terminate = 2;
    f.mixer.stop().unwrap();
    let w = f.world.lock().unwrap();
    assert_eq!(w.destination(stream), Some("headphones"));
    assert!(w.children.is_empty());
    assert!(w.modules.is_empty());
}

#[test]
fn unknown_shutdown_retains_ownership_for_explicit_retry() {
    let mut f = Fixture::new();
    let stream = f
        .world
        .lock()
        .unwrap()
        .stream("Music", "Music", "headphones");
    f.app("music", "Music");
    f.apply();
    f.until(|w| {
        w.destination(stream) == Some("openwave_src_music")
            && w.route("openwave_personal_mix", "headphones", None)
    });
    f.world.lock().unwrap().fail_snapshot = true;
    assert!(f.mixer.stop().is_err());
    let modules = {
        let w = f.world.lock().unwrap();
        assert!(w.children.is_empty());
        assert_eq!(w.destination(stream), Some("openwave_src_music"));
        assert!(w.unloads.is_empty());
        w.modules.clone()
    };
    assert!(f.mixer.stop().is_err());
    assert!(
        f.mixer
            .set_desired(100, Arc::new(f.desired.clone()), IndexMap::new())
            .is_err()
    );
    let (foreign_module, foreign_node) = {
        let mut w = f.world.lock().unwrap();
        assert_eq!(w.modules, modules);
        w.fail_snapshot = false;
        let module = w.id();
        let node = w.sink("unrelated", Some((module, "foreign")), None);
        (module, node)
    };
    f.mixer.stop().unwrap();
    let snapshots = {
        let w = f.world.lock().unwrap();
        assert_eq!(w.destination(stream), Some("headphones"));
        assert_eq!(w.modules, HashMap::from([(foreign_module, foreign_node)]));
        assert_eq!(w.node_id("unrelated"), Some(foreign_node));
        assert!(modules.keys().all(|module| w.unloads.contains(module)));
        w.snapshots
    };
    f.mixer.stop().unwrap();
    assert_eq!(f.world.lock().unwrap().snapshots, snapshots);
}

#[test]
fn worker_panic_remains_failed_after_retained_cleanup_succeeds() {
    let mut f = Fixture::new();
    let stream = f
        .world
        .lock()
        .unwrap()
        .stream("Music", "Music", "headphones");
    f.app("music", "Music");
    f.apply();
    f.until(|w| w.destination(stream) == Some("openwave_src_music"));
    f.world.lock().unwrap().panic_snapshot = true;
    assert!(f.mixer.stop().is_err());
    assert!(f.mixer.stop().is_err());
    {
        let w = f.world.lock().unwrap();
        assert_eq!(w.destination(stream), Some("headphones"));
        assert!(w.children.is_empty());
        assert!(w.modules.is_empty());
    }
    assert!(f.mixer.stop().is_err());
}

#[test]
fn failed_helper_termination_remains_owned_until_stop_retry() {
    let mut f = Fixture::new();
    f.apply();
    f.until(|w| w.route("openwave_personal_mix", "headphones", None));
    f.world
        .lock()
        .expect("routing fixture lock poisoned")
        .fail_terminate
        .insert("openwave_loop_output_personal".into());
    assert!(f.mixer.stop().is_err());
    assert!(f.mixer.stop().is_err());
    {
        let mut w = f.world.lock().expect("routing fixture lock poisoned");
        assert!(w.node_id("openwave_loop_output_personal").is_some());
        assert!(w.unloads.is_empty());
        w.fail_terminate.clear();
    }
    f.mixer.stop().unwrap();
    let w = f.world.lock().expect("routing fixture lock poisoned");
    assert!(w.children.is_empty());
    assert!(w.modules.is_empty());
    assert!(w.sinks.contains_key("headphones"));
}

#[test]
fn drop_retries_retained_cleanup_after_failed_stop() {
    let world = Arc::new(Mutex::new(World::new()));
    let (mut mixer, events) = Mixer::start_with_backend(
        Box::new(FakeBackend(world.clone())),
        CaptureReadiness::default(),
        Duration::from_millis(10),
    )
    .unwrap();
    let desired = DesiredState {
        mixes: default_mixes(),
        ..DesiredState::default()
    };
    mixer
        .set_desired(1, Arc::new(desired), IndexMap::new())
        .unwrap();
    let start = Instant::now();
    loop {
        events.recv_timeout(Duration::from_secs(4)).unwrap();
        if world.lock().expect("routing fixture lock poisoned").route(
            "openwave_personal_mix",
            "headphones",
            None,
        ) {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(4),
            "routing did not converge"
        );
    }
    world
        .lock()
        .expect("routing fixture lock poisoned")
        .fail_snapshot = true;
    assert!(mixer.stop().is_err());
    world
        .lock()
        .expect("routing fixture lock poisoned")
        .fail_snapshot = false;
    drop(mixer);
    let w = world.lock().expect("routing fixture lock poisoned");
    assert!(w.children.is_empty());
    assert!(w.modules.is_empty());
    assert!(w.sinks.contains_key("headphones"));
}

#[test]
fn no_output_still_publishes_and_missing_explicit_output_never_falls_back() {
    let mut f = Fixture::new();
    f.apply();
    f.until(|w| {
        w.node_id("openwave_capture_chat").is_some()
            && w.route("openwave_personal_mix", "headphones", None)
    });
    assert!(
        !f.world
            .lock()
            .unwrap()
            .route("openwave_chat_mix", "headphones", None)
    );
    let capture = f
        .world
        .lock()
        .expect("routing fixture lock poisoned")
        .node_id("openwave_capture_personal")
        .unwrap();
    f.desired
        .matrix
        .outputs
        .insert(mid("personal"), "unplugged".into());
    f.apply();
    f.until(|w| !w.route("openwave_personal_mix", "headphones", None));
    assert_eq!(
        f.world.lock().unwrap().node_id("openwave_capture_personal"),
        Some(capture)
    );
}

#[test]
fn unknown_discovery_retains_snapshot_and_does_not_mutate() {
    let mut f = Fixture::new();
    f.world
        .lock()
        .expect("routing fixture lock poisoned")
        .capture("mic", 1);
    f.device("mic", "mic", 1);
    f.apply();
    f.until(|w| {
        w.node_id("openwave_capture_record").is_some()
            && w.route("openwave_personal_mix", "headphones", None)
    });
    let before = f.observation(|o| {
        o.captures.len() == 1 && o.meter_targets.iter().any(|t| t.key == "src:mic")
    });
    let mutations = {
        let mut w = f.world.lock().expect("routing fixture lock poisoned");
        w.fail_snapshot = true;
        w.mutations
    };
    let after = f.observation(|o| o.errors.iter().any(|e| e.target == "graph"));
    assert_eq!(
        after
            .captures
            .iter()
            .map(|capture| (&capture.node_name, &capture.identity))
            .collect::<Vec<_>>(),
        before
            .captures
            .iter()
            .map(|capture| (&capture.node_name, &capture.identity))
            .collect::<Vec<_>>()
    );
    assert!(matches!(after.observation, Observation::Unknown(_)));
    assert!(
        after
            .captures
            .iter()
            .all(|capture| matches!(capture.muted, Observation::Unknown(_)))
    );
    assert_eq!(
        f.world
            .lock()
            .expect("routing fixture lock poisoned")
            .mutations,
        mutations
    );
}

#[test]
fn corrupt_graph_missing_cookie_duplicate_identity_and_partial_edges_are_unknown() {
    let mut w = World::new();
    let first = w.stream("Player", "Player", "headphones");
    w.stream("Player", "Player", "headphones");
    let graph = w.snapshot().unwrap();
    assert_eq!(graph.streams.len(), 2);
    assert_ne!(graph.streams[0].identity, graph.streams[1].identity);
    let objects =
        json!([{ "id":1,"type":"PipeWire:Interface:Node","info":{"props":{"node.name":"x"}} }]);
    assert!(
        GraphSnapshot::parse(
            &objects,
            &json!([]),
            &json!([]),
            &json!([]),
            &json!([]),
            None
        )
        .is_err()
    );
    w.links.insert((first + 2, 999));
    assert!(w.snapshot().is_err());
    w.links.clear();
    let other = w.node("other", props(json!({})));
    w.objects.get_mut(&other).unwrap()["info"]["props"]["object.serial"] = json!(first + 10000);
    assert!(w.snapshot().is_err());
}

#[test]
fn missing_pulse_move_capability_reports_failure_without_copy_route() {
    let mut f = Fixture::new();
    let stream = {
        let mut w = f.world.lock().expect("routing fixture lock poisoned");
        let id = w.stream("Music", "Music", "headphones");
        w.inputs.remove(&id);
        id
    };
    f.app("music", "Music");
    f.cell("music", "chat", 0.5);
    f.apply();
    f.observation(|o| {
        o.errors
            .iter()
            .any(|e| e.target == format!("stream:{}", stream + 10000))
    });
    let w = f.world.lock().expect("routing fixture lock poisoned");
    assert!(w.links.iter().all(|(source, _)| *source != stream + 2));
}

#[test]
fn foreign_same_name_is_not_adopted_and_reused_module_is_never_unloaded() {
    let mut f = Fixture::new();
    let foreign = f.world.lock().expect("routing fixture lock poisoned").sink(
        "openwave_src_music",
        Some((8, "foreign")),
        None,
    );
    let stream = f
        .world
        .lock()
        .expect("routing fixture lock poisoned")
        .stream("Music", "Music", "headphones");
    f.app("music", "Music");
    f.apply();
    f.observation(|o| o.errors.iter().any(|e| e.target == "source:music"));
    {
        let w = f.world.lock().expect("routing fixture lock poisoned");
        assert_eq!(w.destination(stream), Some("headphones"));
        assert_eq!(w.node_id("openwave_src_music"), Some(foreign));
    }
    f.desired.sources.clear();
    f.apply();
    f.mixer.stop().unwrap();
    assert!(!f.world.lock().unwrap().unloads.contains(&8));
}

#[test]
fn last_moment_server_restart_refuses_teardown_module_cleanup() {
    let mut f = Fixture::new();
    f.app("music", "Music");
    f.apply();
    f.until(|w| {
        w.sinks.contains_key("openwave_src_music")
            && w.route("openwave_personal_mix", "headphones", None)
    });
    f.world
        .lock()
        .expect("routing fixture lock poisoned")
        .restart_on_terminate = true;
    assert!(f.mixer.stop().is_err());
    assert!(f.mixer.stop().is_err());
    let w = f.world.lock().expect("routing fixture lock poisoned");
    assert!(w.unloads.is_empty());
    assert!(w.sinks.contains_key("openwave_src_music"));
}

#[test]
fn offline_capture_mute_survives_same_binding_generation_but_not_readd_epoch() {
    let mut f = Fixture::new();
    f.device("mic", "microphone", 1);
    f.apply();
    let binding = f.bindings["microphone"].clone();
    f.mixer
        .set_capture_mute("microphone".into(), binding.clone(), true)
        .unwrap();
    f.cycles(2);
    f.world
        .lock()
        .expect("routing fixture lock poisoned")
        .capture("microphone", 1);
    f.until(|w| w.sources["microphone"]["mute"] == true);
    {
        let mut w = f.world.lock().expect("routing fixture lock poisoned");
        w.fail_mutes = 100;
    }
    f.mixer
        .set_capture_mute("microphone".into(), binding, false)
        .unwrap();
    f.observation(|o| o.errors.iter().any(|e| e.target == "microphone"));
    f.bindings.clear();
    f.desired.sources.clear();
    f.apply();
    f.device("mic", "microphone", 2);
    f.apply();
    assert!(
        f.mixer
            .set_capture_mute(
                "microphone".into(),
                CaptureBinding {
                    epoch: 1,
                    owners: vec![sid("mic")]
                },
                false
            )
            .is_err()
    );
    f.world
        .lock()
        .expect("routing fixture lock poisoned")
        .fail_mutes = 0;
    f.cycles(3);
    assert_eq!(f.world.lock().unwrap().sources["microphone"]["mute"], true);
}

#[test]
fn pending_capture_mute_retries_on_new_node_only_with_same_epoch() {
    let mut f = Fixture::new();
    f.world
        .lock()
        .expect("routing fixture lock poisoned")
        .capture("microphone", 1);
    f.device("mic", "microphone", 5);
    f.apply();
    f.world
        .lock()
        .expect("routing fixture lock poisoned")
        .fail_mutes = 100;
    f.mixer
        .set_capture_mute("microphone".into(), f.bindings["microphone"].clone(), true)
        .unwrap();
    f.observation(|o| o.errors.iter().any(|e| e.target == "microphone"));
    {
        let mut w = f.world.lock().expect("routing fixture lock poisoned");
        w.capture("microphone", 2);
        w.fail_mutes = 0;
    }
    f.until(|w| w.sources["microphone"]["mute"] == true);
}

#[test]
fn unowned_capture_request_is_rejected_before_a_later_binding() {
    let mut f = Fixture::new();
    f.world
        .lock()
        .expect("routing fixture lock poisoned")
        .capture("microphone", 1);
    assert!(
        f.mixer
            .set_capture_mute(
                "microphone".into(),
                CaptureBinding {
                    epoch: 1,
                    owners: vec![sid("mic")]
                },
                true
            )
            .is_err()
    );
    f.device("mic", "microphone", 1);
    f.apply();
    f.cycles(3);
    assert_eq!(f.world.lock().unwrap().sources["microphone"]["mute"], false);
}

#[test]
fn fx_failure_is_silent_and_same_name_capture_replacement_uses_current_channels() {
    let mut f = Fixture::new();
    {
        let mut w = f.world.lock().expect("routing fixture lock poisoned");
        w.capture("mic", 1);
        w.fail_filter = true;
    }
    f.device("mic", "mic", 1);
    let source = f.desired.sources.get_mut(&sid("mic")).unwrap();
    source.fx = Some(FxSettings {
        lowcut: 80,
        ..FxSettings::default()
    });
    source.extra.insert("channels".into(), json!(2));
    f.cell("mic", "chat", 0.5);
    f.apply();
    f.observation(|o| o.errors.iter().any(|e| e.target == "fx:mic"));
    assert!(
        !f.world
            .lock()
            .unwrap()
            .route("mic", "openwave_chat_mix", None)
    );
    // Changing the raw generation invalidates the old retry and its channel metadata.
    {
        let mut w = f.world.lock().expect("routing fixture lock poisoned");
        w.fail_filter = false;
        w.capture("mic", 1);
    }
    f.until(|w| w.route("openwave_fx_mic", "openwave_chat_mix", Some((0.5, false))));
    assert_eq!(f.world.lock().unwrap().filter_channels.last(), Some(&1));
    let old_fx = f
        .world
        .lock()
        .expect("routing fixture lock poisoned")
        .identity("openwave_fx_mic");
    f.world
        .lock()
        .expect("routing fixture lock poisoned")
        .capture("mic", 2);
    f.until(|w| {
        w.node_id("openwave_fx_mic").is_some()
            && w.identity("openwave_fx_mic") != old_fx
            && w.route("openwave_fx_mic", "openwave_chat_mix", Some((0.5, false)))
    });
    assert_eq!(f.world.lock().unwrap().filter_channels.last(), Some(&2));
}

#[test]
fn enabled_filter_requires_owned_output_and_raw_input_link() {
    let mut f = Fixture::new();
    f.world
        .lock()
        .expect("routing fixture lock poisoned")
        .capture("mic", 1);
    f.device("mic", "mic", 1);
    f.desired.sources.get_mut(&sid("mic")).unwrap().fx = Some(FxSettings {
        mono: true,
        ..FxSettings::default()
    });
    f.cell("mic", "chat", 0.5);
    f.apply();
    f.until(|w| w.route("openwave_fx_mic", "openwave_chat_mix", Some((0.5, false))));
    {
        let mut w = f.world.lock().expect("routing fixture lock poisoned");
        w.links.clear();
        w.fail_links = 100;
    }
    f.observation(|o| o.errors.iter().any(|e| e.target == "fx:mic"));
    f.until(|w| !w.route("openwave_fx_mic", "openwave_chat_mix", None));
    assert!(
        !f.world
            .lock()
            .unwrap()
            .route("mic", "openwave_chat_mix", None)
    );
    f.world
        .lock()
        .expect("routing fixture lock poisoned")
        .fail_links = 0;
    f.until(|w| w.route("openwave_fx_mic", "openwave_chat_mix", Some((0.5, false))));
}

#[test]
fn explicit_mix_removal_checks_persistent_definition_generation() {
    let mut f = Fixture::new();
    let mix = f.desired.mixes[&mid("chat")].clone();
    f.world
        .lock()
        .expect("routing fixture lock poisoned")
        .sink(&mix.sink, None, Some(&mix));
    f.apply();
    f.until(|w| w.node_id("openwave_capture_chat").is_some());
    let replacement = f
        .world
        .lock()
        .expect("routing fixture lock poisoned")
        .sink(&mix.sink, None, None);
    f.desired.mixes.shift_remove(&mid("chat"));
    f.apply();
    f.observation(|o| {
        o.errors
            .iter()
            .any(|e| e.message.contains("identity changed"))
    });
    assert_eq!(
        f.world.lock().unwrap().node_id(&mix.sink),
        Some(replacement)
    );
}

#[test]
fn known_persistent_mix_removed_but_shutdown_preserves_other_definitions() {
    let mut f = Fixture::new();
    let chat = f.desired.mixes[&mid("chat")].clone();
    let personal = f.desired.mixes[&mid("personal")].clone();
    {
        let mut w = f.world.lock().expect("routing fixture lock poisoned");
        w.sink(&chat.sink, None, Some(&chat));
        w.sink(&personal.sink, None, Some(&personal));
    }
    f.apply();
    f.until(|w| w.node_id("openwave_capture_chat").is_some());
    f.desired.mixes.shift_remove(&mid("chat"));
    f.apply();
    f.until(|w| !w.sinks.contains_key(&chat.sink));
    f.mixer.stop().unwrap();
    assert!(f.world.lock().unwrap().sinks.contains_key(&personal.sink));
}

#[test]
fn failed_generated_definition_write_preserves_previous_live_graph() {
    let mut f = Fixture::new();
    f.apply();
    f.until(|w| w.node_id("openwave_capture_chat").is_some());
    let publication = f
        .world
        .lock()
        .expect("routing fixture lock poisoned")
        .node_id("openwave_capture_chat")
        .unwrap();
    f.world
        .lock()
        .expect("routing fixture lock poisoned")
        .fail_write = true;
    f.desired.mixes.shift_remove(&mid("chat"));
    f.apply();
    f.observation(|o| o.errors.iter().any(|e| e.target == "routing"));
    {
        let w = f.world.lock().expect("routing fixture lock poisoned");
        assert!(w.definitions.contains_key(&mid("chat")));
        assert_eq!(w.node_id("openwave_capture_chat"), Some(publication));
    }
    f.world
        .lock()
        .expect("routing fixture lock poisoned")
        .fail_write = false;
    f.until(|w| {
        !w.definitions.contains_key(&mid("chat")) && w.node_id("openwave_capture_chat").is_none()
    });
}

#[test]
fn capture_dispatch_rechecks_next_binding_after_a_blocked_earlier_write() {
    let mut f = Fixture::new();
    {
        let mut w = f.world.lock().expect("routing fixture lock poisoned");
        w.capture("microphone", 1);
        w.capture("other_microphone", 1);
    }
    f.device("mic", "microphone", 1);
    f.device("other", "other_microphone", 2);
    f.apply();
    let (arrived_tx, arrived_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    f.world
        .lock()
        .expect("routing fixture lock poisoned")
        .mute_gate = Some(("microphone".into(), arrived_tx, release_rx));
    f.mixer
        .set_capture_mute("microphone".into(), f.bindings["microphone"].clone(), true)
        .unwrap();
    f.mixer
        .set_capture_mute(
            "other_microphone".into(),
            f.bindings["other_microphone"].clone(),
            true,
        )
        .unwrap();
    arrived_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    f.desired.sources.shift_remove(&sid("other"));
    f.bindings.shift_remove("other_microphone");
    f.apply();
    release_tx.send(()).unwrap();
    f.cycles(3);
    let w = f.world.lock().expect("routing fixture lock poisoned");
    assert_eq!(w.sources["microphone"]["mute"], true);
    assert_eq!(w.sources["other_microphone"]["mute"], false);
}

#[test]
fn rebind_then_return_does_not_resurrect_an_old_pending_capture_mute() {
    let mut f = Fixture::new();
    {
        let mut w = f.world.lock().expect("routing fixture lock poisoned");
        w.capture("microphone", 1);
        w.capture("other", 1);
        w.fail_mutes = 100;
    }
    f.device("mic", "microphone", 1);
    f.apply();
    f.mixer
        .set_capture_mute("microphone".into(), f.bindings["microphone"].clone(), true)
        .unwrap();
    f.observation(|o| o.errors.iter().any(|e| e.target == "microphone"));
    f.bindings.clear();
    f.device("mic", "other", 2);
    f.apply();
    f.bindings.clear();
    f.device("mic", "microphone", 3);
    f.apply();
    f.world
        .lock()
        .expect("routing fixture lock poisoned")
        .fail_mutes = 0;
    f.cycles(3);
    let w = f.world.lock().expect("routing fixture lock poisoned");
    assert_eq!(w.sources["microphone"]["mute"], false);
    assert_eq!(w.sources["other"]["mute"], false);
}

#[test]
fn wave_output_requires_bytes_from_exact_current_raw_generation_even_unbound() {
    let readiness = CaptureReadiness::default();
    let mut f = Fixture::with_readiness(readiness.clone());
    let capture = "alsa_input.usb-Elgato_Wave_XLR_1234-00.mono-fallback";
    let output = "alsa_output.usb-Elgato_Wave_XLR_1234-00.analog-stereo";
    let old = {
        let mut w = f.world.lock().expect("routing fixture lock poisoned");
        w.sink(output, None, None);
        w.capture(capture, 1);
        w.identity(capture)
    };
    f.apply();
    f.until(|w| w.node_id("openwave_capture_personal").is_some());
    let observation = f.observation(|o| {
        o.meter_targets
            .iter()
            .any(|t| t.raw && t.node_name == capture)
    });
    assert!(
        observation
            .meter_targets
            .iter()
            .any(|t| t.identity == old && t.key.starts_with("raw:"))
    );
    assert!(
        !f.world
            .lock()
            .unwrap()
            .route("openwave_personal_mix", output, None)
    );
    let old_tap = readiness
        .register(old.clone(), Instant::now(), Duration::ZERO)
        .unwrap();
    old_tap.started(Instant::now(), None).unwrap();
    old_tap.received(Instant::now()).unwrap();
    f.until(|w| w.route("openwave_personal_mix", output, None));
    let new = {
        let mut w = f.world.lock().expect("routing fixture lock poisoned");
        w.capture(capture, 1);
        w.identity(capture)
    };
    f.until(|w| !w.route("openwave_personal_mix", output, None));
    assert!(readiness.ready(&old));
    assert!(!readiness.ready(&new));
    assert!(
        !f.world
            .lock()
            .unwrap()
            .route("openwave_personal_mix", "headphones", None)
    );
    let new_tap = readiness
        .register(new, Instant::now(), Duration::ZERO)
        .unwrap();
    new_tap.received(Instant::now()).unwrap();
    f.until(|w| w.route("openwave_personal_mix", output, None));
}

#[test]
fn automatic_selects_ready_wave_pair_without_retargeting_explicit_output() {
    let readiness = CaptureReadiness::default();
    let mut f = Fixture::with_readiness(readiness.clone());
    let capture_a = "alsa_input.usb-Elgato_Wave_XLR_A-00.mono-fallback";
    let capture_b = "alsa_input.usb-Elgato_Wave_XLR_B-00.mono-fallback";
    let output_a = "alsa_output.usb-Elgato_Wave_XLR_A-00.analog-stereo";
    let output_b = "alsa_output.usb-Elgato_Wave_XLR_B-00.analog-stereo";
    let identity_b = {
        let mut world = f.world.lock().unwrap();
        world.sink(output_a, None, None);
        world.sink(output_b, None, None);
        world.capture(capture_a, 1);
        world.capture(capture_b, 1);
        world.identity(capture_b)
    };
    let tap_b = readiness
        .register(identity_b, Instant::now(), Duration::ZERO)
        .unwrap();
    tap_b.started(Instant::now(), None).unwrap();
    tap_b.received(Instant::now()).unwrap();
    f.apply();
    f.until(|world| world.route("openwave_personal_mix", output_b, None));
    assert!(
        !f.world
            .lock()
            .unwrap()
            .route("openwave_personal_mix", output_a, None)
    );

    f.desired
        .matrix
        .outputs
        .insert(mid("personal"), output_a.into());
    f.apply();
    f.until(|world| !world.route("openwave_personal_mix", output_b, None));
    f.cycles(2);
    {
        let world = f.world.lock().unwrap();
        assert!(!world.route("openwave_personal_mix", output_a, None));
        assert!(!world.route("openwave_personal_mix", "headphones", None));
        assert!(world.node_id("openwave_capture_personal").is_some());
    }

    f.desired
        .matrix
        .outputs
        .insert(mid("personal"), "auto".into());
    f.apply();
    f.until(|world| world.route("openwave_personal_mix", output_b, None));
    f.desired
        .matrix
        .outputs
        .insert(mid("personal"), "unplugged_headphones".into());
    f.apply();
    f.until(|world| !world.route("openwave_personal_mix", output_b, None));
    f.cycles(2);
    let world = f.world.lock().unwrap();
    assert!(!world.route("openwave_personal_mix", output_a, None));
    assert!(!world.route("openwave_personal_mix", "headphones", None));
}

#[test]
fn replaced_owned_module_id_cannot_authorize_foreign_unload() {
    let mut f = Fixture::new();
    f.app("music", "Music");
    f.apply();
    f.until(|w| w.sinks.contains_key("openwave_src_music"));
    let (module, replacement) = {
        let mut w = f.world.lock().expect("routing fixture lock poisoned");
        let id = w.node_id("openwave_src_music").unwrap();
        let module = w.objects[&id]["info"]["props"]["pulse.module.id"]
            .as_u64()
            .unwrap() as u32;
        let replacement = w.sink("openwave_src_music", Some((module, "not-our-owner")), None);
        (module, replacement)
    };
    f.desired.sources.clear();
    f.apply();
    f.observation(|o| {
        o.errors
            .iter()
            .any(|e| e.message.contains("unload refused"))
    });
    assert!(f.mixer.stop().is_err());
    assert!(f.mixer.stop().is_err());
    let w = f.world.lock().expect("routing fixture lock poisoned");
    assert_eq!(w.node_id("openwave_src_music"), Some(replacement));
    assert!(!w.unloads.contains(&module));
}

#[test]
fn foreign_route_node_never_receives_level_or_links() {
    let mut f = Fixture::new();
    f.world
        .lock()
        .expect("routing fixture lock poisoned")
        .capture("microphone", 1);
    f.device("mic", "microphone", 1);
    let name = routing::cell_route_name(&sid("mic"), &mid("chat"));
    let foreign = f
        .world
        .lock()
        .expect("routing fixture lock poisoned")
        .node(&name, props(json!({"openwave.owner":"foreign"})));
    f.cell("mic", "chat", 0.2);
    f.apply();
    f.observation(|o| o.errors.iter().any(|e| e.target == "cell:mic.chat"));
    let w = f.world.lock().expect("routing fixture lock poisoned");
    assert_eq!(w.levels[&foreign], (1.0, false));
    assert!(
        w.links
            .iter()
            .all(|(a, b)| *a != foreign + 2 && *b != foreign + 1)
    );
    drop(w);
    f.desired.sources.get_mut(&sid("mic")).unwrap().muted = true;
    f.apply();
    let blocked = f.observation(|o| {
        o.revision == f.revision && o.errors.iter().any(|e| e.target == "cell:mic.chat")
    });
    assert!(!blocked.silent_sources.contains_key(&sid("mic")));
}

#[test]
fn missing_selected_device_never_uses_another_live_microphone() {
    let mut f = Fixture::new();
    f.world
        .lock()
        .expect("routing fixture lock poisoned")
        .capture("other", 1);
    f.device("mic", "missing", 1);
    f.cell("mic", "chat", 0.8);
    f.apply();
    f.until(|w| w.node_id("openwave_capture_chat").is_some());
    f.cycles(3);
    let w = f.world.lock().expect("routing fixture lock poisoned");
    assert!(!w.route("other", "openwave_chat_mix", None));
    assert!(w.node_id("openwave_loop_3_mic_chat").is_none());
}

#[test]
fn stopped_mixer_rejects_all_mutations_without_discovery() {
    let mut f = Fixture::new();
    f.mixer.stop().unwrap();
    assert!(
        f.mixer
            .set_desired(1, Arc::new(f.desired.clone()), IndexMap::new())
            .is_err()
    );
    assert!(
        f.mixer
            .set_capture_mute(
                "mic".into(),
                CaptureBinding {
                    epoch: 1,
                    owners: vec![sid("mic")]
                },
                true
            )
            .is_err()
    );
    let w = f.world.lock().expect("routing fixture lock poisoned");
    assert_eq!(w.snapshots, 0);
    assert_eq!(w.mutations, 0);
}

#[test]
fn node_id_fallback_never_becomes_meter_serial_authority() {
    let mut f = Fixture::new();
    {
        let mut w = f.world.lock().expect("routing fixture lock poisoned");
        let id = w.capture("microphone", 1);
        w.objects.get_mut(&id).unwrap()["info"]["props"]
            .as_object_mut()
            .unwrap()
            .remove("object.serial");
        w.sources.get_mut("microphone").unwrap()["properties"]
            .as_object_mut()
            .unwrap()
            .remove("object.serial");
    }
    f.device("mic", "microphone", 1);
    f.apply();
    let observed = f.observation(|o| o.errors.iter().any(|e| e.target == "src:mic"));
    assert_eq!(observed.captures.len(), 1);
    assert!(
        !observed
            .meter_targets
            .iter()
            .any(|target| target.key == "src:mic")
    );
}

#[test]
fn missing_original_stream_sink_uses_only_an_eligible_destination() {
    let mut f = Fixture::new();
    let stream = {
        let mut w = f.world.lock().expect("routing fixture lock poisoned");
        w.sink("speakers", None, None);
        w.stream("Music", "Music", "speakers")
    };
    f.app("music", "Music");
    f.apply();
    f.until(|w| w.destination(stream) == Some("openwave_src_music"));
    {
        let mut w = f.world.lock().expect("routing fixture lock poisoned");
        let id = w.node_id("speakers").unwrap();
        w.remove_node(id);
    }
    f.desired.sources.clear();
    f.apply();
    f.until(|w| {
        w.destination(stream) == Some("headphones") && !w.sinks.contains_key("openwave_src_music")
    });
}

#[test]
fn owner_reordering_preserves_same_epoch_pending_mute() {
    let mut f = Fixture::new();
    f.world
        .lock()
        .expect("routing fixture lock poisoned")
        .capture("microphone", 1);
    f.device("first", "microphone", 7);
    f.device("second", "microphone", 7);
    f.bindings.get_mut("microphone").unwrap().owners = vec![sid("first"), sid("second")];
    f.apply();
    f.world
        .lock()
        .expect("routing fixture lock poisoned")
        .fail_mutes = 100;
    f.mixer
        .set_capture_mute("microphone".into(), f.bindings["microphone"].clone(), true)
        .unwrap();
    f.observation(|o| o.errors.iter().any(|e| e.target == "microphone"));
    f.bindings.get_mut("microphone").unwrap().owners.reverse();
    f.apply();
    f.world
        .lock()
        .expect("routing fixture lock poisoned")
        .fail_mutes = 0;
    f.until(|w| w.sources["microphone"]["mute"] == true);
}

#[test]
fn master_events_follow_proven_identity_observation_and_carry_desired_revision() {
    let mut f = Fixture::new();
    f.desired.matrix.volumes.insert(
        mid("chat"),
        LevelState {
            volume: 0.3,
            muted: true,
            extra: Map::new(),
        },
    );
    f.apply();
    let mut identities = IndexMap::new();
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        assert!(Instant::now() < deadline);
        match f.events.recv_timeout(Duration::from_secs(4)).unwrap() {
            MixerEvent::Observed(observation) => identities = observation.mix_identities,
            MixerEvent::MasterObserved {
                mix,
                revision,
                identity,
                level,
                muted,
            } if mix == mid("chat") => {
                assert_eq!(revision, f.revision);
                assert_eq!(identities.get(&mix), Some(&identity));
                assert!((level - 0.3).abs() < 0.001);
                assert!(muted);
                break;
            }
            _ => {}
        }
    }
    // If a consumer rejected a queued old-revision observation, an unrelated
    // desired edit must not permanently suppress the current master observation.
    f.desired
        .matrix
        .outputs
        .insert(mid("personal"), "none".into());
    f.apply();
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        assert!(Instant::now() < deadline);
        match f.events.recv_timeout(Duration::from_secs(4)).unwrap() {
            MixerEvent::Observed(observation) => identities = observation.mix_identities,
            MixerEvent::MasterObserved {
                mix,
                revision,
                identity,
                level,
                muted,
            } if mix == mid("chat") && revision == f.revision => {
                assert_eq!(identities.get(&mix), Some(&identity));
                assert!((level - 0.3).abs() < 0.001);
                assert!(muted);
                break;
            }
            _ => {}
        }
    }
}

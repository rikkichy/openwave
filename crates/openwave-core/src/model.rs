use crate::{
    effects::FxSettings,
    profiles::ProfileId,
    protocol::{DeviceInfo, DeviceState},
    scenes::{Scene, SceneId},
};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{collections::HashSet, fmt, sync::Arc};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Invalid,
    Unsupported,
    Unavailable,
    Busy,
    Io,
    CorruptStore,
    Cancelled,
    Identity,
    Frozen,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[error("{message}")]
pub struct OperationError {
    pub code: ErrorCode,
    pub message: String,
}
impl OperationError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Invalid, message)
    }
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unavailable, message)
    }
}
impl From<std::io::Error> for OperationError {
    fn from(value: std::io::Error) -> Self {
        Self::new(ErrorCode::Io, value.to_string())
    }
}
impl From<serde_json::Error> for OperationError {
    fn from(value: serde_json::Error) -> Self {
        Self::invalid(value.to_string())
    }
}
pub type Result<T, E = OperationError> = std::result::Result<T, E>;

macro_rules! stable_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);
        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self> {
                let value = value.into();
                if value.is_empty()
                    || !value
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
                {
                    return Err(OperationError::invalid(format!(
                        "Invalid stable id: {value:?}"
                    )));
                }
                Ok(Self(value))
            }
            pub fn generate() -> Self {
                let mut buffer = [0; 32];
                let uuid = uuid::Uuid::new_v4();
                Self(uuid.simple().encode_lower(&mut buffer)[..12].to_owned())
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl TryFrom<String> for $name {
            type Error = OperationError;
            fn try_from(v: String) -> Result<Self> {
                Self::new(v)
            }
        }
        impl From<$name> for String {
            fn from(v: $name) -> Self {
                v.0
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }
        impl std::borrow::Borrow<str> for $name {
            fn borrow(&self) -> &str {
                self.as_str()
            }
        }
    };
}
stable_id!(SourceId);
stable_id!(MixId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UnitId {
    pub profile: ProfileId,
    pub bus: u8,
    pub address: u8,
    pub incarnation: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeIdentity {
    pub server_cookie: u32,
    pub object_serial: String,
}
#[derive(Debug, Clone, PartialEq)]
pub enum Observation<T> {
    Known(T),
    Unknown(OperationError),
}
impl<T> Observation<T> {
    pub fn known(&self) -> Option<&T> {
        match self {
            Self::Known(v) => Some(v),
            Self::Unknown(_) => None,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    User,
    Hardware(UnitId),
    Capture(NodeIdentity),
    Scene(SceneId),
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DeviceSetting {
    GainRaw(u16),
    Mute(bool),
    HeadphoneDb(f64),
    Phantom(bool),
    LowImpedance(bool),
    MonitorMix(u16),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CommandId(pub u64);
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OperationIssue {
    pub target: String,
    pub message: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandOutcome {
    Applied {
        revision: u64,
    },
    SceneFinished {
        revision: u64,
        skipped: Vec<OperationIssue>,
        failed: Vec<OperationIssue>,
    },
    Rejected(OperationError),
    Cancelled,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    #[default]
    App,
    Device,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Source {
    pub id: SourceId,
    pub kind: SourceKind,
    pub name: String,
    pub icon_name: String,
    pub match_app_names: Vec<String>,
    pub node_name: String,
    pub level: f64,
    pub muted: bool,
    pub protected: bool,
    pub group: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fx: Option<FxSettings>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
pub type Sources = IndexMap<SourceId, Source>;
impl Source {
    pub fn new(name: String, kind: SourceKind) -> Self {
        Self {
            id: SourceId::generate(),
            kind,
            name,
            icon_name: if kind == SourceKind::Device {
                "audio-input-microphone-symbolic"
            } else {
                "applications-multimedia-symbolic"
            }
            .into(),
            match_app_names: Vec::new(),
            node_name: String::new(),
            level: 1.0,
            muted: false,
            protected: false,
            group: String::new(),
            fx: None,
            extra: Map::new(),
        }
    }
    pub fn catch_all(&self) -> bool {
        self.extra
            .get("catch_all")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }
}
#[derive(Debug, Clone, Default)]
pub struct SourceEdit {
    pub name: Option<String>,
    pub icon_name: Option<String>,
    pub match_app_names: Option<Vec<String>>,
    pub node_name: Option<String>,
    pub catch_all: Option<bool>,
    pub group: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mix {
    pub id: MixId,
    pub name: String,
    pub description: String,
    pub subtitle: String,
    pub icon_name: String,
    pub sink: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
pub type Mixes = IndexMap<MixId, Mix>;
impl Mix {
    pub fn new(name: String) -> Self {
        let id = MixId::generate();
        Self {
            sink: format!("openwave_mix_{id}"),
            id,
            description: format!("OpenWave {name}"),
            name,
            subtitle: String::new(),
            icon_name: "audio-speakers-symbolic".into(),
            extra: Map::new(),
        }
    }
}
#[derive(Debug, Clone, Default)]
pub struct MixEdit {
    pub name: Option<String>,
    pub description: Option<String>,
    pub subtitle: Option<String>,
    pub icon_name: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LevelState {
    pub volume: f64,
    pub muted: bool,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
impl Default for LevelState {
    fn default() -> Self {
        Self {
            volume: 0.0,
            muted: false,
            extra: Map::new(),
        }
    }
}
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MatrixState {
    pub cells: IndexMap<String, LevelState>,
    pub outputs: IndexMap<MixId, String>,
    pub volumes: IndexMap<MixId, LevelState>,
    pub extra: Map<String, Value>,
}
impl MatrixState {
    pub fn cell(&self, source: &SourceId, mix: &MixId) -> LevelState {
        self.cells
            .get(&format!("{source}.{mix}"))
            .cloned()
            .unwrap_or_default()
    }
    pub fn output(&self, mix: &MixId) -> &str {
        self.outputs
            .get(mix)
            .map(String::as_str)
            .unwrap_or(if mix.as_str() == "personal" {
                "auto"
            } else {
                "none"
            })
    }
    pub fn from_value(value: Value) -> Result<Self> {
        let mut object = object(value, "matrix")?;
        let mut outputs = crate::model::object(
            object
                .remove("outputs")
                .unwrap_or_else(|| Value::Object(Map::new())),
            "outputs",
        )?;
        if let Some(Value::String(legacy)) = object.remove("output") {
            outputs.entry("personal").or_insert(Value::String(legacy));
        }
        let volumes = object
            .remove("volumes")
            .unwrap_or_else(|| Value::Object(Map::new()));
        let mut state = Self::default();
        for (id, value) in outputs {
            let id = MixId::new(id)?;
            let value = value.as_str().filter(|v| !v.is_empty()).ok_or_else(|| {
                OperationError::invalid("Output must be a nonempty sink name, auto or none")
            })?;
            state.outputs.insert(id, value.into());
        }
        for (id, value) in crate::model::object(volumes, "volumes")? {
            state
                .volumes
                .insert(MixId::new(id)?, decode_level(value, 1.0, true)?);
        }
        for (key, value) in object {
            if key.contains('.') {
                parse_cell_key(&key)?;
                state.cells.insert(key, decode_level(value, 0.0, false)?);
            } else {
                state.extra.insert(key, value);
            }
        }
        Ok(state)
    }
    pub fn to_value(&self) -> Result<Value> {
        let mut value = self.extra.clone();
        for (key, cell) in &self.cells {
            value.insert(key.clone(), serde_json::to_value(cell)?);
        }
        value.insert("outputs".into(), serde_json::to_value(&self.outputs)?);
        value.insert("volumes".into(), serde_json::to_value(&self.volumes)?);
        Ok(Value::Object(value))
    }
}
pub fn parse_cell_key(key: &str) -> Result<(SourceId, MixId)> {
    let (source, mix) = key
        .split_once('.')
        .ok_or_else(|| OperationError::invalid("Cell requires source.mix identity"))?;
    Ok((SourceId::new(source)?, MixId::new(mix)?))
}
pub fn level(value: f64) -> Result<f64> {
    if !value.is_finite() {
        return Err(OperationError::invalid("Level must be finite"));
    }
    Ok(value.clamp(0.0, 1.0))
}
pub fn command_level(value: f64) -> Result<f64> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(OperationError::invalid("Level must be finite and in 0..1"));
    }
    Ok(value)
}
pub fn send_level(value: f64) -> Result<f64> {
    let v = command_level(value)?;
    Ok(if v < 0.01 { 0.0 } else { v })
}
fn legacy_number(value: Value) -> Result<f64> {
    match value {
        Value::Number(n) => n
            .as_f64()
            .ok_or_else(|| OperationError::invalid("Invalid numeric level"))
            .and_then(level),
        Value::Bool(b) => Ok(if b { 1.0 } else { 0.0 }),
        Value::String(s) => s
            .trim()
            .parse::<f64>()
            .map_err(|_| OperationError::invalid("Invalid numeric level"))
            .and_then(level),
        _ => Err(OperationError::invalid("Invalid numeric level")),
    }
}
fn decode_level(value: Value, default: f64, required: bool) -> Result<LevelState> {
    let mut extra = object(value, "level")?;
    let volume = match extra.remove("volume") {
        Some(v) => legacy_number(v)?,
        None if required => return Err(OperationError::invalid("Master requires volume")),
        None => default,
    };
    let muted = take_bool(&mut extra, "muted", false)?;
    Ok(LevelState {
        volume,
        muted,
        extra,
    })
}
pub fn object(value: Value, label: &str) -> Result<Map<String, Value>> {
    match value {
        Value::Object(map) => Ok(map),
        _ => Err(OperationError::invalid(format!(
            "{label} must be an object"
        ))),
    }
}
fn take_string(map: &mut Map<String, Value>, key: &str, default: &str) -> Result<String> {
    match map.remove(key) {
        None => Ok(default.into()),
        Some(Value::String(v)) => Ok(v),
        _ => Err(OperationError::invalid(format!("{key} must be a string"))),
    }
}
fn take_bool(map: &mut Map<String, Value>, key: &str, default: bool) -> Result<bool> {
    match map.remove(key) {
        None => Ok(default),
        Some(Value::Bool(v)) => Ok(v),
        _ => Err(OperationError::invalid(format!("{key} must be boolean"))),
    }
}
pub fn normalize_bindings(names: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    names
        .into_iter()
        .filter_map(|v| {
            let v = v.trim().to_owned();
            if v.is_empty() || !seen.insert(v.clone()) {
                None
            } else {
                Some(v)
            }
        })
        .collect()
}
pub fn normalize_sources(value: Value) -> Result<Sources> {
    let mut sources = Sources::new();
    let mut open_groups = HashSet::new();
    for (key, value) in object(value, "sources")? {
        let id = SourceId::new(key)?;
        let mut extra = object(value, "source")?;
        if take_string(&mut extra, "id", id.as_str())? != id.as_str() {
            return Err(OperationError::invalid("Source identity mismatch"));
        }
        let kind = match take_string(&mut extra, "kind", "app")?.as_str() {
            "app" => SourceKind::App,
            "device" => SourceKind::Device,
            _ => return Err(OperationError::invalid("Unknown source kind")),
        };
        let legacy = extra.remove("match_app_name");
        let bindings = match extra.remove("match_app_names") {
            None | Some(Value::Null) => vec![match legacy {
                None => String::new(),
                Some(Value::String(v)) => v,
                _ => return Err(OperationError::invalid("Legacy binding must be a string")),
            }],
            Some(value) => serde_json::from_value::<Vec<String>>(value)?,
        };
        let name = take_string(&mut extra, "name", id.as_str())?;
        let icon_name = take_string(
            &mut extra,
            "icon_name",
            if kind == SourceKind::Device {
                "audio-input-microphone-symbolic"
            } else {
                "applications-multimedia-symbolic"
            },
        )?;
        let node_name = take_string(&mut extra, "node_name", "")?;
        let group = take_string(&mut extra, "group", "")?.trim().to_owned();
        let level = extra
            .remove("level")
            .map(legacy_number)
            .transpose()?
            .unwrap_or(1.0);
        let mut muted = take_bool(&mut extra, "muted", false)?;
        let protected = take_bool(&mut extra, "protected", false)?;
        let fx = extra.remove("fx").map(FxSettings::from_value).transpose()?;
        if !muted && !group.is_empty() && !open_groups.insert(group.clone()) {
            muted = true;
        }
        sources.insert(
            id.clone(),
            Source {
                id,
                kind,
                name,
                icon_name,
                match_app_names: normalize_bindings(bindings),
                node_name,
                level,
                muted,
                protected,
                group,
                fx,
                extra,
            },
        );
    }
    Ok(sources)
}
pub fn normalize_mixes(value: Value) -> Result<Mixes> {
    let mut mixes = Mixes::new();
    let mut sinks = HashSet::new();
    for (key, value) in object(value, "mix definitions")? {
        let id = MixId::new(key)?;
        let mut extra = object(value, "mix")?;
        if take_string(&mut extra, "id", id.as_str())? != id.as_str() {
            return Err(OperationError::invalid("Mix identity mismatch"));
        }
        let name = take_string(&mut extra, "name", id.as_str())?;
        let description = take_string(&mut extra, "description", &format!("OpenWave {name}"))?;
        let subtitle = take_string(&mut extra, "subtitle", "")?;
        let icon_name = take_string(&mut extra, "icon_name", "audio-speakers-symbolic")?;
        let sink = take_string(&mut extra, "sink", &format!("openwave_mix_{id}"))?;
        let suffix = sink
            .strip_prefix("openwave_")
            .ok_or_else(|| OperationError::invalid("Invalid mix sink"))?;
        SourceId::new(suffix)?;
        if ["openwave_src_", "openwave_loop_", "openwave_capture_"]
            .iter()
            .any(|p| sink.starts_with(p))
            || !sinks.insert(sink.clone())
        {
            return Err(OperationError::invalid("Reserved or duplicate mix sink"));
        }
        mixes.insert(
            id.clone(),
            Mix {
                id,
                name,
                description,
                subtitle,
                icon_name,
                sink,
                extra,
            },
        );
    }
    Ok(mixes)
}
pub fn default_mixes() -> Mixes {
    [
        (
            "personal",
            "Personal Mix",
            "What you hear",
            "audio-headphones-symbolic",
        ),
        (
            "chat",
            "Chat Mix",
            "Send to voice apps",
            "system-users-symbolic",
        ),
        (
            "record",
            "Record Mix",
            "Send to OBS or a recorder",
            "media-record-symbolic",
        ),
    ]
    .into_iter()
    .map(|(key, name, subtitle, icon)| {
        let id = MixId::new(key).expect("static mix id");
        (
            id.clone(),
            Mix {
                id,
                name: name.into(),
                subtitle: subtitle.into(),
                description: format!("OpenWave {name}"),
                sink: format!("openwave_{key}_mix"),
                icon_name: icon.into(),
                extra: Map::new(),
            },
        )
    })
    .collect()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Preferences {
    pub width: i32,
    pub height: i32,
    pub maximized: bool,
    pub offered_capture_nodes: Vec<String>,
    pub gain_locked: bool,
    pub tray_icon_color: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            maximized: false,
            offered_capture_nodes: Vec::new(),
            gain_locked: false,
            tray_icon_color: "white".into(),
            extra: Map::new(),
        }
    }
}
impl Preferences {
    pub fn from_value(value: Value) -> Result<Self> {
        let mut map = object(value, "preferences")?;
        let mut prefs = Self::default();
        for (key, default, minimum) in [
            ("width", &mut prefs.width, 820),
            ("height", &mut prefs.height, 480),
        ] {
            if let Some(value) = map.remove(key) {
                *default =
                    i32::try_from(value.as_i64().ok_or_else(|| {
                        OperationError::invalid(format!("{key} must be an integer"))
                    })?)
                    .map_err(|_| OperationError::invalid("Geometry is out of range"))?
                    .max(minimum);
            }
        }
        prefs.maximized = take_bool(&mut map, "maximized", false)?;
        prefs.gain_locked = take_bool(&mut map, "gain_locked", false)?;
        prefs.tray_icon_color = take_string(&mut map, "tray_icon_color", "white")?;
        if !["white", "black"].contains(&prefs.tray_icon_color.as_str()) {
            return Err(OperationError::invalid(
                "Tray icon color must be white or black",
            ));
        }
        if let Some(value) = map.remove("offered_capture_nodes") {
            prefs.offered_capture_nodes = serde_json::from_value(value)?;
        }
        prefs.extra = map;
        Ok(prefs)
    }
}
#[derive(Debug, Clone, Default)]
pub struct PreferencesEdit {
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub maximized: Option<bool>,
    pub offered_capture_nodes: Option<Vec<String>>,
    pub tray_icon_color: Option<String>,
}
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DesiredState {
    pub sources: Sources,
    pub mixes: Mixes,
    pub matrix: MatrixState,
    pub scenes: IndexMap<SceneId, Scene>,
}
#[derive(Debug, Clone, PartialEq)]
pub struct UnitSnapshot {
    pub id: UnitId,
    pub info: DeviceInfo,
    pub state: Observation<DeviceState>,
    pub desired_mute: Option<bool>,
    pub input_peak: f64,
    pub output_peak: f64,
    pub errors: Vec<OperationIssue>,
}
#[derive(Debug, Clone, PartialEq)]
pub struct CaptureSnapshot {
    pub identity: NodeIdentity,
    pub node_id: u32,
    pub node_name: String,
    pub name: String,
    pub muted: Observation<bool>,
    pub channels: Option<u32>,
    pub properties: Map<String, Value>,
}
#[derive(Debug, Clone, PartialEq)]
pub struct StreamSnapshot {
    pub identity: NodeIdentity,
    pub node_id: u32,
    pub node_name: String,
    pub app_name: String,
    pub binary: String,
    pub sink: Option<String>,
    pub pulse_index: Option<u32>,
}
#[derive(Debug, Clone, PartialEq)]
pub struct OutputSnapshot {
    pub identity: NodeIdentity,
    pub node_id: u32,
    pub node_name: String,
    pub name: String,
    pub priority: i64,
    pub is_wave: bool,
    pub properties: Map<String, Value>,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Lifecycle {
    #[default]
    Starting,
    Running,
    Frozen,
    Draining,
    Stopped,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalibrationToken {
    pub session: u64,
    pub source: SourceId,
    pub node_name: String,
    pub identity: NodeIdentity,
    pub channels: u32,
}
#[derive(Debug, Clone, PartialEq)]
pub enum CalibrationPhase {
    NoiseReady,
    RecordingNoise,
    SpeechReady,
    RecordingSpeech,
    Review {
        proposal: FxSettings,
        summary: String,
    },
    Expired(String),
}
#[derive(Debug, Clone, PartialEq)]
pub struct CalibrationSnapshot {
    pub token: CalibrationToken,
    pub phase: CalibrationPhase,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SetupPhase {
    #[default]
    Checking,
    Required,
    Running,
    Replug(String),
    Failed(String),
    Starting,
    ActivationFailed(String),
    Ready,
}
#[derive(Debug, Clone)]
pub struct AppSnapshot {
    pub revision: u64,
    pub desired: Arc<DesiredState>,
    pub units: Arc<Vec<UnitSnapshot>>,
    pub captures: Arc<Vec<CaptureSnapshot>>,
    pub streams: Arc<Vec<StreamSnapshot>>,
    pub outputs: Arc<Vec<OutputSnapshot>>,
    pub meters: Arc<IndexMap<String, f64>>,
    pub errors: Arc<Vec<OperationIssue>>,
    pub selected_unit: Option<UnitId>,
    pub preferences: Arc<Preferences>,
    pub lifecycle: Lifecycle,
    pub calibration: Option<CalibrationSnapshot>,
    pub autostart: bool,
    pub hidden_autostart: bool,
    pub service_status: String,
    pub setup_required: bool,
    pub scene_outcome: Option<CommandOutcome>,
    pub unit_intents: Arc<IndexMap<UnitId, Vec<DeviceSetting>>>,
    pub default_output: Option<String>,
    pub pending_cells: Arc<IndexMap<String, LevelState>>,
    pub pending_fx: Arc<IndexMap<SourceId, FxSettings>>,
    pub setup_phase: SetupPhase,
    pub scene_pending: bool,
}
impl Default for AppSnapshot {
    fn default() -> Self {
        Self {
            revision: 0,
            desired: Arc::new(DesiredState::default()),
            units: Arc::default(),
            captures: Arc::default(),
            streams: Arc::default(),
            outputs: Arc::default(),
            meters: Arc::default(),
            errors: Arc::default(),
            selected_unit: None,
            preferences: Arc::new(Preferences::default()),
            lifecycle: Lifecycle::Starting,
            calibration: None,
            autostart: false,
            hidden_autostart: false,
            service_status: String::new(),
            setup_required: false,
            scene_outcome: None,
            unit_intents: Arc::default(),
            default_output: None,
            pending_cells: Arc::default(),
            pending_fx: Arc::default(),
            setup_phase: SetupPhase::Checking,
            scene_pending: false,
        }
    }
}

impl AppSnapshot {
    pub fn action_snapshot(&self) -> Result<String> {
        let mut cells = Map::new();
        let mut outputs = Map::new();
        for source in self.desired.sources.keys() {
            for mix in self.desired.mixes.keys() {
                let key = format!("{source}.{mix}");
                let value = self.desired.matrix.cells.get(&key);
                cells.insert(
                    key,
                    serde_json::json!({
                        "volume": value.map_or(0.0, |cell| cell.volume),
                        "muted": value.is_some_and(|cell| cell.muted),
                    }),
                );
            }
        }
        for mix in self.desired.mixes.keys() {
            outputs.insert(
                mix.to_string(),
                Value::String(self.desired.matrix.output(mix).into()),
            );
        }
        let sources: Vec<_> = self.desired.sources.values().map(|source| serde_json::json!({
            "id": source.id, "name": source.name, "kind": source.kind, "group": source.group,
            "fx": source.fx.clone().unwrap_or_default(), "level": source.level, "muted": source.muted,
        })).collect();
        let mixes: Vec<_> = self
            .desired
            .mixes
            .values()
            .map(|mix| {
                serde_json::json!({
                    "id": mix.id, "name": mix.name, "sink": mix.sink,
                })
            })
            .collect();
        let volumes: Map<String, Value> = self
            .desired
            .matrix
            .volumes
            .iter()
            .map(|(mix, state)| {
                (
                    mix.to_string(),
                    serde_json::json!({"volume": state.volume, "muted": state.muted}),
                )
            })
            .collect();
        Ok(serde_json::to_string(&serde_json::json!({
            "sources": sources, "mixes": mixes, "cells": cells, "outputs": outputs,
            "volumes": volumes, "groups": crate::routing::source_groups(&self.desired.sources),
        }))?)
    }
    pub fn action_scenes(&self) -> Result<String> {
        let names: IndexMap<_, _> = self
            .desired
            .scenes
            .iter()
            .map(|(id, scene)| (id, &scene.name))
            .collect();
        Ok(serde_json::to_string(&names)?)
    }
    pub fn action_levels(&self) -> Result<String> {
        let levels: IndexMap<_, _> = self
            .meters
            .iter()
            .map(|(id, peak)| (id, (peak * 10_000.0).round_ties_even() / 10_000.0))
            .collect();
        Ok(serde_json::to_string(&levels)?)
    }
}

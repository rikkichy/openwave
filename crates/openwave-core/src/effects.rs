use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};

use crate::model::{OperationError, Result, Source, SourceId, SourceKind};

pub const FX_NODE_PREFIX: &str = "openwave_fx_";

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FxSettings {
    pub lowcut: u16,
    pub gate: bool,
    pub gate_thresh: f64,
    pub comp: bool,
    pub comp_thresh: f64,
    pub comp_ratio: f64,
    pub eq_low: f64,
    pub eq_mid: f64,
    pub eq_high: f64,
    pub delay_ms: f64,
    pub mono: bool,
}

impl Default for FxSettings {
    fn default() -> Self {
        Self {
            lowcut: 0,
            gate: false,
            gate_thresh: -50.0,
            comp: false,
            comp_thresh: -18.0,
            comp_ratio: 3.0,
            eq_low: 0.0,
            eq_mid: 0.0,
            eq_high: 0.0,
            delay_ms: 0.0,
            mono: false,
        }
    }
}

impl<'de> Deserialize<'de> for FxSettings {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        Self::from_value(Value::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl FxSettings {
    /// Decode the settings object itself, not a containing source record.
    pub fn from_value(value: Value) -> Result<Self> {
        let object = value
            .as_object()
            .ok_or_else(|| OperationError::invalid("fx must be an object"))?;
        let mut settings = Self::default();
        for (key, value) in object {
            match key.as_str() {
                "gate" | "comp" | "mono" => {
                    let value = value.as_bool().ok_or_else(|| {
                        OperationError::invalid(format!("{key} must be a boolean"))
                    })?;
                    match key.as_str() {
                        "gate" => settings.gate = value,
                        "comp" => settings.comp = value,
                        _ => settings.mono = value,
                    }
                }
                "lowcut" | "gate_thresh" | "comp_thresh" | "comp_ratio" | "eq_low" | "eq_mid"
                | "eq_high" | "delay_ms" => {
                    let number = value.as_f64().filter(|n| n.is_finite()).ok_or_else(|| {
                        OperationError::invalid(format!("{key} must be a finite number"))
                    })?;
                    match key.as_str() {
                        "lowcut" => {
                            if ![0.0, 80.0, 120.0].contains(&number) {
                                return Err(OperationError::invalid(
                                    "lowcut must be 0, 80, or 120 Hz",
                                ));
                            }
                            settings.lowcut = number as u16;
                        }
                        "gate_thresh" => settings.gate_thresh = number,
                        "comp_thresh" => settings.comp_thresh = number,
                        "comp_ratio" => settings.comp_ratio = number,
                        "eq_low" => settings.eq_low = number,
                        "eq_mid" => settings.eq_mid = number,
                        "eq_high" => settings.eq_high = number,
                        _ => settings.delay_ms = number,
                    }
                }
                _ => {
                    return Err(OperationError::invalid(format!(
                        "unknown effect setting: {key}"
                    )));
                }
            }
        }
        settings.validated()
    }

    /// Validate typed edits too; public fields are convenient DTOs, not authority.
    pub fn validated(&self) -> Result<Self> {
        if ![0, 80, 120].contains(&self.lowcut) {
            return Err(OperationError::invalid("lowcut must be 0, 80, or 120 Hz"));
        }
        let mut result = self.clone();
        for (key, value, min, max) in [
            ("gate_thresh", &mut result.gate_thresh, -70.0, -20.0),
            ("comp_thresh", &mut result.comp_thresh, -30.0, 0.0),
            ("comp_ratio", &mut result.comp_ratio, 1.0, 10.0),
            ("eq_low", &mut result.eq_low, -12.0, 12.0),
            ("eq_mid", &mut result.eq_mid, -12.0, 12.0),
            ("eq_high", &mut result.eq_high, -12.0, 12.0),
            ("delay_ms", &mut result.delay_ms, 0.0, 500.0),
        ] {
            if !value.is_finite() {
                return Err(OperationError::invalid(format!(
                    "{key} must be a finite number"
                )));
            }
            *value = value.clamp(min, max);
        }
        Ok(result)
    }

    pub fn active(&self) -> bool {
        self.lowcut != 0
            || self.gate
            || (self.comp && self.comp_ratio > 1.0)
            || self.eq_low != 0.0
            || self.eq_mid != 0.0
            || self.eq_high != 0.0
            || self.delay_ms != 0.0
            || self.mono
    }

    /// Reject unknown actions before mutation; lowcut toggles off ↔ 80 Hz.
    pub fn toggle(&mut self, key: &str) -> Result<()> {
        let mut next = self.validated()?;
        match key {
            "lowcut" => next.lowcut = if next.lowcut == 0 { 80 } else { 0 },
            "gate" => next.gate = !next.gate,
            "comp" => next.comp = !next.comp,
            "mono" => next.mono = !next.mono,
            _ => return Err(OperationError::invalid("effect cannot be toggled")),
        }
        *self = next;
        Ok(())
    }
}

/// Compute a candidate; the controller owns persistence and publication.
pub fn toggle_fx(source: &Source, key: &str) -> Result<FxSettings> {
    if source.kind != SourceKind::Device {
        return Err(OperationError::invalid("effects require a device source"));
    }
    let mut settings = source.fx.clone().unwrap_or_default();
    settings.toggle(key)?;
    Ok(settings)
}

pub fn fx_node_name(source_id: &SourceId) -> String {
    format!("{FX_NODE_PREFIX}{source_id}")
}

pub fn validate_raw_node(node: &str) -> Result<()> {
    if node.is_empty()
        || node.contains('\0')
        || node == "0"
        || node == "-1"
        || node.starts_with(FX_NODE_PREFIX)
    {
        return Err(OperationError::invalid("select a raw microphone node"));
    }
    Ok(())
}

/// `None` in discovery means stereo; an explicit unsupported count is an error.
pub fn capture_channels(observed: Option<u32>) -> Result<u32> {
    match observed.unwrap_or(2) {
        n @ (1 | 2) => Ok(n),
        _ => Err(OperationError::invalid(
            "capture must have one or two channels",
        )),
    }
}

pub fn render_fx_config(source: &Source, channels: u32, owner: &str) -> Result<Option<String>> {
    let settings = source
        .fx
        .as_ref()
        .map(FxSettings::validated)
        .transpose()?
        .unwrap_or_default();
    capture_channels(Some(channels))?;
    if !settings.active() {
        return Ok(None);
    }
    validate_raw_node(&source.node_name)?;
    if source.name.contains('\0') {
        return Err(OperationError::invalid("source name must not contain NUL"));
    }
    if owner.is_empty() || owner.contains('\0') {
        return Err(OperationError::invalid("invalid effect owner"));
    }
    let node_name = fx_node_name(&source.id);
    let mut nodes = Vec::new();
    let mut links = Vec::new();
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();
    let downmix = settings.mono && channels == 2;
    let output_channels = if settings.mono { 1 } else { channels };
    if downmix {
        nodes.push(json!({"type":"builtin", "name":"downmix", "label":"mixer", "control":{"Gain 1":0.5,"Gain 2":0.5}}));
        inputs.extend(["downmix:In 1".to_owned(), "downmix:In 2".to_owned()]);
    }
    for channel in 0..output_channels {
        let mut strip: Vec<(String, String)> = Vec::new();
        let mut add = |name: &str,
                       label: &str,
                       control: Option<Value>,
                       plugin: Option<&str>,
                       config: Option<Value>| {
            let name = format!("{name}_{channel}");
            let mut node = json!({"type":if plugin.is_some() { "ladspa" } else { "builtin" }, "name":name, "label":label});
            if let Some(control) = control {
                node["control"] = control;
            }
            if let Some(plugin) = plugin {
                node["plugin"] = json!(plugin);
            }
            if let Some(config) = config {
                node["config"] = config;
            }
            strip.push((
                format!("{name}:{}", if plugin.is_some() { "Input" } else { "In" }),
                format!("{name}:{}", if plugin.is_some() { "Output" } else { "Out" }),
            ));
            nodes.push(node);
        };
        if settings.lowcut != 0 {
            add(
                "hp",
                "bq_highpass",
                Some(json!({"Freq":settings.lowcut,"Q":0.70710678})),
                None,
                None,
            );
        }
        if settings.gate {
            add(
                "gate",
                "gate",
                Some(json!({
                    "Threshold (dB)":settings.gate_thresh,"Attack (ms)":10.0,"Hold (ms)":120.0,
                    "Decay (ms)":150.0,"Range (dB)":-70.0,"LF key filter (Hz)":40.0,
                    "HF key filter (Hz)":20000.0,"Output select (-1 = key listen, 0 = gate, 1 = bypass)":0.0
                })),
                Some("gate_1410"),
                None,
            );
        }
        if settings.comp && settings.comp_ratio > 1.0 {
            add(
                "comp",
                "sc4m",
                Some(json!({
                    "Threshold level (dB)":settings.comp_thresh,"Ratio (1:n)":settings.comp_ratio,
                    "RMS/peak":0.0,"Attack time (ms)":15.0,"Release time (ms)":150.0,
                    "Knee radius (dB)":3.0,"Makeup gain (dB)":0.0
                })),
                Some("sc4m_1916"),
                None,
            );
        }
        for (key, label, frequency, gain) in [
            ("eq_low", "bq_lowshelf", 100.0, settings.eq_low),
            ("eq_mid", "bq_peaking", 1000.0, settings.eq_mid),
            ("eq_high", "bq_highshelf", 8000.0, settings.eq_high),
        ] {
            if gain != 0.0 {
                add(
                    key,
                    label,
                    Some(json!({"Freq":frequency,"Gain":gain,"Q":0.70710678})),
                    None,
                    None,
                );
            }
        }
        if settings.delay_ms != 0.0 {
            add(
                "delay",
                "delay",
                Some(json!({"Delay (s)":settings.delay_ms / 1000.0})),
                None,
                Some(json!({"max-delay":1.0})),
            );
        }
        if strip.is_empty() {
            let name = format!("thru_{channel}");
            nodes.push(json!({"type":"builtin","name":name,"label":"copy"}));
            strip.push((format!("{name}:In"), format!("{name}:Out")));
        }
        for pair in strip.windows(2) {
            links.push(json!({"output":pair[0].1,"input":pair[1].0}));
        }
        if downmix {
            links.push(json!({"output":"downmix:Out","input":strip[0].0}));
        } else {
            inputs.push(strip[0].0.clone());
        }
        outputs.push(strip.last().expect("nonempty strip").1.clone());
    }
    let description = format!("OpenWave FX: {}", source.name);
    let capture_position = if channels == 1 {
        vec!["MONO"]
    } else {
        vec!["FL", "FR"]
    };
    let output_position = if output_channels == 1 {
        vec!["MONO"]
    } else {
        vec!["FL", "FR"]
    };
    let args = json!({
        "node.description":description,"media.name":node_name,"audio.rate":48000,
        "filter.graph":{"nodes":nodes,"links":links,"inputs":inputs,"outputs":outputs},
        "capture.props":{
            "node.name":format!("{node_name}_cap"),"media.name":format!("{node_name}_cap"),
            "target.object":source.node_name,"node.passive":true,"node.dont-reconnect":true,
            "application.name":"OpenWave","node.dont-fallback":true,"node.dont-move":true,
            "node.description":format!("{description} (capture)"),"audio.channels":channels,
            "audio.position":capture_position,"openwave.owner":owner
        },
        "playback.props":{
            "node.name":node_name,"media.name":node_name,"media.class":"Audio/Source",
            "application.name":"OpenWave","node.description":description,
            "audio.channels":output_channels,"audio.position":output_position,"openwave.owner":owner
        }
    });
    let config = json!({
        "context.properties":{"log.level":2},
        "context.spa-libs":{"audio.convert.*":"audioconvert/libspa-audioconvert","support.*":"support/libspa-support"},
        "context.modules":[
            {"name":"libpipewire-module-rt","args":{"nice.level":-11},"flags":["ifexists","nofail"]},
            {"name":"libpipewire-module-protocol-native"},
            {"name":"libpipewire-module-client-node"},
            {"name":"libpipewire-module-adapter"},
            {"name":"libpipewire-module-filter-chain","args":args}
        ]
    });
    let mut rendered = serde_json::to_string_pretty(&config)?;
    rendered.push('\n');
    Ok(Some(rendered))
}

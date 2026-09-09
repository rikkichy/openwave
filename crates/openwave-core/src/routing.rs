use std::collections::{BTreeMap, HashMap, HashSet};

use indexmap::IndexMap;
use serde_json::{Map, Value};

use crate::model::{
    MixId, NodeIdentity, OperationError, OutputSnapshot, Result, SourceId, SourceKind, Sources,
    StreamSnapshot,
};

pub fn normalize_identity(value: &str) -> String {
    let mut spaced = String::with_capacity(value.len());
    for word in value.split_whitespace() {
        if !spaced.is_empty() {
            spaced.push(' ');
        }
        spaced.push_str(word);
    }
    caseless::default_case_fold_str(&spaced)
}

/// Cache this by observed stream identity; the fields are ordered by specificity.
#[derive(Debug, Clone)]
pub struct NormalizedStream {
    pub identity: NodeIdentity,
    identities: [String; 4],
}
impl NormalizedStream {
    pub fn new(stream: &StreamSnapshot) -> Self {
        Self {
            identity: stream.identity.clone(),
            identities: [
                normalize_identity(&stream.app_name),
                normalize_identity(&stream.node_name),
                normalize_identity(&stream.binary),
                normalize_identity(stream.binary.rsplit('/').next().unwrap_or("")),
            ],
        }
    }
}

/// Cache this by desired revision. Each exact binding keeps its stable-ID winner.
#[derive(Debug, Clone)]
pub struct NormalizedMatcher {
    sources: Vec<SourceId>,
    bindings: HashMap<String, SourceId>,
    fallback: Option<SourceId>,
}
impl NormalizedMatcher {
    pub fn new(sources: &Sources) -> Self {
        let mut bindings: HashMap<String, SourceId> = HashMap::new();
        let mut fallback: Option<SourceId> = None;
        for (id, source) in sources {
            if source.kind != SourceKind::App {
                continue;
            }
            if source.catch_all() && fallback.as_ref().is_none_or(|current| id < current) {
                fallback = Some(id.clone());
            }
            for name in &source.match_app_names {
                let name = normalize_identity(name);
                if name.is_empty() {
                    continue;
                }
                bindings
                    .entry(name)
                    .and_modify(|owner| {
                        if id < &*owner {
                            *owner = id.clone();
                        }
                    })
                    .or_insert_with(|| id.clone());
            }
        }
        Self {
            sources: sources.keys().cloned().collect(),
            bindings,
            fallback,
        }
    }

    pub fn owner(&self, stream: &NormalizedStream) -> Option<&SourceId> {
        stream
            .identities
            .iter()
            .find_map(|name| self.bindings.get(name))
            .or(self.fallback.as_ref())
    }

    pub fn claim_normalized(
        &self,
        streams: &[NormalizedStream],
    ) -> IndexMap<SourceId, Vec<NodeIdentity>> {
        let mut claims: IndexMap<_, Vec<_>> = self
            .sources
            .iter()
            .cloned()
            .map(|id| (id, Vec::new()))
            .collect();
        let mut seen = HashSet::new();
        for stream in streams {
            if seen.insert(&stream.identity) {
                if let Some(owner) = self.owner(stream) {
                    claims
                        .get_mut(owner)
                        .expect("matcher owns source")
                        .push(stream.identity.clone());
                }
            }
        }
        claims
    }

    pub fn claim(&self, streams: &[StreamSnapshot]) -> IndexMap<SourceId, Vec<NodeIdentity>> {
        let mut claims: IndexMap<_, Vec<_>> = self
            .sources
            .iter()
            .cloned()
            .map(|id| (id, Vec::new()))
            .collect();
        let mut seen = HashSet::new();
        for stream in streams {
            if seen.insert(&stream.identity) {
                if let Some(owner) = self.owner(&NormalizedStream::new(stream)) {
                    claims
                        .get_mut(owner)
                        .expect("matcher owns source")
                        .push(stream.identity.clone());
                }
            }
        }
        claims
    }
}

pub fn claim_streams(
    sources: &Sources,
    streams: &[StreamSnapshot],
) -> IndexMap<SourceId, Vec<NodeIdentity>> {
    NormalizedMatcher::new(sources).claim(streams)
}
pub fn source_sink_name(source: &SourceId) -> String {
    format!("openwave_src_{source}")
}
pub fn mix_capture_name(mix: &MixId) -> String {
    format!("openwave_capture_{mix}")
}
pub fn cell_route_name(source: &SourceId, mix: &MixId) -> String {
    format!("openwave_loop_{}_{source}_{mix}", source.as_str().len())
}

fn property_key(key: &str) -> Result<()> {
    if key.is_empty()
        || !key
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
    {
        return Err(OperationError::invalid("Invalid audio property key"));
    }
    Ok(())
}

/// SPA values are JSON literals; keys are restricted to property identifiers.
pub fn properties(values: &Map<String, Value>) -> Result<String> {
    let mut text = String::from("{ ");
    for (index, (key, value)) in values.iter().enumerate() {
        property_key(key)?;
        if index != 0 {
            text.push(' ');
        }
        text.push_str(key);
        text.push_str(" = ");
        text.push_str(&serde_json::to_string(value)?);
    }
    text.push_str(" }");
    Ok(text)
}

fn pulse_quote(text: &str) -> Result<String> {
    if text.contains('\0') {
        return Err(OperationError::invalid(
            "Audio properties cannot contain NUL",
        ));
    }
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for c in text.chars() {
        if c == '\\' || c == '"' {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    Ok(quoted)
}

/// The outer module argument parser and inner proplist parser each remove a layer.
pub fn pulse_properties(values: &Map<String, Value>) -> Result<String> {
    let mut pairs = String::new();
    for (index, (key, value)) in values.iter().enumerate() {
        property_key(key)?;
        if index != 0 {
            pairs.push(' ');
        }
        pairs.push_str(key);
        pairs.push('=');
        let text = match value {
            Value::String(text) => pulse_quote(text)?,
            _ => pulse_quote(&serde_json::to_string(value)?)?,
        };
        pairs.push_str(&text);
    }
    pulse_quote(&pairs)
}

pub fn eligible_output(output: &OutputSnapshot) -> bool {
    !output.node_name.is_empty() && !output.node_name.starts_with("openwave_")
}
pub fn validate_output_choice(choice: &str) -> Result<()> {
    if choice.is_empty() || choice.starts_with("openwave_") || choice.contains('\0') {
        return Err(OperationError::invalid(
            "Output must be auto, none or a non-OpenWave sink name",
        ));
    }
    Ok(())
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SilentOutput {
    NotMonitored,
    MissingExplicit,
    NoEligibleOutput,
    AmbiguousIdentity,
}
#[derive(Debug, Clone, PartialEq)]
pub enum OutputDecision<'a> {
    Monitor(&'a OutputSnapshot),
    Silent(SilentOutput),
}

pub fn resolve_output<'a>(
    choice: &str,
    outputs: &'a [OutputSnapshot],
    default: Option<&str>,
) -> Result<OutputDecision<'a>> {
    resolve_output_with_wave(choice, outputs, default, None)
}

/// A ready, exact capture/headphone pairing can supply the preferred Automatic sink.
/// Explicit choices ignore this preference and remain subject to the runtime readiness gate.
pub fn resolve_output_with_wave<'a>(
    choice: &str,
    outputs: &'a [OutputSnapshot],
    default: Option<&str>,
    preferred_wave: Option<&str>,
) -> Result<OutputDecision<'a>> {
    validate_output_choice(choice)?;
    if choice == "none" {
        return Ok(OutputDecision::Silent(SilentOutput::NotMonitored));
    }
    let exact = |name: &str| {
        let mut matches = outputs
            .iter()
            .filter(|o| eligible_output(o) && o.node_name == name);
        let first = matches.next();
        if matches.next().is_some() {
            Err(SilentOutput::AmbiguousIdentity)
        } else {
            Ok(first)
        }
    };
    let selected = if choice != "auto" {
        match exact(choice) {
            Ok(Some(output)) => return Ok(OutputDecision::Monitor(output)),
            Ok(None) => return Ok(OutputDecision::Silent(SilentOutput::MissingExplicit)),
            Err(reason) => return Ok(OutputDecision::Silent(reason)),
        }
    } else {
        let wave = preferred_wave
            .and_then(|name| {
                outputs
                    .iter()
                    .find(|o| eligible_output(o) && o.is_wave && o.node_name == name)
            })
            .or_else(|| {
                outputs
                    .iter()
                    .filter(|o| eligible_output(o) && o.is_wave)
                    .min_by_key(|o| &o.node_name)
            });
        wave.or_else(|| {
            default.and_then(|name| {
                outputs
                    .iter()
                    .find(|o| eligible_output(o) && o.node_name == name)
            })
        })
        .or_else(|| {
            outputs
                .iter()
                .filter(|o| eligible_output(o))
                .min_by(|a, b| {
                    b.priority
                        .cmp(&a.priority)
                        .then_with(|| a.node_name.cmp(&b.node_name))
                })
        })
    };
    Ok(match selected {
        Some(output) => match exact(&output.node_name) {
            Ok(Some(output)) => OutputDecision::Monitor(output),
            Err(reason) => OutputDecision::Silent(reason),
            Ok(None) => unreachable!(),
        },
        None => OutputDecision::Silent(SilentOutput::NoEligibleOutput),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MuteChange {
    pub source: SourceId,
    pub muted: bool,
}

/// Mutates one candidate model. Publish/persist that candidate as one revision;
/// hardware and graph consumers must execute returned silences before openings.
pub fn set_source_muted(
    sources: &mut Sources,
    target: &SourceId,
    muted: bool,
) -> Result<Vec<MuteChange>> {
    let group = sources
        .get(target)
        .ok_or_else(|| OperationError::invalid("Unknown source"))?
        .group
        .clone();
    let mut changes = Vec::new();
    if !muted && !group.is_empty() {
        for (id, source) in sources.iter_mut() {
            if id != target && source.group == group && !source.muted {
                source.muted = true;
                changes.push(MuteChange {
                    source: id.clone(),
                    muted: true,
                });
            }
        }
    }
    let source = sources.get_mut(target).expect("validated source");
    if source.muted != muted {
        source.muted = muted;
        changes.push(MuteChange {
            source: target.clone(),
            muted,
        });
    }
    Ok(changes)
}

pub fn source_groups(sources: &Sources) -> Vec<String> {
    let mut counts = BTreeMap::new();
    for source in sources.values().filter(|s| !s.group.is_empty()) {
        *counts.entry(&source.group).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .filter(|(_, count)| *count >= 2)
        .map(|(name, _)| name.clone())
        .collect()
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupSwitch {
    pub source: SourceId,
    pub changes: Vec<MuteChange>,
}
pub fn switch_group(sources: &mut Sources, group: &str) -> Result<Option<GroupSwitch>> {
    if group.is_empty() {
        return Ok(None);
    }
    let members: Vec<_> = sources
        .iter()
        .filter(|(_, source)| source.group == group)
        .map(|(id, source)| (id.clone(), source.muted))
        .collect();
    if members.len() < 2 {
        return Ok(None);
    }
    let next = members
        .iter()
        .position(|(_, muted)| !muted)
        .map_or(0, |index| (index + 1) % members.len());
    let source = members[next].0.clone();
    let changes = set_source_muted(sources, &source, false)?;
    Ok(Some(GroupSwitch { source, changes }))
}

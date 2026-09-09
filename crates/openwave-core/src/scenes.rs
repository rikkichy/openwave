use std::fmt;

use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::model::{ErrorCode, OperationError, OperationIssue, Result, UnitId};
use crate::profiles::ProfileId;

/// Existing store IDs are opaque nonempty strings, not newly generated slugs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct SceneId(String);

impl SceneId {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.is_empty() {
            return Err(OperationError::invalid("scene ID must not be empty"));
        }
        Ok(Self(value))
    }

    pub fn from_name(name: &str) -> Self {
        let mut slug = String::new();
        let mut separator = false;
        // Lowercase first, as in the original slugger (e.g. Kelvin sign -> k).
        for ch in name.to_lowercase().chars() {
            if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
                if separator && !slug.is_empty() {
                    slug.push('-');
                }
                slug.push(ch);
                separator = false;
            } else {
                separator = true;
            }
        }
        Self(if slug.is_empty() {
            "scene".into()
        } else {
            slug
        })
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SceneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for SceneId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SourcePatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub muted: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct LevelPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub muted: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct HardwarePatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gain_raw: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mute: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hp_volume_db: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub low_impedance: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monitor_mix: Option<u16>,
}

/// Optional sections distinguish absence from an explicitly saved empty map.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Scene {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sources: Option<IndexMap<String, SourcePatch>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cells: Option<IndexMap<String, LevelPatch>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outputs: Option<IndexMap<String, Option<String>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volumes: Option<IndexMap<String, LevelPatch>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hardware: Option<IndexMap<String, HardwarePatch>>,
}

fn mapping(value: Value, label: &str) -> Result<Map<String, Value>> {
    match value {
        Value::Object(map) if map.keys().all(|key| !key.is_empty()) => Ok(map),
        _ => Err(OperationError::invalid(format!(
            "{label} must be a map with nonempty string keys"
        ))),
    }
}

fn unsupported(map: &Map<String, Value>, label: &str) -> Result<()> {
    if map.is_empty() {
        Ok(())
    } else {
        Err(OperationError::invalid(format!(
            "{label} has unsupported fields"
        )))
    }
}

fn finite(value: f64, low: f64, high: f64, label: &str) -> Result<()> {
    if value.is_finite() && (low..=high).contains(&value) {
        Ok(())
    } else {
        Err(OperationError::invalid(format!(
            "{label} must be a finite number in [{low}, {high}]"
        )))
    }
}

fn number(map: &mut Map<String, Value>, key: &str, low: f64, high: f64) -> Result<Option<f64>> {
    let Some(value) = map.remove(key) else {
        return Ok(None);
    };
    let value = value
        .as_f64()
        .ok_or_else(|| OperationError::invalid(format!("{key} must be a number")))?;
    finite(value, low, high, key)?;
    Ok(Some(value))
}

fn integer(map: &mut Map<String, Value>, key: &str) -> Result<Option<u16>> {
    let Some(value) = map.remove(key) else {
        return Ok(None);
    };
    let value = value
        .as_u64()
        .and_then(|v| u16::try_from(v).ok())
        .ok_or_else(|| {
            OperationError::invalid(format!("{key} must be an integer in [0, 65535]"))
        })?;
    Ok(Some(value))
}

fn boolean(map: &mut Map<String, Value>, key: &str) -> Result<Option<bool>> {
    match map.remove(key) {
        None => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(value)),
        _ => Err(OperationError::invalid(format!("{key} must be boolean"))),
    }
}

impl SourcePatch {
    pub fn from_value(value: Value) -> Result<Self> {
        let mut map = mapping(value, "source patch")?;
        let result = Self {
            level: number(&mut map, "level", 0.0, 1.0)?,
            muted: boolean(&mut map, "muted")?,
        };
        unsupported(&map, "source patch")?;
        Ok(result)
    }

    pub fn validate(&self) -> Result<()> {
        if let Some(value) = self.level {
            finite(value, 0.0, 1.0, "level")?;
        }
        Ok(())
    }
}

impl LevelPatch {
    pub fn from_value(value: Value) -> Result<Self> {
        let mut map = mapping(value, "level patch")?;
        let result = Self {
            volume: number(&mut map, "volume", 0.0, 1.0)?,
            muted: boolean(&mut map, "muted")?,
        };
        unsupported(&map, "level patch")?;
        Ok(result)
    }

    pub fn validate(&self) -> Result<()> {
        if let Some(value) = self.volume {
            finite(value, 0.0, 1.0, "volume")?;
        }
        Ok(())
    }
}

impl HardwarePatch {
    pub fn from_value(value: Value) -> Result<Self> {
        let mut map = mapping(value, "hardware patch")?;
        let result = Self {
            gain_raw: integer(&mut map, "gain_raw")?,
            mute: boolean(&mut map, "mute")?,
            hp_volume_db: number(&mut map, "hp_volume_db", -128.0, 0.0)?,
            low_impedance: boolean(&mut map, "low_impedance")?,
            monitor_mix: integer(&mut map, "monitor_mix")?,
        };
        unsupported(&map, "hardware patch")?;
        Ok(result)
    }

    pub fn validate(&self) -> Result<()> {
        if let Some(value) = self.hp_volume_db {
            finite(value, -128.0, 0.0, "hp_volume_db")?;
        }
        Ok(())
    }
}

fn section<T>(
    map: &mut Map<String, Value>,
    key: &str,
    decode: impl Fn(Value) -> Result<T>,
) -> Result<Option<IndexMap<String, T>>> {
    let Some(value) = map.remove(key) else {
        return Ok(None);
    };
    mapping(value, key)?
        .into_iter()
        .map(|(key, value)| Ok((key, decode(value)?)))
        .collect::<Result<_>>()
        .map(Some)
}

fn cell_key(key: &str) -> Result<()> {
    match key.rsplit_once('.') {
        Some((source, mix)) if !source.is_empty() && !mix.is_empty() => Ok(()),
        _ => Err(OperationError::invalid(format!(
            "cell {key} needs a source.mix key"
        ))),
    }
}

fn validate_section<T>(
    section: &Option<IndexMap<String, T>>,
    validate: impl Fn(&str, &T) -> Result<()>,
) -> Result<()> {
    if let Some(entries) = section {
        for (key, value) in entries {
            if key.is_empty() {
                return Err(OperationError::invalid(
                    "scene section key must not be empty",
                ));
            }
            validate(key, value)?;
        }
    }
    Ok(())
}

impl Scene {
    pub fn from_value(value: Value) -> Result<Self> {
        let mut map = mapping(value, "scene")?;
        let name = match map.remove("name") {
            Some(Value::String(name)) => name,
            _ => return Err(OperationError::invalid("scene needs a string name")),
        };
        let result = Self {
            name,
            sources: section(&mut map, "sources", SourcePatch::from_value)?,
            cells: section(&mut map, "cells", LevelPatch::from_value)?,
            outputs: section(&mut map, "outputs", |value| match value {
                Value::Null => Ok(None),
                Value::String(value) => Ok(Some(value)),
                _ => Err(OperationError::invalid(
                    "output must be a sink name or null",
                )),
            })?,
            volumes: section(&mut map, "volumes", LevelPatch::from_value)?,
            hardware: section(&mut map, "hardware", HardwarePatch::from_value)?,
        };
        unsupported(&map, "scene")?;
        result.validate()?;
        Ok(result)
    }

    /// Call before publication as well as persistence: public fields may be edited.
    /// Profile bounds and live source/mix references are checked during recall.
    pub fn validate(&self) -> Result<()> {
        validate_section(&self.sources, |_, patch| patch.validate())?;
        validate_section(&self.cells, |key, patch| {
            cell_key(key)?;
            patch.validate()
        })?;
        validate_section(&self.outputs, |_, _| Ok(()))?;
        validate_section(&self.volumes, |_, patch| patch.validate())?;
        validate_section(&self.hardware, |_, patch| patch.validate())
    }
}

macro_rules! validated_deserialize {
    ($($ty:ty),+ $(,)?) => {$(
        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
                Self::from_value(Value::deserialize(deserializer)?).map_err(serde::de::Error::custom)
            }
        }
    )+};
}
validated_deserialize!(Scene, SourcePatch, LevelPatch, HardwarePatch);

pub fn decode_store(value: Value) -> Result<IndexMap<SceneId, Scene>> {
    let mut root = mapping(value, "scene store")?;
    let scenes = root
        .remove("scenes")
        .ok_or_else(|| OperationError::invalid("scene store needs scenes"))?;
    unsupported(&root, "scene store")?;
    mapping(scenes, "scenes")?
        .into_iter()
        .map(|(key, value)| Ok((SceneId::new(key)?, Scene::from_value(value)?)))
        .collect()
}

pub fn encode_store(scenes: &IndexMap<SceneId, Scene>) -> Result<Value> {
    for scene in scenes.values() {
        scene.validate()?;
    }
    let mut root = Map::new();
    root.insert("scenes".into(), serde_json::to_value(scenes)?);
    Ok(Value::Object(root))
}

pub fn hardware_key(profile: ProfileId, serial: &str) -> String {
    if serial.is_empty() {
        profile.as_str().into()
    } else {
        format!("{}:{serial}", profile.as_str())
    }
}

/// Supply all connected units, including those lacking serial information.
#[derive(Debug, Clone, Copy)]
pub struct HardwareCandidate<'a> {
    pub unit: UnitId,
    pub serial: &'a str,
}

/// Select only an exact unique serial or an unambiguous single-model legacy entry.
/// The captured unit must itself still be present in the candidate snapshot.
pub fn pick_hardware_entry<'a>(
    hardware: &'a IndexMap<String, HardwarePatch>,
    unit: UnitId,
    candidates: &[HardwareCandidate<'_>],
) -> Result<Option<(&'a str, &'a HardwarePatch)>> {
    let mut target = candidates.iter().filter(|candidate| candidate.unit == unit);
    let Some(candidate) = target.next() else {
        return Err(OperationError::new(
            ErrorCode::Identity,
            "scene unit is no longer connected",
        ));
    };
    if target.next().is_some()
        || candidates
            .iter()
            .filter(|other| other.unit.profile == unit.profile && other.serial == candidate.serial)
            .count()
            != 1
    {
        return Err(OperationError::new(
            ErrorCode::Identity,
            "scene hardware identity is ambiguous",
        ));
    }
    if !candidate.serial.is_empty() {
        if let Some((key, patch)) =
            hardware.get_key_value(&hardware_key(unit.profile, candidate.serial))
        {
            return Ok(Some((key.as_str(), patch)));
        }
    }
    if candidates
        .iter()
        .filter(|other| other.unit.profile == unit.profile)
        .count()
        == 1
    {
        return Ok(hardware
            .get_key_value(unit.profile.as_str())
            .map(|(key, patch)| (key.as_str(), patch)));
    }
    Ok(None)
}

#[derive(Debug)]
pub struct HardwareSelection<'a> {
    pub unit: UnitId,
    pub key: &'a str,
    pub patch: &'a HardwarePatch,
}

#[derive(Debug)]
pub struct HardwareResolution<'a> {
    pub selected: Vec<HardwareSelection<'a>>,
    pub skipped: Vec<OperationIssue>,
}

/// Unmatched entries (including overridden legacy entries) are explicit skips.
/// Completion, capability/gain-lock checks and writes belong to the controller.
pub fn resolve_hardware<'a>(
    hardware: &'a IndexMap<String, HardwarePatch>,
    candidates: &[HardwareCandidate<'_>],
) -> HardwareResolution<'a> {
    let selected: Vec<_> = candidates
        .iter()
        .filter_map(|candidate| {
            pick_hardware_entry(hardware, candidate.unit, candidates)
                .ok()
                .flatten()
                .map(|(key, patch)| HardwareSelection {
                    unit: candidate.unit,
                    key,
                    patch,
                })
        })
        .collect();
    let skipped = hardware.keys().filter(|key| !selected.iter().any(|entry| entry.key == key.as_str()))
        .map(|key| OperationIssue {
            target: format!("hardware {key}"),
            message: "No unique connected target selected; entry is missing, ambiguous or superseded by an exact serial entry".into(),
        }).collect();
    HardwareResolution { selected, skipped }
}

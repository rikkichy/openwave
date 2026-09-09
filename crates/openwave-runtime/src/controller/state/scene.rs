use super::*;
use scenes::{HardwareCandidate, HardwarePatch, LevelPatch, Scene, SourcePatch};

impl Controller {
    pub(super) fn save_scene(&mut self, name: &str) -> Result<()> {
        let name = name.trim();
        if name.is_empty() {
            return Err(OperationError::invalid("A scene needs a name"));
        }
        self.store.sources.require_writable()?;
        self.store.mixes.require_writable()?;
        self.store.matrix.require_writable()?;
        self.store.scenes.require_writable()?;
        let sources = self.store.sources.value();
        let mixes = self.store.mixes.value();
        let matrix = self.store.matrix.value();
        let mut cells = IndexMap::new();
        for source in sources.keys() {
            for mix in mixes.keys() {
                let cell = matrix.cell(source, mix);
                cells.insert(
                    format!("{source}.{mix}"),
                    LevelPatch {
                        volume: Some(cell.volume),
                        muted: Some(cell.muted),
                    },
                );
            }
        }
        let mut identities = HashMap::new();
        for unit in self.units.values() {
            *identities
                .entry((unit.id.profile, unit.info.serial.as_str()))
                .or_insert(0) += 1;
        }
        let hardware = self
            .units
            .values()
            .filter_map(|unit| {
                if identities[&(unit.id.profile, unit.info.serial.as_str())] != 1 {
                    return None;
                }
                let profile = unit.id.profile.profile();
                let patch = unit
                    .state
                    .known()
                    .map(|state| HardwarePatch {
                        gain_raw: Some(state.gain_raw),
                        mute: Some(state.muted),
                        hp_volume_db: Some(state.hp_volume_db),
                        low_impedance: if profile.has_low_z() {
                            state.low_impedance
                        } else {
                            None
                        },
                        monitor_mix: if profile.has_monitor_mix() {
                            state.monitor_mix
                        } else {
                            None
                        },
                    })
                    .unwrap_or_default();
                Some((
                    scenes::hardware_key(unit.id.profile, &unit.info.serial),
                    patch,
                ))
            })
            .collect();
        let scene = Scene {
            name: name.into(),
            sources: Some(
                sources
                    .iter()
                    .map(|(id, source)| {
                        (
                            id.to_string(),
                            SourcePatch {
                                level: Some(source.level),
                                muted: Some(source.muted),
                            },
                        )
                    })
                    .collect(),
            ),
            cells: Some(cells),
            outputs: Some(
                mixes
                    .keys()
                    .map(|id| (id.to_string(), Some(matrix.output(id).into())))
                    .collect(),
            ),
            volumes: Some(
                matrix
                    .volumes
                    .iter()
                    .filter(|(id, _)| mixes.contains_key(*id))
                    .map(|(id, value)| {
                        (
                            id.to_string(),
                            LevelPatch {
                                volume: Some(value.volume),
                                muted: Some(value.muted),
                            },
                        )
                    })
                    .collect(),
            ),
            hardware: Some(hardware),
        };
        scene.validate()?;
        let mut saved = self.store.scenes.value().clone();
        saved.insert(SceneId::from_name(name), scene);
        self.store.scenes.replace(saved)?;
        self.refresh_desired();
        self.route();
        Ok(())
    }

    pub(super) fn delete_scene(&mut self, id: &SceneId) -> Result<()> {
        self.store.scenes.require_writable()?;
        let mut saved = self.store.scenes.value().clone();
        if saved.shift_remove(id).is_none() {
            return Err(OperationError::invalid(format!("Unknown scene: {id}")));
        }
        self.store.scenes.replace(saved)?;
        self.refresh_desired();
        self.route();
        Ok(())
    }

    pub(super) fn apply_scene(&mut self, command: CommandId, id: &SceneId) -> Result<()> {
        self.store.scenes.require_writable()?;
        let scene = self
            .store
            .scenes
            .value()
            .get(id)
            .cloned()
            .ok_or_else(|| OperationError::invalid(format!("Unknown scene: {id}")))?;
        scene.validate()?;
        let request = self
            .requests
            .get_mut(&command)
            .ok_or_else(|| OperationError::invalid("Scene request is not accepted"))?;
        request.scene = true;
        self.view.scene_outcome = None;
        self.view.scene_pending = true;
        self.dirty = true;

        let empty = IndexMap::new();
        let hardware = scene.hardware.as_ref().unwrap_or(&empty);
        let candidates: Vec<_> = self
            .units
            .values()
            .map(|unit| HardwareCandidate {
                unit: unit.id,
                serial: &unit.info.serial,
            })
            .collect();
        let resolved = scenes::resolve_hardware(hardware, &candidates);
        let mut skipped = resolved.skipped;
        let excluded: HashSet<_> = resolved.selected.iter().map(|entry| entry.unit).collect();
        let mut patches = scene.sources.unwrap_or_default();
        for entry in &resolved.selected {
            if let Some(muted) = entry.patch.mute {
                for source in self.unit_sources(entry.unit) {
                    patches
                        .entry(source.to_string())
                        .or_default()
                        .muted
                        .get_or_insert(muted);
                }
            }
        }

        // Publish only a successfully saved candidate. Hardware mirrors below use
        // that committed outcome, including when a source-store write fails.
        let before = self.store.sources.value().clone();
        if !patches.is_empty() {
            let result = self.store.sources.require_writable().and_then(|()| {
                let mut sources = before.clone();
                for (key, patch) in &patches {
                    let Some(source) = SourceId::new(key.clone())
                        .ok()
                        .filter(|id| sources.contains_key(id))
                    else {
                        skipped.push(OperationIssue {
                            target: format!("source {key}"),
                            message: "Source no longer exists".into(),
                        });
                        continue;
                    };
                    if let Some(level) = patch.level {
                        sources.get_mut(&source).expect("existing source").level = level;
                    }
                    if let Some(muted) = patch.muted {
                        routing::set_source_muted(&mut sources, &source, muted)?;
                    }
                }
                for (key, patch) in &patches {
                    if patch.muted == Some(false)
                        && SourceId::new(key.clone())
                            .ok()
                            .and_then(|id| sources.get(&id))
                            .is_some_and(|source| source.muted)
                    {
                        skipped.push(OperationIssue {
                            target: format!("source {key}"),
                            message: "A later recalled group member is open".into(),
                        });
                    }
                }
                if sources != before {
                    self.commit_sources(sources)?;
                }
                Ok(())
            });
            if let Err(error) = result {
                self.operation_failure(Some(command), "source persistence".into(), error);
            }
        }

        let mut superseded = HashSet::new();
        for entry in &resolved.selected {
            for (present, field) in [
                (entry.patch.gain_raw.is_some(), SettingField::Gain),
                (entry.patch.mute.is_some(), SettingField::Mute),
                (entry.patch.hp_volume_db.is_some(), SettingField::Headphones),
                (
                    entry.patch.low_impedance.is_some(),
                    SettingField::LowImpedance,
                ),
                (entry.patch.monitor_mix.is_some(), SettingField::Monitor),
            ] {
                if present {
                    superseded.insert(PendingKey::Device(entry.unit, field));
                }
            }
        }
        for (source, patch) in &patches {
            if patch.muted.is_some() {
                if let Some(unit) = SourceId::new(source.clone())
                    .ok()
                    .and_then(|id| self.store.sources.value().get(&id))
                    .and_then(|source| self.bound_unit(source))
                {
                    superseded.insert(PendingKey::Device(unit, SettingField::Mute));
                }
            }
        }
        for (id, source) in self.store.sources.value() {
            if before.get(id).is_some_and(|old| old.muted != source.muted) {
                if let Some(unit) = self.bound_unit(source) {
                    superseded.insert(PendingKey::Device(unit, SettingField::Mute));
                }
            }
        }
        if let Some(cells) = &scene.cells {
            for key in cells.keys() {
                if let Ok((source, mix)) = parse_cell_key(key) {
                    superseded.insert(PendingKey::Cell(source, mix));
                }
            }
        }
        self.cancel_pending(|key| superseded.contains(key));

        let has_matrix = scene.cells.as_ref().is_some_and(|v| !v.is_empty())
            || scene.outputs.as_ref().is_some_and(|v| !v.is_empty())
            || scene.volumes.as_ref().is_some_and(|v| !v.is_empty());
        if has_matrix {
            let result = self
                .store
                .matrix
                .require_writable()
                .and_then(|()| self.store.mixes.require_writable())
                .and_then(|()| {
                    let mut matrix = self.store.matrix.value().clone();
                    if let Some(cells) = &scene.cells {
                        if !cells.is_empty() {
                            self.store.sources.require_writable()?;
                        }
                        for (key, patch) in cells {
                            let existing = parse_cell_key(key).ok().is_some_and(|(source, mix)| {
                                self.store.sources.value().contains_key(&source)
                                    && self.store.mixes.value().contains_key(&mix)
                            });
                            if !existing {
                                skipped.push(OperationIssue {
                                    target: format!("cell {key}"),
                                    message: "Source or mix no longer exists".into(),
                                });
                                continue;
                            }
                            let cell = matrix.cells.entry(key.clone()).or_default();
                            if let Some(volume) = patch.volume {
                                cell.volume = volume;
                            }
                            if let Some(muted) = patch.muted {
                                cell.muted = muted;
                            }
                        }
                    }
                    if let Some(outputs) = &scene.outputs {
                        for (key, choice) in outputs {
                            let mix = MixId::new(key.clone())
                                .ok()
                                .filter(|id| self.store.mixes.value().contains_key(id));
                            let choice = choice
                                .as_deref()
                                .filter(|choice| !choice.is_empty())
                                .unwrap_or("auto");
                            if let Some(mix) = mix.filter(|_| !choice.starts_with("openwave_")) {
                                matrix.outputs.insert(mix, choice.into());
                            } else {
                                skipped.push(OperationIssue {
                                    target: format!("output {key}"),
                                    message: "Mix is missing or output is an OpenWave virtual node"
                                        .into(),
                                });
                            }
                        }
                    }
                    if let Some(volumes) = &scene.volumes {
                        for (key, patch) in volumes {
                            let Some(mix) = MixId::new(key.clone())
                                .ok()
                                .filter(|id| self.store.mixes.value().contains_key(id))
                            else {
                                skipped.push(OperationIssue {
                                    target: format!("volume {key}"),
                                    message: "Mix no longer exists".into(),
                                });
                                continue;
                            };
                            let master = matrix.volumes.entry(mix).or_insert_with(|| LevelState {
                                volume: 1.0,
                                ..LevelState::default()
                            });
                            if let Some(volume) = patch.volume {
                                master.volume = volume;
                            }
                            if let Some(muted) = patch.muted {
                                master.muted = muted;
                            }
                        }
                    }
                    if &matrix != self.store.matrix.value() {
                        self.commit_matrix(matrix)?;
                    }
                    Ok(())
                });
            if let Err(error) = result {
                self.operation_failure(Some(command), "matrix persistence".into(), error);
            }
        }

        self.sync_mutes_excluding(
            &before,
            &Origin::Scene(id.clone()),
            Some(command),
            &excluded,
        );
        for entry in resolved.selected {
            let unit = entry.unit;
            let profile = unit.profile.profile();
            let patch = entry.patch;
            let mut settings = Vec::with_capacity(5);
            let mut skip = |field: &str, message: &str| {
                skipped.push(OperationIssue {
                    target: format!("hardware {}: {field}", entry.key),
                    message: message.into(),
                })
            };
            if let Some(gain) = patch.gain_raw {
                if self.view.selected_unit == Some(unit) && self.view.preferences.gain_locked {
                    skip("gain", "Selected device gain is locked");
                } else if gain > profile.gain_max {
                    skip("gain", "Gain exceeds profile range");
                } else {
                    settings.push(DeviceSetting::GainRaw(gain));
                }
            }
            let bound = self.unit_sources(unit);
            let changed = bound.iter().any(|id| {
                before
                    .get(id)
                    .is_some_and(|old| old.muted != self.store.sources.value()[id].muted)
            });
            if patch.mute.is_some() || changed {
                let muted = if bound.is_empty() {
                    patch.mute
                } else {
                    Some(bound.iter().all(|id| self.store.sources.value()[id].muted))
                };
                if let Some(muted) = muted {
                    settings.push(DeviceSetting::Mute(muted));
                }
            }
            if let Some(db) = patch.hp_volume_db {
                settings.push(DeviceSetting::HeadphoneDb(db));
            }
            if let Some(low_z) = patch.low_impedance {
                if profile.has_low_z() {
                    settings.push(DeviceSetting::LowImpedance(low_z));
                } else {
                    skip("low impedance", "Unsupported by this profile");
                }
            }
            if let Some(monitor) = patch.monitor_mix {
                if !profile.has_monitor_mix() || monitor > profile.mix_max {
                    skip("monitor mix", "Unsupported or outside profile range");
                } else {
                    settings.push(DeviceSetting::MonitorMix(monitor));
                }
            }
            if let Err(error) = self.queue_device(Some(command), unit, settings) {
                self.operation_failure(Some(command), format!("hardware {}", entry.key), error);
            }
        }
        if let Some(request) = self.requests.get_mut(&command) {
            request.skipped.extend(skipped);
        }
        Ok(())
    }
}

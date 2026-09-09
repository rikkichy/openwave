use super::*;
use openwave_core::{effects, model::SourceKind};

impl Controller {
    pub(super) fn apply(&mut self, id: CommandId, command: AppCommand) {
        if self.frozen
            && !matches!(
                command,
                AppCommand::Shutdown
                    | AppCommand::PrepareUninstall { .. }
                    | AppCommand::ConfirmUninstall { .. }
            )
        {
            self.finish(id, CommandOutcome::Cancelled);
            return;
        }
        if let Err(error) = self.execute(id, command) {
            if let Some(request) = self.requests.get_mut(&id) {
                request.waiting = false;
            }
            self.operation_failure(Some(id), "command".into(), error);
        }
        self.advance_handovers();
        self.finish_if_ready(id);
    }
    fn execute(&mut self, id: CommandId, command: AppCommand) -> Result<()> {
        match command {
            AppCommand::AddSource { source } => {
                self.store.sources.require_writable()?;
                if self.store.sources.value().contains_key(&source.id) {
                    return Err(OperationError::invalid("Source identity already exists"));
                }
                if source.name.trim().is_empty() {
                    return Err(OperationError::invalid("Source needs a name"));
                }
                let before = self.store.sources.value().clone();
                let mut candidate = before.clone();
                candidate.insert(source.id.clone(), source);
                self.commit_sources(candidate)?;
                self.sync_mutes(&before, &Origin::User, Some(id));
            }
            AppCommand::EditSource { source, changes } => {
                let before = self.store.sources.value().clone();
                let mut candidate = before.clone();
                let old = before
                    .get(&source)
                    .ok_or_else(|| OperationError::invalid("Unknown source"))?;
                let group = changes.group.as_ref().map(|group| group.trim().to_owned());
                let joining_live_group = group.as_ref().is_some_and(|group| {
                    !group.is_empty()
                        && *group != old.group
                        && before.iter().any(|(peer, record)| {
                            peer != &source && record.group == *group && !record.muted
                        })
                });
                let record = candidate
                    .get_mut(&source)
                    .ok_or_else(|| OperationError::invalid("Unknown source"))?;
                if let Some(name) = changes.name {
                    if name.trim().is_empty() {
                        return Err(OperationError::invalid("Source needs a name"));
                    }
                    record.name = name;
                }
                if let Some(icon) = changes.icon_name {
                    record.icon_name = icon;
                }
                if let Some(bindings) = changes.match_app_names {
                    if record.kind != SourceKind::App {
                        return Err(OperationError::invalid(
                            "Capture sources cannot have application bindings",
                        ));
                    }
                    record.match_app_names = normalize_bindings(bindings);
                }
                if let Some(node) = changes.node_name {
                    if record.kind != SourceKind::Device {
                        return Err(OperationError::invalid(
                            "Application sources cannot have capture bindings",
                        ));
                    }
                    record.node_name = node;
                }
                if let Some(catch_all) = changes.catch_all {
                    record.extra.insert("catch_all".into(), catch_all.into());
                }
                if let Some(group) = group {
                    record.group = group;
                }
                if joining_live_group {
                    record.muted = true;
                }
                self.cancel_pending(
                    |key| matches!(key, PendingKey::Fx(target) if *target == source),
                );
                self.commit_sources(candidate)?;
                self.sync_mutes(&before, &Origin::User, Some(id));
            }
            AppCommand::RemoveSource { source } => self.remove_source(&source)?,
            AppCommand::OrderSources { order } => {
                let mut remaining = self.store.sources.value().clone();
                let mut ordered = Sources::new();
                for id in order {
                    if let Some(source) = remaining.shift_remove(&id) {
                        ordered.insert(id, source);
                    }
                }
                ordered.extend(remaining);
                self.commit_sources(ordered)?;
            }
            AppCommand::AddMix { mix } => {
                if self.store.mixes.value().contains_key(&mix.id) {
                    return Err(OperationError::invalid("Mix identity already exists"));
                }
                if mix.name.trim().is_empty() {
                    return Err(OperationError::invalid("Mix needs a name"));
                }
                let mut mixes = self.store.mixes.value().clone();
                mixes.insert(mix.id.clone(), mix);
                self.store.mixes.replace(mixes)?;
                self.refresh_desired();
                self.route();
            }
            AppCommand::EditMix { mix, changes } => {
                let mut mixes = self.store.mixes.value().clone();
                let record = mixes
                    .get_mut(&mix)
                    .ok_or_else(|| OperationError::invalid("Unknown mix"))?;
                if let Some(name) = changes.name {
                    if name.trim().is_empty() {
                        return Err(OperationError::invalid("Mix needs a name"));
                    }
                    record.name = name;
                }
                if let Some(description) = changes.description {
                    record.description = description;
                }
                if let Some(subtitle) = changes.subtitle {
                    record.subtitle = subtitle;
                }
                if let Some(icon) = changes.icon_name {
                    record.icon_name = icon;
                }
                self.store.mixes.replace(mixes)?;
                self.refresh_desired();
                self.route();
            }
            AppCommand::RemoveMix { mix } => self.remove_mix(&mix)?,
            AppCommand::OrderMixes { order } => {
                let mut remaining = self.store.mixes.value().clone();
                let mut ordered = Mixes::new();
                for id in order {
                    if let Some(mix) = remaining.shift_remove(&id) {
                        ordered.insert(id, mix);
                    }
                }
                ordered.extend(remaining);
                self.store.mixes.replace(ordered)?;
                self.refresh_desired();
                self.route();
            }
            AppCommand::SetSourceLevel { source, level } => {
                let level = command_level(level)?;
                let mut sources = self.store.sources.value().clone();
                sources
                    .get_mut(&source)
                    .ok_or_else(|| OperationError::invalid("Unknown source"))?
                    .level = level;
                self.commit_sources(sources)?;
            }
            AppCommand::SetSourceMute { source, muted } => {
                self.set_source_mute(&source, muted, Origin::User, Some(id))?
            }
            AppCommand::ToggleSourceMute { source } => {
                let muted = !self
                    .store
                    .sources
                    .value()
                    .get(&source)
                    .ok_or_else(|| OperationError::invalid("Unknown source"))?
                    .muted;
                self.set_source_mute(&source, muted, Origin::User, Some(id))?;
            }
            AppCommand::SetCell {
                source,
                mix,
                level,
                muted,
                timing,
            } => {
                self.require_cell(&source, &mix)?;
                let level = send_level(level)?;
                let key = PendingKey::Cell(source.clone(), mix.clone());
                if timing == EditTiming::Debounced {
                    self.defer(
                        key,
                        id,
                        AppCommand::SetCell {
                            source,
                            mix,
                            level,
                            muted,
                            timing: EditTiming::Immediate,
                        },
                        Duration::from_millis(150),
                    );
                } else {
                    self.cancel_pending(|old| *old == key);
                    let mut matrix = self.store.matrix.value().clone();
                    let cell = matrix.cells.entry(format!("{source}.{mix}")).or_default();
                    cell.volume = level;
                    cell.muted = muted;
                    self.commit_matrix(matrix)?;
                }
            }
            AppCommand::SetCellLevel { source, mix, level } => {
                self.require_cell(&source, &mix)?;
                let level = send_level(level)?;
                self.cancel_pending(
                    |key| matches!(key, PendingKey::Cell(s, m) if *s == source && *m == mix),
                );
                let mut matrix = self.store.matrix.value().clone();
                matrix
                    .cells
                    .entry(format!("{source}.{mix}"))
                    .or_default()
                    .volume = level;
                self.commit_matrix(matrix)?;
            }
            AppCommand::ToggleCellMute { source, mix } => {
                self.require_cell(&source, &mix)?;
                let key = PendingKey::Cell(source.clone(), mix.clone());
                let pending = self.pending.get(&key).and_then(|edit| match &edit.command {
                    AppCommand::SetCell { level, muted, .. } => Some((*level, *muted)),
                    _ => None,
                });
                self.cancel_pending(|old| *old == key);
                let mut matrix = self.store.matrix.value().clone();
                let cell = matrix.cells.entry(format!("{source}.{mix}")).or_default();
                if let Some((level, muted)) = pending {
                    cell.volume = level;
                    cell.muted = muted;
                }
                cell.muted = !cell.muted;
                self.commit_matrix(matrix)?;
            }
            AppCommand::SetOutput { mix, choice } => {
                self.require_mix(&mix)?;
                routing::validate_output_choice(&choice)?;
                let mut matrix = self.store.matrix.value().clone();
                matrix.outputs.insert(mix, choice);
                self.commit_matrix(matrix)?;
            }
            AppCommand::SetMaster { mix, level, muted } => {
                self.require_mix(&mix)?;
                let level = command_level(level)?;
                let mut matrix = self.store.matrix.value().clone();
                let master = matrix.volumes.entry(mix).or_insert_with(|| LevelState {
                    volume: 1.0,
                    ..LevelState::default()
                });
                master.volume = level;
                master.muted = muted;
                self.commit_matrix(matrix)?;
            }
            AppCommand::SetFx {
                source,
                settings,
                timing,
            } => {
                self.require_fx_source(&source)?;
                let settings = settings.validated()?;
                let key = PendingKey::Fx(source.clone());
                if timing == EditTiming::Debounced {
                    self.defer(
                        key,
                        id,
                        AppCommand::SetFx {
                            source,
                            settings,
                            timing: EditTiming::Immediate,
                        },
                        Duration::from_millis(400),
                    );
                } else {
                    self.cancel_pending(|old| *old == key);
                    self.set_source_fx(&source, settings)?;
                }
            }
            AppCommand::ToggleFx { source, effect } => {
                self.require_fx_source(&source)?;
                let settings = effects::toggle_fx(&self.store.sources.value()[&source], &effect)?;
                self.cancel_pending(
                    |key| matches!(key, PendingKey::Fx(target) if *target == source),
                );
                self.set_source_fx(&source, settings)?;
            }
            AppCommand::JoinGroup { source, target } => {
                if source == target {
                    return Err(OperationError::invalid("Cannot group a source with itself"));
                }
                let before = self.store.sources.value().clone();
                let target_source = before
                    .get(&target)
                    .ok_or_else(|| OperationError::invalid("Unknown target source"))?;
                let group = if !target_source.group.is_empty() {
                    target_source.group.clone()
                } else if !target_source.name.trim().is_empty() {
                    target_source.name.trim().to_owned()
                } else {
                    target.to_string()
                };
                let mut candidate = before.clone();
                candidate
                    .get_mut(&source)
                    .ok_or_else(|| OperationError::invalid("Unknown dragged source"))?
                    .group = group.clone();
                candidate
                    .get_mut(&target)
                    .ok_or_else(|| OperationError::invalid("Unknown target source"))?
                    .group = group;
                routing::set_source_muted(&mut candidate, &target, false)?;
                self.commit_sources(candidate)?;
                self.sync_mutes(&before, &Origin::User, Some(id));
            }
            AppCommand::LeaveGroup { source } => {
                let mut sources = self.store.sources.value().clone();
                sources
                    .get_mut(&source)
                    .ok_or_else(|| OperationError::invalid("Unknown source"))?
                    .group
                    .clear();
                self.commit_sources(sources)?;
            }
            AppCommand::SwitchGroup { group } => {
                let before = self.store.sources.value().clone();
                let mut candidate = before.clone();
                if routing::switch_group(&mut candidate, &group)?.is_some() {
                    self.commit_sources(candidate)?;
                    self.sync_mutes(&before, &Origin::User, Some(id));
                }
            }
            AppCommand::SelectUnit { unit } => {
                if unit.is_some_and(|unit| !self.units.contains_key(&unit)) {
                    return Err(OperationError::unavailable(
                        "Selected unit has disconnected",
                    ));
                }
                self.cancel_pending(|key| matches!(key, PendingKey::Device(..)));
                self.view.selected_unit = unit;
                self.dirty = true;
            }
            AppCommand::SetDeviceSetting {
                unit,
                setting,
                timing,
            } => self.device_setting(id, unit, setting, timing)?,
            AppCommand::ToggleDeviceMute { unit } => {
                let snapshot = self
                    .units
                    .get(&unit)
                    .ok_or_else(|| OperationError::unavailable("Selected unit has disconnected"))?;
                let sources = self.mute_sources_for_unit(unit);
                let muted = if sources.is_empty() {
                    snapshot
                        .desired_mute
                        .or_else(|| snapshot.state.known().map(|state| state.muted))
                        .ok_or_else(|| OperationError::unavailable("Device mute is unknown"))?
                } else {
                    sources
                        .iter()
                        .all(|source| self.store.sources.value()[source].muted)
                };
                self.device_setting(id, unit, DeviceSetting::Mute(!muted), EditTiming::Immediate)?;
            }
            AppCommand::SetGainLock { locked } => {
                let mut preferences = self.store.preferences.value().clone();
                preferences.gain_locked = locked;
                self.store.preferences.replace(preferences)?;
                if locked {
                    let selected = self.view.selected_unit;
                    self.cancel_pending(|key| matches!(key, PendingKey::Device(unit, SettingField::Gain) if Some(*unit) == selected));
                }
                self.view.preferences = Arc::new(self.store.preferences.value().clone());
                self.dirty = true;
            }
            AppCommand::SetPreferences { changes } => {
                let mut preferences = self.store.preferences.value().clone();
                if let Some(width) = changes.width {
                    preferences.width = width.max(820);
                }
                if let Some(height) = changes.height {
                    preferences.height = height.max(480);
                }
                if let Some(maximized) = changes.maximized {
                    preferences.maximized = maximized;
                }
                if let Some(offered) = changes.offered_capture_nodes {
                    preferences.offered_capture_nodes = offered;
                }
                if let Some(color) = changes.tray_icon_color {
                    preferences.tray_icon_color = color;
                }
                self.store.preferences.replace(preferences)?;
                self.view.preferences = Arc::new(self.store.preferences.value().clone());
                self.dirty = true;
            }
            AppCommand::SetAutostart { enabled, hidden } => {
                self.backend.dispatch(BackendCommand::Autostart {
                    job: id.0,
                    enabled,
                    hidden,
                })?;
                if let Some(request) = self.requests.get_mut(&id) {
                    request.waiting = true;
                }
            }
            AppCommand::SaveScene { name } => self.save_scene(&name)?,
            AppCommand::ApplyScene { scene } => self.apply_scene(id, &scene)?,
            AppCommand::DeleteScene { scene } => self.delete_scene(&scene)?,
            AppCommand::StartCalibration { source } => self.start_calibration(&source)?,
            AppCommand::RecordNoise { token } => self.record_calibration(id, &token, false)?,
            AppCommand::RecordSpeech { token } => self.record_calibration(id, &token, true)?,
            AppCommand::AcceptCalibration { token } => self.accept_calibration(&token)?,
            AppCommand::CancelCalibration { token } => self.cancel_calibration_token(&token)?,
            AppCommand::RunSetup => {
                self.store.mixes.require_writable()?;
                self.backend.dispatch(BackendCommand::Setup {
                    job: id.0,
                    mixes: self.store.mixes.value().clone(),
                })?;
                if let Some(request) = self.requests.get_mut(&id) {
                    request.waiting = true;
                }
                self.view.setup_phase = SetupPhase::Running;
                self.dirty = true;
            }
            AppCommand::ContinueSetup => {
                self.view.setup_phase = SetupPhase::Starting;
                self.dirty = true;
                self.publish();
                if let Err(error) = self.backend.dispatch(BackendCommand::Activate) {
                    self.view.setup_phase = SetupPhase::ActivationFailed(error.to_string());
                    self.dirty = true;
                    return Err(error);
                }
                self.view.setup_phase = SetupPhase::Ready;
                self.view.setup_required = false;
                self.dirty = true;
            }
            AppCommand::Reconnect => self.backend.dispatch(BackendCommand::Rescan)?,
            AppCommand::Shutdown => self.shutdown(id, None),
            AppCommand::PrepareUninstall { canonical_identity } => {
                if self.backend.identity().as_os_str() != std::ffi::OsStr::new(&canonical_identity)
                {
                    return Err(OperationError::new(
                        ErrorCode::Identity,
                        "Uninstall request targets a different installation",
                    ));
                }
                self.shutdown(id, None);
            }
            AppCommand::ConfirmUninstall {
                plan,
                delete_settings,
            } => {
                if plan.installation.canonical_identity != self.backend.identity() {
                    return Err(OperationError::new(
                        ErrorCode::Identity,
                        "Removal plan targets a different installation",
                    ));
                }
                if !plan.can_execute() {
                    return Err(OperationError::invalid(plan.blockers.join("\n")));
                }
                if let Err(error) = self.backend.prepare_removal(&plan, delete_settings) {
                    self.events.uninstall(UninstallResult {
                        success: false, removed: Vec::new(), error: Some(error.to_string()),
                        guidance: "Preparation failed before shutdown or removal; the running application was not frozen.".into(),
                        app_removed: false,
                    });
                    return Err(error);
                }
                self.shutdown(id, Some((plan, delete_settings)));
            }
        }
        Ok(())
    }
    fn require_mix(&self, mix: &MixId) -> Result<()> {
        self.store.mixes.require_writable()?;
        self.store.matrix.require_writable()?;
        if !self.store.mixes.value().contains_key(mix) {
            return Err(OperationError::invalid("Unknown mix"));
        }
        Ok(())
    }
    fn require_cell(&self, source: &SourceId, mix: &MixId) -> Result<()> {
        self.store.sources.require_writable()?;
        self.require_mix(mix)?;
        if !self.store.sources.value().contains_key(source) {
            return Err(OperationError::invalid("Unknown source"));
        }
        Ok(())
    }
    fn require_fx_source(&self, source: &SourceId) -> Result<()> {
        self.store.sources.require_writable()?;
        if self
            .store
            .sources
            .value()
            .get(source)
            .is_none_or(|source| source.kind != SourceKind::Device)
        {
            return Err(OperationError::invalid("Effects require a capture source"));
        }
        Ok(())
    }
    pub(super) fn set_source_fx(
        &mut self,
        source: &SourceId,
        settings: effects::FxSettings,
    ) -> Result<()> {
        self.require_fx_source(source)?;
        let settings = settings.validated()?;
        let mut sources = self.store.sources.value().clone();
        sources
            .get_mut(source)
            .ok_or_else(|| OperationError::invalid("Unknown source"))?
            .fx = Some(settings);
        self.commit_sources(sources)
    }
    pub(super) fn set_source_mute(
        &mut self,
        source: &SourceId,
        muted: bool,
        origin: Origin,
        owner: Option<CommandId>,
    ) -> Result<()> {
        let before = self.store.sources.value().clone();
        let mut candidate = before.clone();
        let changes = routing::set_source_muted(&mut candidate, source, muted)?;
        if !changes.is_empty() {
            self.commit_sources(candidate)?;
            self.sync_mutes(&before, &origin, owner);
        }
        Ok(())
    }
    fn device_setting(
        &mut self,
        id: CommandId,
        unit: UnitId,
        setting: DeviceSetting,
        timing: EditTiming,
    ) -> Result<()> {
        if !self.units.contains_key(&unit) {
            return Err(OperationError::unavailable("Captured unit is disconnected"));
        }
        let setting = validate_setting(unit.profile, setting)?;
        if matches!(setting, DeviceSetting::GainRaw(_))
            && self.view.preferences.gain_locked
            && self.view.selected_unit == Some(unit)
        {
            return Err(OperationError::invalid("Selected device gain is locked"));
        }
        let key = PendingKey::Device(unit, setting_field(setting));
        if timing == EditTiming::Debounced
            && matches!(
                setting,
                DeviceSetting::GainRaw(_)
                    | DeviceSetting::HeadphoneDb(_)
                    | DeviceSetting::MonitorMix(_)
            )
        {
            self.defer(
                key,
                id,
                AppCommand::SetDeviceSetting {
                    unit,
                    setting,
                    timing: EditTiming::Immediate,
                },
                Duration::from_millis(200),
            );
            return Ok(());
        }
        self.cancel_pending(|old| *old == key);
        if let DeviceSetting::Mute(muted) = setting {
            let sources = self.mute_sources_for_unit(unit);
            if !sources.is_empty() {
                let mut before = self.store.sources.value().clone();
                let mut candidate = before.clone();
                for source in &sources {
                    routing::set_source_muted(&mut candidate, source, muted)?;
                }
                self.commit_sources(candidate)?;
                // This captured unit is written explicitly below. Do not fall
                // back to a graph capture write for its rows while discovery is
                // unknown; only mirror the other members changed by the group.
                for source in &sources {
                    before.get_mut(source).expect("captured source").muted =
                        self.store.sources.value()[source].muted;
                }
                self.sync_mutes_excluding(&before, &Origin::User, Some(id), &HashSet::from([unit]));
                let muted = sources
                    .iter()
                    .all(|source| self.store.sources.value()[source].muted);
                return self.queue_device(Some(id), unit, vec![DeviceSetting::Mute(muted)]);
            }
        }
        self.queue_device(Some(id), unit, vec![setting])
    }
    fn remove_source(&mut self, source: &SourceId) -> Result<()> {
        let row = self
            .store
            .sources
            .value()
            .get(source)
            .ok_or_else(|| OperationError::invalid("Unknown source"))?;
        if row.protected && !matches!(self.graph_observation, Observation::Known(())) {
            return Err(OperationError::unavailable(
                "Capture availability is unknown; wait for graph discovery before removing a protected input",
            ));
        }
        if row.protected
            && self
                .view
                .captures
                .iter()
                .any(|capture| capture.node_name == row.node_name)
        {
            return Err(OperationError::invalid(
                "Connected protected inputs cannot be removed",
            ));
        }
        let before = Arc::clone(&self.view.desired);
        let mut sources = self.store.sources.value().clone();
        sources.shift_remove(source);
        let prepared_preferences = if !row.node_name.is_empty()
            && !sources
                .values()
                .any(|owner| owner.node_name == row.node_name)
            && self
                .view
                .preferences
                .offered_capture_nodes
                .contains(&row.node_name)
        {
            let mut preferences = (*self.view.preferences).clone();
            preferences
                .offered_capture_nodes
                .retain(|node| node != &row.node_name);
            Some(self.store.preferences.prepare(preferences)?)
        } else {
            None
        };
        let mut matrix = self.store.matrix.value().clone();
        matrix
            .cells
            .retain(|key, _| parse_cell_key(key).is_ok_and(|(id, _)| id != *source));
        let prepared_sources = self.store.sources.prepare(sources)?;
        let prepared_matrix = self.store.matrix.prepare(matrix)?;
        self.store.sources.publish(prepared_sources)?;
        let matrix_result = self.store.matrix.publish(prepared_matrix);
        let preferences_result = if let Some(prepared) = prepared_preferences {
            let result = self.store.preferences.publish(prepared);
            self.view.preferences = Arc::new(self.store.preferences.value().clone());
            result
        } else {
            Ok(())
        };
        self.invalidate_meters(&before.sources);
        self.cancel_pending(
            |key| matches!(key, PendingKey::Cell(id, _) | PendingKey::Fx(id) if id == source),
        );
        self.refresh_desired();
        self.refresh_bindings();
        self.refresh_calibration_validity();
        self.route();
        matrix_result.and(preferences_result)
    }
    fn remove_mix(&mut self, mix: &MixId) -> Result<()> {
        self.require_mix(mix)?;
        if self.store.mixes.value().len() <= 1 {
            return Err(OperationError::invalid("At least one mix must remain"));
        }
        let before = Arc::clone(&self.view.desired);
        let mut mixes = self.store.mixes.value().clone();
        mixes.shift_remove(mix);
        let mut matrix = self.store.matrix.value().clone();
        matrix
            .cells
            .retain(|key, _| parse_cell_key(key).is_ok_and(|(_, id)| id != *mix));
        matrix.outputs.shift_remove(mix);
        matrix.volumes.shift_remove(mix);
        let prepared_mixes = self.store.mixes.prepare(mixes)?;
        let prepared_matrix = self.store.matrix.prepare(matrix)?;
        self.store.mixes.publish(prepared_mixes)?;
        let matrix_result = self.store.matrix.publish(prepared_matrix);
        self.invalidate_meters(&before.sources);
        self.cancel_pending(|key| matches!(key, PendingKey::Cell(_, id) if id == mix));
        self.refresh_desired();
        self.route();
        matrix_result
    }
    pub(super) fn follow_capture_mutes(&mut self) {
        let captures = Arc::clone(&self.view.captures);
        let sources = self.store.sources.value().clone();
        for (id, source) in sources {
            if source.kind != SourceKind::Device || self.bound_unit(&source).is_some() {
                continue;
            }
            let mut matching = captures
                .iter()
                .filter(|capture| capture.node_name == source.node_name);
            let Some(capture) = matching.next() else {
                continue;
            };
            if matching.next().is_some() {
                continue;
            }
            if let Some(muted) = capture.muted.known() {
                let Some(binding) = self.bindings.get(&source.node_name).cloned() else {
                    continue;
                };
                let previous = self.capture_baselines.insert(
                    id.clone(),
                    (binding.clone(), capture.identity.clone(), *muted),
                );
                // A first observation establishes the external baseline, not a
                // request to discard a saved software mute. Rebinding/recreation
                // establishes a new baseline for the same reason.
                let changed = previous.is_some_and(|(old_binding, identity, previous)| {
                    old_binding == binding && identity == capture.identity && previous != *muted
                });
                if changed
                    && self
                        .store
                        .sources
                        .value()
                        .get(&id)
                        .is_some_and(|source| source.muted != *muted)
                {
                    if let Err(error) = self.set_source_mute(
                        &id,
                        *muted,
                        Origin::Capture(capture.identity.clone()),
                        None,
                    ) {
                        self.issue("sources", error);
                    }
                }
            }
        }
    }
    pub(super) fn auto_add_captures(&mut self) {
        if !self.store.routing_available() {
            return;
        }
        let captures: Vec<_> = self
            .view
            .captures
            .iter()
            .filter(|capture| {
                capture
                    .node_name
                    .starts_with("alsa_input.usb-Elgato_Systems_Elgato_Wave_")
                    || capture
                        .node_name
                        .starts_with("alsa_input.usb-Elgato_Systems_Elgato_XLR_Dock_")
            })
            .cloned()
            .collect();
        for capture in &captures {
            if self
                .store
                .sources
                .value()
                .values()
                .any(|source| source.node_name == capture.node_name)
                || self
                    .view
                    .preferences
                    .offered_capture_nodes
                    .contains(&capture.node_name)
            {
                continue;
            }
            let mut source = Source::new(capture.name.clone(), SourceKind::Device);
            source.node_name = capture.node_name.clone();
            source.protected = true;
            if let Some(channels) = capture.channels {
                source.extra.insert("channels".into(), channels.into());
            }
            let id = source.id.clone();
            let mut sources = self.store.sources.value().clone();
            sources.insert(id.clone(), source);
            if let Err(error) = self.commit_sources(sources) {
                self.issue("sources", error);
                continue;
            }
            if captures.len() == 1 && !self.store.sources.value().contains_key("mic") {
                let mut matrix = self.store.matrix.value().clone();
                let legacy: Vec<_> = matrix
                    .cells
                    .iter()
                    .filter_map(|(key, cell)| {
                        key.strip_prefix("mic.")
                            .map(|mix| (mix.to_owned(), cell.clone()))
                    })
                    .collect();
                for (mix, cell) in &legacy {
                    if self.store.mixes.value().contains_key(mix.as_str()) {
                        matrix.cells.insert(format!("{id}.{mix}"), cell.clone());
                    }
                }
                if !legacy.is_empty() {
                    matrix.cells.retain(|key, _| !key.starts_with("mic."));
                    if let Err(error) = self.commit_matrix(matrix) {
                        self.issue("matrix", error);
                    }
                }
            }
            let mut preferences = (*self.view.preferences).clone();
            preferences
                .offered_capture_nodes
                .push(capture.node_name.clone());
            if self.store.preferences.writable() {
                if let Err(error) = self.store.preferences.replace(preferences.clone()) {
                    self.issue("preferences", error);
                }
            }
            self.view.preferences = Arc::new(preferences);
            self.dirty = true;
        }
    }
    fn freeze(&mut self) {
        if self.frozen {
            return;
        }
        {
            let mut admission = self
                .shared
                .admission
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            admission.accepting = false;
        }
        self.frozen = true;
        self.view.lifecycle = Lifecycle::Frozen;
        self.held_captures.clear();
        self.handovers.clear();
        self.cancel_pending(|_| true);
        self.cancel_calibration();
        if self.store.preferences.writable() {
            if let Err(error) = self
                .store
                .preferences
                .replace((*self.view.preferences).clone())
            {
                self.issue("preferences", error);
            }
        }
        self.dirty = true;
        self.publish();
    }
    fn shutdown(&mut self, id: CommandId, removal: Option<(Arc<UninstallPlan>, bool)>) {
        self.freeze();
        self.view.lifecycle = Lifecycle::Draining;
        self.dirty = true;
        self.publish();
        let stopped = self.backend.shutdown();
        while let Some(event) = self.backend.next_event() {
            self.observe(event);
        }
        let outstanding: Vec<_> = self
            .requests
            .keys()
            .copied()
            .filter(|request| *request != id)
            .collect();
        for pending in outstanding {
            if self
                .requests
                .get(&pending)
                .is_some_and(|request| request.scene)
            {
                if let Some(request) = self.requests.get_mut(&pending) {
                    request.failed.push(OperationIssue {
                        target: "shutdown".into(),
                        message: "Unfinished scene targets were cancelled".into(),
                    });
                    request.remaining.clear();
                    request.waiting = false;
                }
                self.finish_if_ready(pending);
            } else {
                self.finish(pending, CommandOutcome::Cancelled);
            }
        }
        if let Err(error) = stopped {
            self.view.lifecycle = Lifecycle::Frozen;
            self.issue("shutdown", &error);
            if removal.is_some() {
                self.events.uninstall(UninstallResult {
                    success: false, removed: Vec::new(), error: Some(error.to_string()),
                    guidance: "Worker drain failed; no integration, settings or application removal was attempted.".into(),
                    app_removed: false,
                });
            }
            self.finish(
                id,
                CommandOutcome::Rejected(OperationError::unavailable(error.to_string())),
            );
            self.events.shutdown(Err(error));
            return;
        }
        if let Some((plan, delete_settings)) = removal {
            let result = self.backend.remove(&plan, delete_settings).unwrap_or_else(|error| UninstallResult {
                success: false, removed: Vec::new(), error: Some(error.to_string()),
                guidance: "Removal could not complete; retain the prepared recovery record and inspect its retry command.".into(),
                app_removed: false,
            });
            let failure = (!result.success).then(|| {
                format!(
                    "{}\n{}",
                    result.error.as_deref().unwrap_or("Removal incomplete"),
                    result.guidance
                )
            });
            self.events.uninstall(result);
            if let Some(message) = failure {
                self.view.lifecycle = Lifecycle::Frozen;
                self.issue("uninstall", &message);
                self.finish(
                    id,
                    CommandOutcome::Rejected(OperationError::unavailable(message)),
                );
                return;
            }
        }
        self.view.lifecycle = Lifecycle::Stopped;
        self.stopped = true;
        self.dirty = true;
        self.finish(
            id,
            CommandOutcome::Applied {
                revision: self.view.revision,
            },
        );
        self.publish();
        self.events.shutdown(Ok(()));
    }
}

use super::*;

#[derive(Clone, PartialEq, Eq)]
enum MuteTarget {
    Unit(UnitId),
    Capture(NodeIdentity),
    Absent,
    Missing,
}

#[derive(Clone)]
struct Member {
    source: SourceId,
    node: String,
    binding: CaptureBinding,
    target: MuteTarget,
}

pub(super) struct Handover {
    opening: SourceId,
    members: Vec<Member>,
    invalid: bool,
    silence_requested: bool,
}

impl Controller {
    fn mute_target(&self, source: &Source) -> MuteTarget {
        if self.graph_observation.known().is_none() {
            return MuteTarget::Missing;
        }
        if let Some(unit) = self.bound_unit(source) {
            return MuteTarget::Unit(unit);
        }
        let mut captures = self
            .view
            .captures
            .iter()
            .filter(|c| c.node_name == source.node_name);
        match (captures.next(), captures.next()) {
            (Some(capture), None) => MuteTarget::Capture(capture.identity.clone()),
            (None, None) => MuteTarget::Absent,
            _ => MuteTarget::Missing,
        }
    }

    pub(super) fn update_handovers(&mut self, before: &Sources) {
        self.handovers.retain(|group, handover| {
            self.store
                .sources
                .value()
                .get(&handover.opening)
                .is_some_and(|source| !source.muted && source.group == *group)
        });
        for (id, source) in self.store.sources.value() {
            if source.muted
                || source.group.is_empty()
                || before.get(id).is_some_and(|old| {
                    !old.muted && old.group == source.group && old.node_name == source.node_name
                })
            {
                continue;
            }
            let members = self
                .store
                .sources
                .value()
                .iter()
                .filter(|(_, peer)| peer.group == source.group && peer.kind == SourceKind::Device)
                .filter_map(|(id, peer)| {
                    self.bindings.get(&peer.node_name).map(|binding| Member {
                        source: id.clone(),
                        node: peer.node_name.clone(),
                        binding: binding.clone(),
                        target: self.mute_target(peer),
                    })
                })
                .collect();
            self.handovers.insert(
                source.group.clone(),
                Handover {
                    opening: id.clone(),
                    members,
                    invalid: false,
                    silence_requested: false,
                },
            );
        }
    }

    pub(super) fn routing_desired(&self) -> Arc<DesiredState> {
        if self.handovers.is_empty() {
            return Arc::clone(&self.view.desired);
        }
        let mut desired = (*self.view.desired).clone();
        for handover in self.handovers.values() {
            if let Some(source) = desired.sources.get_mut(&handover.opening) {
                source.muted = true;
            }
        }
        Arc::new(desired)
    }

    pub(super) fn unit_mute_pending(&self, unit: UnitId) -> bool {
        self.jobs.values().any(|job| {
            job.unit == unit
                && job
                    .settings
                    .iter()
                    .any(|setting| matches!(setting, DeviceSetting::Mute(_)))
        })
    }

    pub(super) fn mute_sources_for_unit(&self, unit: UnitId) -> Vec<SourceId> {
        let mut sources = self.unit_sources(unit);
        // Unknown graph state cannot discard an exact captured row/unit binding.
        // It still governs superseding edits as well as held opening admission.
        for (group, handover) in &self.handovers {
            for member in &handover.members {
                if member.target == MuteTarget::Unit(unit)
                    && self.member_binding_current(group, member)
                    && !sources.contains(&member.source)
                {
                    sources.push(member.source.clone());
                }
            }
        }
        sources
    }

    fn member_binding_current(&self, group: &str, member: &Member) -> bool {
        self.bindings.get(&member.node) == Some(&member.binding)
            && self
                .store
                .sources
                .value()
                .get(&member.source)
                .is_some_and(|source| {
                    source.node_name == member.node
                        && source.group == group
                        && source.kind == SourceKind::Device
                })
    }

    fn member_current(&self, group: &str, member: &Member) -> bool {
        self.member_binding_current(group, member)
            && self
                .store
                .sources
                .value()
                .get(&member.source)
                .is_some_and(|source| self.mute_target(source) == member.target)
    }

    fn member_has_open_owner(&self, member: &Member) -> bool {
        if matches!(member.target, MuteTarget::Absent | MuteTarget::Missing) {
            return false;
        }
        self.store
            .sources
            .value()
            .values()
            .any(|source| !source.muted && self.mute_target(source) == member.target)
    }

    fn member_silent(&self, member: &Member) -> bool {
        if self.member_has_open_owner(member) {
            // A row in another group may legitimately keep this physical input
            // open. Only the departing row must be silent, not its other owners.
            return self.graph_revision == Some(self.routing_revision)
                && self.view.captures.iter().any(|capture| {
                    capture.node_name == member.node
                        && self.silent_sources.get(&member.source) == Some(&capture.identity)
                });
        }
        match &member.target {
            MuteTarget::Unit(unit) => {
                !self.unit_mute_pending(*unit)
                    && self
                        .units
                        .get(unit)
                        .and_then(|u| u.state.known())
                        .is_some_and(|state| state.muted)
            }
            MuteTarget::Capture(identity) => {
                self.graph_revision == Some(self.routing_revision)
                    && !self.capture_intents.contains_key(&member.node)
                    && self.view.captures.iter().any(|capture| {
                        capture.identity == *identity
                            && capture.node_name == member.node
                            && capture.muted.known() == Some(&true)
                    })
            }
            MuteTarget::Absent => {
                self.graph_observation.known().is_some()
                    && self.graph_revision == Some(self.routing_revision)
                    && !self
                        .view
                        .captures
                        .iter()
                        .any(|capture| capture.node_name == member.node)
            }
            MuteTarget::Missing => false,
        }
    }

    fn request_handover_silence(&mut self) {
        if self.graph_observation.known().is_none() {
            return;
        }
        let mut required: Vec<Member> = Vec::new();
        for (group, handover) in &self.handovers {
            if handover.invalid || handover.silence_requested {
                continue;
            }
            let opening_target = handover
                .members
                .iter()
                .find(|member| member.source == handover.opening)
                .map(|member| &member.target);
            for member in &handover.members {
                if member.source == handover.opening
                    || opening_target == Some(&member.target)
                    || !self.member_current(group, member)
                    || self.member_silent(member)
                    || self.member_has_open_owner(member)
                    || required.iter().any(|old| old.target == member.target)
                {
                    continue;
                }
                let pending = match &member.target {
                    MuteTarget::Unit(unit) => self.unit_mute_pending(*unit),
                    MuteTarget::Capture(_) => self.capture_intents.contains_key(&member.node),
                    MuteTarget::Absent | MuteTarget::Missing => true,
                };
                if !pending {
                    required.push(member.clone());
                }
            }
        }
        for handover in self.handovers.values_mut() {
            handover.silence_requested = true;
        }
        // Saved software mute may deliberately differ from the first hardware
        // baseline. A handover must request its required silence even when that
        // peer's desired row did not change in this transition. Admit this once;
        // completion events must not create an unbounded device retry loop.
        for member in required {
            let result = match member.target {
                MuteTarget::Unit(unit) => {
                    self.queue_device(None, unit, vec![DeviceSetting::Mute(true)])
                }
                MuteTarget::Capture(_) => {
                    self.queue_capture(member.node.clone(), member.binding, true, None)
                }
                MuteTarget::Absent | MuteTarget::Missing => continue,
            };
            if let Err(error) = result {
                self.issue(format!("capture {}", member.node), error);
            }
        }
    }

    pub(super) fn queue_capture(
        &mut self,
        node: String,
        binding: CaptureBinding,
        muted: bool,
        owner: Option<CommandId>,
    ) -> Result<()> {
        if !muted
            && binding.owners.iter().any(|id| {
                self.store
                    .sources
                    .value()
                    .get(id)
                    .is_some_and(|source| self.handovers.contains_key(&source.group))
            })
        {
            if let Some((_, previous)) = self.held_captures.insert(node, (binding, owner)) {
                if previous != owner {
                    if let Some(id) = previous {
                        self.finish(id, CommandOutcome::Cancelled);
                    }
                }
            }
            return Ok(());
        }
        let mut captures = self
            .view
            .captures
            .iter()
            .filter(|capture| capture.node_name == node);
        match (captures.next(), captures.next()) {
            (Some(_), None) if self.graph_observation.known().is_some() => {}
            _ => {
                return Err(OperationError::unavailable(
                    "Capture mute needs an exact observed identity",
                ));
            }
        };
        self.backend.dispatch(BackendCommand::CaptureMute {
            node_name: node.clone(),
            binding: binding.clone(),
            muted,
        })?;
        self.capture_intents.insert(node, (binding, muted));
        Ok(())
    }

    // Admission/dispatch is not silence. Only a current exact graph observation or
    // a completed captured-unit operation can release another domain's opening.
    pub(super) fn advance_handovers(&mut self) {
        if self.frozen {
            return;
        }
        self.request_handover_silence();
        let mut ready = Vec::new();
        let mut invalid = Vec::new();
        for (group, handover) in &self.handovers {
            if handover.invalid {
                continue;
            }
            // Unknown graph state is temporary, not absence or replacement.
            if self.graph_observation.known().is_none() {
                continue;
            }
            if handover
                .members
                .iter()
                .any(|member| !self.member_current(group, member))
            {
                if self.graph_revision == Some(self.routing_revision) {
                    invalid.push(group.clone());
                }
                continue;
            }
            let opening_target = handover
                .members
                .iter()
                .find(|m| m.source == handover.opening)
                .map(|m| &m.target);
            if handover
                .members
                .iter()
                .filter(|m| m.source != handover.opening && opening_target != Some(&m.target))
                .all(|member| self.member_silent(member))
                && handover.members.iter().all(|m| {
                    m.target != MuteTarget::Missing
                        && (m.source != handover.opening || m.target != MuteTarget::Absent)
                })
            {
                ready.push(group.clone());
            }
        }
        for group in invalid {
            self.handovers
                .get_mut(&group)
                .expect("existing handover")
                .invalid = true;
            self.issue(
                format!("group {group}"),
                "Mute handover identity changed; request a new group transition",
            );
        }
        let released = !ready.is_empty();
        for group in ready {
            self.handovers.remove(&group);
        }
        let held: Vec<_> = self
            .jobs
            .iter()
            .filter(|(_, job)| !job.dispatched)
            .map(|(id, _)| *id)
            .collect();
        for job_id in held {
            let job = &self.jobs[&job_id];
            let cancelled = !self.units.contains_key(&job.unit)
                || job.open_sources.iter().any(|(id, node, group)| {
                    self.store.sources.value().get(id).is_none_or(|source| {
                        source.muted
                            || source.node_name != *node
                            || source.group != *group
                            || (self.graph_observation.known().is_some()
                                && self.bound_unit(source) != Some(job.unit))
                    }) || self.handovers.get(group).is_some_and(|h| h.invalid)
                });
            if !cancelled
                && job
                    .open_sources
                    .iter()
                    .any(|(_, _, group)| self.handovers.contains_key(group))
            {
                continue;
            }
            let job = self.jobs.remove(&job_id).expect("held device job");
            let result = if cancelled {
                Err(OperationError::new(
                    ErrorCode::Cancelled,
                    "Mute opening was superseded",
                ))
            } else {
                self.backend.dispatch(BackendCommand::Device {
                    job: job_id,
                    unit: job.unit,
                    settings: job.settings.clone(),
                })
            };
            match result {
                Ok(()) => {
                    self.jobs.insert(
                        job_id,
                        DeviceJob {
                            dispatched: true,
                            ..job
                        },
                    );
                }
                Err(error) => {
                    self.operation_failure(job.owner, format!("device {:?}", job.unit), error);
                    if let Some(id) = job.owner {
                        if let Some(request) = self.requests.get_mut(&id) {
                            request.remaining.remove(&job_id);
                        }
                        self.finish_if_ready(id);
                    }
                }
            }
        }
        let held = std::mem::take(&mut self.held_captures);
        let mut owners = HashSet::new();
        for (node, (binding, owner)) in held {
            let cancelled = self.bindings.get(&node) != Some(&binding)
                || binding.owners.iter().all(|id| {
                    self.store
                        .sources
                        .value()
                        .get(id)
                        .is_none_or(|source| source.muted)
                })
                || binding.owners.iter().any(|id| {
                    self.store.sources.value().get(id).is_some_and(|source| {
                        self.handovers.get(&source.group).is_some_and(|h| h.invalid)
                    })
                });
            let result = if cancelled {
                Err(OperationError::new(
                    ErrorCode::Cancelled,
                    "Capture opening was superseded",
                ))
            } else {
                self.queue_capture(node.clone(), binding, false, owner)
            };
            if let Err(error) = result {
                self.operation_failure(owner, format!("capture {node}"), error);
            }
            if let Some(id) = owner {
                owners.insert(id);
            }
        }
        for id in owners {
            self.finish_if_ready(id);
        }
        if released {
            self.refresh_desired();
            self.route();
        }
    }
}

use super::*;
use openwave_core::{
    calibration::{self as analysis, Analysis, Metrics, ToneProposal},
    effects::{capture_channels, validate_raw_node},
};

pub(super) struct CalibrationSession {
    token: CalibrationToken,
    binding: CaptureBinding,
    recording: Option<(CommandId, bool)>,
    floor: Option<Metrics>,
    proposal: Option<(Analysis, ToneProposal)>,
}

impl Controller {
    fn calibration_capture(&self, source: &SourceId) -> Result<&CaptureSnapshot> {
        if let Observation::Unknown(error) = &self.graph_observation {
            return Err(OperationError::unavailable(format!(
                "Calibration requires a current audio graph: {error}"
            )));
        }
        self.store.sources.require_writable()?;
        let row = self.store.sources.value().get(source).ok_or_else(|| {
            OperationError::new(ErrorCode::Identity, "Calibration source was removed")
        })?;
        if row.kind != SourceKind::Device {
            return Err(OperationError::invalid(
                "Calibration requires a device source",
            ));
        }
        validate_raw_node(&row.node_name)?;
        if row.node_name == "auto"
            || row.node_name.starts_with("openwave_")
            || row.node_name.ends_with(".monitor")
        {
            return Err(OperationError::invalid(
                "Select a raw microphone, not a monitor or processed input",
            ));
        }
        let mut captures = self
            .view
            .captures
            .iter()
            .filter(|capture| capture.node_name == row.node_name);
        let capture = captures.next().ok_or_else(|| {
            OperationError::new(ErrorCode::Identity, "Calibration input is disconnected")
        })?;
        if captures.next().is_some() {
            return Err(OperationError::new(
                ErrorCode::Identity,
                "Calibration input identity is ambiguous",
            ));
        }
        let serial = capture.properties.get("object.serial").and_then(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .or_else(|| value.as_u64().map(|n| n.to_string()))
        });
        if serial.as_deref() != Some(capture.identity.object_serial.as_str())
            || serial.as_deref().is_none_or(str::is_empty)
        {
            return Err(OperationError::new(
                ErrorCode::Identity,
                "Calibration needs the current capture object.serial",
            ));
        }
        capture_channels(capture.channels)?;
        Ok(capture)
    }

    fn validate_calibration(&self, token: &CalibrationToken) -> Result<()> {
        let session = self
            .calibration
            .as_ref()
            .filter(|session| session.token == *token)
            .ok_or_else(|| {
                OperationError::new(
                    ErrorCode::Identity,
                    "Calibration session expired; repeat the measurements",
                )
            })?;
        let capture = self.calibration_capture(&token.source)?;
        if capture.node_name != token.node_name
            || capture.identity != token.identity
            || capture_channels(capture.channels)? != token.channels
        {
            return Err(OperationError::new(
                ErrorCode::Identity,
                "Calibration input generation or channels changed; repeat the measurements",
            ));
        }
        if self.bindings.get(&token.node_name) != Some(&session.binding) {
            return Err(OperationError::new(
                ErrorCode::Identity,
                "Calibration source binding changed; repeat the measurements",
            ));
        }
        Ok(())
    }

    fn calibration_phase(&mut self, phase: CalibrationPhase) {
        if let Some(snapshot) = &mut self.view.calibration {
            snapshot.phase = phase;
        }
        self.dirty = true;
    }

    fn expire_calibration(&mut self, error: OperationError) {
        if let Some(session) = self.calibration.take() {
            if let Err(cancel_error) = self
                .backend
                .dispatch(BackendCommand::CancelCalibration(session.token))
            {
                self.issue("calibration cancellation", cancel_error);
            }
            if let Some((command, _)) = session.recording {
                self.finish(command, CommandOutcome::Rejected(error.clone()));
            }
        }
        self.calibration_phase(CalibrationPhase::Expired(error.to_string()));
        self.issue("calibration", error);
    }

    pub(super) fn start_calibration(&mut self, source: &SourceId) -> Result<()> {
        self.cancel_calibration();
        let capture = self.calibration_capture(source)?;
        let token = CalibrationToken {
            session: self.next_session,
            source: source.clone(),
            node_name: capture.node_name.clone(),
            identity: capture.identity.clone(),
            channels: capture_channels(capture.channels)?,
        };
        let binding = self
            .bindings
            .get(&token.node_name)
            .cloned()
            .ok_or_else(|| {
                OperationError::new(
                    ErrorCode::Identity,
                    "Calibration source has no current capture binding",
                )
            })?;
        self.next_session = self
            .next_session
            .checked_add(1)
            .ok_or_else(|| OperationError::unavailable("Calibration session space exhausted"))?;
        self.view.calibration = Some(CalibrationSnapshot {
            token: token.clone(),
            phase: CalibrationPhase::NoiseReady,
        });
        self.calibration = Some(CalibrationSession {
            token,
            binding,
            recording: None,
            floor: None,
            proposal: None,
        });
        self.clear_issue("calibration");
        self.dirty = true;
        Ok(())
    }

    pub(super) fn record_calibration(
        &mut self,
        command: CommandId,
        token: &CalibrationToken,
        speech: bool,
    ) -> Result<()> {
        self.refresh_calibration_validity();
        self.validate_calibration(token)?;
        let session = self.calibration.as_ref().expect("validated session");
        if session.recording.is_some()
            || session.proposal.is_some()
            || speech != session.floor.is_some()
        {
            return Err(OperationError::invalid(
                "Record room noise once, then speech, then review the proposal",
            ));
        }
        if let Err(error) = self.backend.dispatch(BackendCommand::RecordCalibration {
            token: token.clone(),
            seconds: if speech {
                analysis::SPEECH_SECONDS
            } else {
                analysis::FLOOR_SECONDS
            },
        }) {
            self.expire_calibration(error.clone());
            return Err(error);
        }
        self.calibration
            .as_mut()
            .expect("validated session")
            .recording = Some((command, speech));
        if let Some(request) = self.requests.get_mut(&command) {
            request.waiting = true;
        }
        self.calibration_phase(if speech {
            CalibrationPhase::RecordingSpeech
        } else {
            CalibrationPhase::RecordingNoise
        });
        Ok(())
    }

    pub(super) fn accept_calibration(&mut self, token: &CalibrationToken) -> Result<()> {
        self.refresh_calibration_validity();
        self.validate_calibration(token)?;
        let session = self.calibration.as_ref().expect("validated session");
        let (analysis, tone) = session.proposal.as_ref().ok_or_else(|| {
            OperationError::invalid("Record both calibration phases and review before applying")
        })?;
        let current = self.store.sources.value()[&token.source]
            .fx
            .clone()
            .unwrap_or_default();
        let settings = analysis.proposed_settings(&current, Some(tone))?;
        self.cancel_pending(|key| matches!(key, PendingKey::Fx(source) if source == &token.source));
        if let Err(error) = self.set_source_fx(&token.source, settings) {
            self.expire_calibration(error.clone());
            return Err(error);
        }
        self.calibration = None;
        self.view.calibration = None;
        self.clear_issue("calibration");
        self.dirty = true;
        Ok(())
    }

    pub(super) fn cancel_calibration(&mut self) {
        if let Some(session) = self.calibration.take() {
            if let Err(error) = self
                .backend
                .dispatch(BackendCommand::CancelCalibration(session.token))
            {
                self.issue("calibration cancellation", error);
            }
            if let Some((command, _)) = session.recording {
                self.finish(command, CommandOutcome::Cancelled);
            }
        }
        self.view.calibration = None;
        self.dirty = true;
    }

    pub(super) fn cancel_calibration_token(&mut self, token: &CalibrationToken) -> Result<()> {
        if !self
            .view
            .calibration
            .as_ref()
            .is_some_and(|snapshot| snapshot.token == *token)
        {
            return Err(OperationError::new(
                ErrorCode::Identity,
                "Calibration session is no longer current",
            ));
        }
        self.cancel_calibration();
        Ok(())
    }

    pub(super) fn refresh_calibration_validity(&mut self) {
        let Some(session) = self.calibration.as_ref() else {
            return;
        };
        if let Err(error) = self.validate_calibration(&session.token) {
            self.expire_calibration(error);
        }
    }

    pub(super) fn calibration_complete(&mut self, event: CalibrationEvent) {
        if self.frozen {
            return;
        }
        let Some(session) = self
            .calibration
            .as_ref()
            .filter(|session| session.token == event.token)
        else {
            return;
        };
        let Some((command, speech)) = session.recording else {
            return;
        };
        if let Err(error) = self.validate_calibration(&event.token) {
            self.expire_calibration(error);
            return;
        }
        let metrics = match event.result {
            Ok(metrics) => metrics,
            Err(error) if error.code == ErrorCode::Cancelled => {
                self.cancel_calibration();
                return;
            }
            Err(error) => {
                self.expire_calibration(error);
                return;
            }
        };
        if speech {
            let floor = self
                .calibration
                .as_ref()
                .and_then(|session| session.floor.as_ref())
                .expect("speech requires noise");
            let proposal =
                analysis::analyze(&floor.peaks_db, &metrics.peaks_db).and_then(|analysis| {
                    let tone = analysis::analyze_tone(floor, &metrics)?;
                    let current = self.store.sources.value()[&event.token.source]
                        .fx
                        .clone()
                        .unwrap_or_default();
                    let settings = analysis.proposed_settings(&current, Some(&tone))?;
                    Ok((analysis, tone, settings))
                });
            let (analysis, tone, settings) = match proposal {
                Ok(proposal) => proposal,
                Err(error) => {
                    self.expire_calibration(error);
                    return;
                }
            };
            let mut summary = format!(
                "Noise floor: {:.1} dB\nQuiet / loud speech: {:.1} / {:.1} dB\n\nGate: {:.1} dB\nCompressor: {:.1} dB, {:.1}:1\nLow cut: {} Hz\nHigh shelf: {:+.0} dB",
                analysis.measured.floor_db,
                analysis.measured.quiet_voice_db,
                analysis.measured.loud_voice_db,
                settings.gate_thresh,
                settings.comp_thresh,
                settings.comp_ratio,
                settings.lowcut,
                settings.eq_high
            );
            if tone.mono == Some(true) {
                summary.push_str("\nMono: enabled (one channel is very quiet)");
            }
            self.calibration.as_mut().expect("current session").proposal = Some((analysis, tone));
            self.calibration_phase(CalibrationPhase::Review {
                proposal: settings,
                summary,
            });
        } else {
            self.calibration.as_mut().expect("current session").floor = Some(metrics);
            self.calibration_phase(CalibrationPhase::SpeechReady);
        }
        self.calibration
            .as_mut()
            .expect("current session")
            .recording = None;
        if let Some(request) = self.requests.get_mut(&command) {
            request.waiting = false;
        }
        self.finish_if_ready(command);
    }
}

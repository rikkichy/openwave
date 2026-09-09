use crate::{
    process::CommandRunner,
    recovery::{self, Commands, Restoration},
};
use openwave_core::{
    health::{
        self, CaptureSample, HealthGraph, HealthReport, HealthSamples, HealthWatch, SinkSample,
        WatchedNode,
    },
    model::{NodeIdentity, Observation, OperationError, Result},
};
use serde_json::Value;
use std::{
    collections::HashMap,
    fs::File,
    io::Read,
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

type Gaps = Arc<dyn Fn() -> HashMap<NodeIdentity, Duration> + Send + Sync>;
fn unknown<T>(message: &str) -> Observation<T> {
    Observation::Unknown(OperationError::unavailable(message))
}
fn observed<T>(result: Result<T>) -> Observation<T> {
    match result {
        Ok(value) => Observation::Known(value),
        Err(error) => Observation::Unknown(error),
    }
}
fn graph(runner: &impl Commands) -> Result<HealthGraph> {
    health::parse_health_graph(&recovery::json_command(runner, "pw-dump", &[], false)?)
}
fn mute_listing(runner: &impl Commands, kind: &str) -> Result<Value> {
    let value = recovery::json_command(runner, "pactl", &["--format=json", "list", kind], false)?;
    health::parse_mutes(&value)?;
    Ok(value)
}
fn mute_for(value: &Result<Value>, name: &str, node: &WatchedNode) -> Observation<bool> {
    let value = match value {
        Ok(value) => value,
        Err(error) => return Observation::Unknown(error.clone()),
    };
    let Some(entry) = value.as_array().and_then(|entries| {
        entries
            .iter()
            .find(|entry| entry.get("name").and_then(Value::as_str) == Some(name))
    }) else {
        return unknown("Mute target is absent");
    };
    let serial = entry
        .pointer("/properties/object.serial")
        .and_then(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .or_else(|| value.as_u64().map(|n| n.to_string()))
        });
    if serial.as_deref() != Some(node.identity.object_serial.as_str()) {
        return unknown("Mute target generation is unknown or changed");
    }
    entry
        .get("mute")
        .and_then(Value::as_bool)
        .map_or_else(|| unknown("Mute is unavailable"), Observation::Known)
}
pub(crate) fn read_playback_status(
    root: &Path,
    playback: health::AlsaPlayback,
) -> Result<health::PlaybackStatus> {
    let path = root.join(format!(
        "card{}/pcm{}p/sub{}/status",
        playback.card, playback.device, playback.subdevice
    ));
    let mut text = String::new();
    File::open(path)?.take(65_537).read_to_string(&mut text)?;
    if text.len() > 65_536 {
        return Err(OperationError::unavailable(
            "ALSA playback status exceeds bound",
        ));
    }
    health::parse_playback_status(&text)
}
pub(crate) fn collect_samples(
    runner: &impl Commands,
    gaps: &dyn Fn() -> HashMap<NodeIdentity, Duration>,
    proc_root: &Path,
) -> Result<HealthSamples> {
    recovery::check_cancel(runner)?;
    let first = graph(runner)?;
    let counts = if first.captures.is_empty() {
        Ok(Default::default())
    } else {
        let args = ["--batch-mode".into(), "--iterations".into(), "3".into()];
        runner
            .run("pw-top", &args, Duration::from_secs(15), false)
            .and_then(|bytes| {
                String::from_utf8(bytes)
                    .map_err(|_| OperationError::unavailable("pw-top output is not UTF-8"))
            })
            .map(|text| health::parse_pw_top(&text))
    };
    let source_mutes = if first.captures.is_empty() {
        Ok(Value::Array(vec![]))
    } else {
        mute_listing(runner, "sources")
    };
    let sink_mutes = if first.sinks.is_empty() {
        Ok(Value::Array(vec![]))
    } else {
        mute_listing(runner, "sinks")
    };
    let mut samples = HealthSamples::default();
    for (name, sink) in &first.sinks {
        samples.sinks.insert(
            name.clone(),
            Observation::Known(SinkSample {
                identity: sink.node.identity.clone(),
                running: sink.node.running,
                muted: mute_for(&sink_mutes, name, &sink.node),
                playback: observed(read_playback_status(proc_root, sink.playback)),
            }),
        );
    }
    // Bracket pw-top/Pulse/proc collection with coherent graph identity. An ID
    // reused during the 15-second sampler cannot be attributed to an old unit.
    let second = graph(runner)?;
    if first.captures != second.captures || first.sinks != second.sinks {
        return Err(OperationError::unavailable(
            "Health graph changed during collection",
        ));
    }
    recovery::check_cancel(runner)?;
    let ages = gaps();
    for (name, node) in &second.captures {
        let xruns = match &counts {
            Ok(counts) => counts
                .get(&node.node_id)
                .filter(|count| count.node_name == *name)
                .map_or_else(
                    || unknown("No coherent xrun observation"),
                    |count| Observation::Known(count.count),
                ),
            Err(error) => Observation::Unknown(error.clone()),
        };
        samples.captures.insert(
            name.clone(),
            Observation::Known(CaptureSample {
                identity: node.identity.clone(),
                running: node.running,
                muted: mute_for(&source_mutes, name, node),
                xruns,
                byte_age: ages.get(&node.identity).copied().map_or_else(
                    || unknown("No byte age for this capture generation"),
                    Observation::Known,
                ),
            }),
        );
    }
    Ok(samples)
}

/// Unexpected collector failures also end every healthy interval. They cannot
/// leave a refill timer running across a missing or malformed observation.
pub(crate) fn collect_observation(
    runner: &impl Commands,
    gaps: &dyn Fn() -> HashMap<NodeIdentity, Duration>,
    proc_root: &Path,
) -> Observation<HealthSamples> {
    match catch_unwind(AssertUnwindSafe(|| {
        collect_samples(runner, gaps, proc_root)
    })) {
        Ok(result) => observed(result),
        Err(_) => unknown("Health collector panicked; observation unavailable"),
    }
}
fn record_error(slot: &mut Option<OperationError>, error: OperationError) {
    if error.code == openwave_core::model::ErrorCode::Cancelled {
        return;
    }
    *slot = Some(match slot.take() {
        None => error,
        Some(previous) => OperationError::unavailable(format!("{previous}; {error}")),
    });
}
pub(crate) fn check_once(
    watch: &mut HealthWatch,
    runner: &impl Commands,
    auto_recover: bool,
    samples: Observation<HealthSamples>,
    now: Duration,
    restoration: &mut Restoration,
) -> HealthReport {
    let mut report = watch.observe(&samples, now);
    // Repayment is not a new recovery attempt, even in observe-only mode or
    // after cancellation. Do not use the pre-restoration sample to start work.
    if restoration.is_pending() {
        if let Err(error) = restoration.restore_with(runner) {
            record_error(&mut report.error, error);
        }
        return report;
    }
    let Some(samples) = samples.known() else {
        return report;
    };
    for (name, decision) in &report.captures {
        if decision.just_no_data || decision.just_glitching {
            log::warn!(
                "{name}: capture fault (no data={}, xruns={}); auto-recover {auto_recover}",
                decision.no_data,
                decision.glitching
            );
        }
        if auto_recover && decision.recovery_due && !runner.cancelled() && !restoration.is_pending()
        {
            let Some(sample) = samples.captures.get(name).and_then(Observation::known) else {
                continue;
            };
            // This budget is shared by xrun and no-byte remedies. Spend before
            // card discovery too, so repeated mapping/command failures are bounded.
            watch.capture.record_attempt(name, now);
            if let Err(error) =
                recovery::cycle_card_with(name, &sample.identity, runner, restoration)
            {
                log::warn!("{name}: card recovery failed: {error}");
                record_error(&mut report.error, error);
            }
        }
    }
    for (name, decision) in &report.sinks {
        if decision.just_stalled {
            log::warn!("{name}: playback hardware stalled; auto-recover {auto_recover}");
        }
        if auto_recover && decision.recovery_due && !runner.cancelled() && !restoration.is_pending()
        {
            let Some(sample) = samples.sinks.get(name).and_then(Observation::known) else {
                continue;
            };
            watch.sink.record_attempt(name, now);
            if let Err(error) =
                recovery::recycle_sink_with(name, &sample.identity, runner, restoration)
            {
                log::warn!("{name}: sink recovery failed: {error}");
                record_error(&mut report.error, error);
            }
        }
    }
    report
}

type RestoreOnStop = Box<dyn FnMut() -> Result<()> + Send>;
pub struct HealthMonitor {
    cancel: Arc<AtomicBool>,
    thread: Option<JoinHandle<RestoreOnStop>>,
    restore: Option<RestoreOnStop>,
    failure: Option<OperationError>,
}
impl HealthMonitor {
    pub fn start(auto_recover: bool, gaps: Gaps) -> Result<Self> {
        let cancel = Arc::new(AtomicBool::new(false));
        let runner = CommandRunner::new(cancel.clone());
        Self::start_with(auto_recover, gaps, cancel, runner)
    }
    pub(crate) fn start_with(
        auto_recover: bool,
        gaps: Gaps,
        cancel: Arc<AtomicBool>,
        runner: impl Commands + Send + 'static,
    ) -> Result<Self> {
        let stopped = cancel.clone();
        let thread = thread::Builder::new()
            .name("openwave-health".into())
            .spawn(move || {
                let mut watch = HealthWatch::default();
                let start = Instant::now();
                let mut restoration = Restoration::default();
                while !stopped.load(Ordering::Acquire) {
                    let samples = collect_observation(&runner, &*gaps, Path::new("/proc/asound"));
                    let report = check_once(
                        &mut watch,
                        &runner,
                        auto_recover,
                        samples,
                        start.elapsed(),
                        &mut restoration,
                    );
                    if let Some(error) = report.error {
                        log::warn!("Health observation/recovery: {error}");
                    }
                    if !stopped.load(Ordering::Acquire) {
                        thread::park_timeout(health::CHECK_INTERVAL);
                    }
                }
                // Transfer the runner and the obligation, not just its error.
                // A failed stop retains both for the next bounded retry.
                Box::new(move || restoration.restore_with(&runner)) as RestoreOnStop
            })?;
        Ok(Self {
            cancel,
            thread: Some(thread),
            restore: None,
            failure: None,
        })
    }
    /// Cancel collection first. Joining cannot abandon an owed, bounded restore.
    pub fn stop(&mut self) -> Result<()> {
        self.cancel.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            match thread.join() {
                Ok(restore) => self.restore = Some(restore),
                Err(_) => {
                    self.failure = Some(OperationError::unavailable("Health worker panicked"));
                }
            }
        }
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        if let Some(restore) = &mut self.restore {
            restore()?;
            self.restore = None;
        }
        Ok(())
    }
    /// Retain this owner until every exact restoration has completed. Each
    /// `stop` attempt remains bounded, but the original identity may be absent
    /// indefinitely. A worker join failure is not a restoration retry.
    pub fn stop_until_restored(&mut self) -> Result<()> {
        loop {
            match self.stop() {
                Err(error) if self.restore.is_some() => {
                    log::warn!("Health shutdown waiting for restoration: {error}");
                    thread::sleep(Duration::from_secs(1));
                }
                result => return result,
            }
        }
    }
}
impl Drop for HealthMonitor {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            log::error!("Health shutdown: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocking_shutdown_preserves_nonretryable_join_failure() {
        let mut monitor = HealthMonitor {
            cancel: Arc::new(AtomicBool::new(false)),
            thread: Some(thread::spawn(|| -> RestoreOnStop {
                panic!("fixture health worker panic")
            })),
            restore: None,
            failure: None,
        };
        assert!(monitor.stop_until_restored().is_err());
        assert!(monitor.stop().is_err());
    }
}

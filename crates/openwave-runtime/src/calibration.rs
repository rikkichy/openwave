use crate::process::OwnedChild;
use openwave_core::{
    calibration::{self, GRACE_SECONDS, Metrics, RATE},
    model::{CalibrationToken, ErrorCode, OperationError, Result},
};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use std::{
    io::Read,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

fn cancelled() -> OperationError {
    OperationError::new(ErrorCode::Cancelled, "Calibration cancelled")
}
fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Acquire) {
        Err(cancelled())
    } else {
        Ok(())
    }
}
fn validate_capture(node: &str, seconds: u32, channels: u32) -> Result<()> {
    if node.is_empty()
        || node.contains('\0')
        || matches!(node, "auto" | "0" | "-1")
        || node.starts_with("openwave_fx_")
    {
        return Err(OperationError::invalid(
            "Select a raw microphone node for calibration",
        ));
    }
    if !(1..=60).contains(&seconds) || !matches!(channels, 1 | 2) {
        return Err(OperationError::invalid(
            "Capture needs 1–60 seconds and one or two channels",
        ));
    }
    Ok(())
}

/// Capture only the explicit raw target. No default-device fallback or settings writes.
pub fn capture_raw(
    node_name: &str,
    seconds: u32,
    channels: u32,
    cancel: Arc<AtomicBool>,
) -> Result<Vec<u8>> {
    validate_capture(node_name, seconds, channels)?;
    let args = vec!["--record".into(), "--raw".into(), "--target".into(), node_name.into(), "--rate".into(), RATE.to_string(),
        "--channels".into(), channels.to_string(), "--format".into(), "s16".into(), "--properties".into(),
        r#"{ "media.name": "openwave_calibration", "node.name": "openwave_calibration", "application.name": "OpenWave", "node.dont-reconnect": true, "node.dont-fallback": true, "node.dont-move": true }"#.into(), "-".into()];
    capture_with(
        seconds,
        channels,
        cancel,
        Duration::from_secs(seconds as u64 + GRACE_SECONDS as u64) + Duration::from_millis(500),
        || OwnedChild::spawn("pw-cat", &args, Stdio::piped()),
    )
}

fn capture_token(
    token: &CalibrationToken,
    seconds: u32,
    cancel: Arc<AtomicBool>,
) -> Result<Vec<u8>> {
    let runner = crate::process::CommandRunner::new(cancel.clone());
    validate_token(token, &runner)?;
    let raw = capture_raw(
        &token.identity.object_serial,
        seconds,
        token.channels,
        cancel,
    )?;
    validate_token(token, &runner)?;
    Ok(raw)
}

pub(crate) fn validate_token(
    token: &CalibrationToken,
    runner: &impl crate::recovery::Commands,
) -> Result<()> {
    let graph = crate::recovery::validate_node(runner, &token.node_name, &token.identity, false)?;
    let node = graph
        .as_array()
        .and_then(|nodes| {
            nodes.iter().find(|node| {
                node.pointer("/info/props/node.name")
                    .and_then(serde_json::Value::as_str)
                    == Some(token.node_name.as_str())
            })
        })
        .ok_or_else(|| {
            OperationError::new(ErrorCode::Identity, "Calibration capture disappeared")
        })?;
    let serial = node.pointer("/info/props/object.serial").and_then(|value| {
        value
            .as_str()
            .map(str::to_owned)
            .or_else(|| value.as_u64().map(|n| n.to_string()))
    });
    if serial.as_deref() != Some(token.identity.object_serial.as_str()) {
        return Err(OperationError::new(
            ErrorCode::Identity,
            "Calibration needs the current capture object.serial",
        ));
    }
    let channels = match node.pointer("/info/props/audio.channels") {
        None => None,
        Some(value) => Some(
            value
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .or_else(|| value.as_str().and_then(|s| s.parse::<u32>().ok()))
                .ok_or_else(|| {
                    OperationError::new(ErrorCode::Identity, "Capture channels are unavailable")
                })?,
        ),
    };
    if openwave_core::effects::capture_channels(channels)? != token.channels {
        return Err(OperationError::new(
            ErrorCode::Identity,
            "Calibration capture channels changed",
        ));
    }
    Ok(())
}

// The spawn seam exercises ownership transfer during cancellation without opening audio.
pub(crate) fn capture_with(
    seconds: u32,
    channels: u32,
    cancel: Arc<AtomicBool>,
    timeout: Duration,
    spawn: impl FnOnce() -> Result<OwnedChild>,
) -> Result<Vec<u8>> {
    validate_capture("raw", seconds, channels)?;
    check_cancel(&cancel)?;
    let deadline = Instant::now() + timeout;
    let mut child = spawn()?;
    let result = (|| {
        check_cancel(&cancel)?;
        let mut stdout = child
            .take_stdout()
            .ok_or_else(|| OperationError::unavailable("Capture stdout unavailable"))?;
        let frame = channels as usize * 2;
        let transient = RATE as usize * frame / 2;
        let expected = RATE as usize * seconds as usize * frame;
        // One output allocation; discard transient into a reusable stack buffer.
        let mut discard = [0_u8; 8192];
        let mut remaining = transient;
        while remaining != 0 {
            let n = remaining.min(discard.len());
            read_exact_until(&mut stdout, &mut discard[..n], deadline, &cancel)?;
            remaining -= n;
        }
        let mut raw = vec![0; expected];
        read_exact_until(&mut stdout, &mut raw, deadline, &cancel)?;
        check_cancel(&cancel)?;
        Ok(raw)
    })();
    let cleanup = child.terminate();
    match (result, cleanup) {
        (Ok(raw), Ok(())) => {
            check_cancel(&cancel)?;
            Ok(raw)
        }
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) => Err(OperationError::new(
            error.code,
            format!("{error}; capture cleanup: {cleanup}"),
        )),
    }
}

fn read_exact_until(
    stdout: &mut std::process::ChildStdout,
    mut bytes: &mut [u8],
    deadline: Instant,
    cancel: &AtomicBool,
) -> Result<()> {
    while !bytes.is_empty() {
        check_cancel(cancel)?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(incomplete());
        }
        let timeout = Timespec::try_from(remaining.min(Duration::from_millis(100)))
            .map_err(|e| OperationError::unavailable(format!("Capture poll timeout: {e}")))?;
        let mut fds = [PollFd::new(&*stdout, PollFlags::IN)];
        match poll(&mut fds, Some(&timeout)) {
            Ok(0) => continue,
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(error) => return Err(std::io::Error::from(error).into()),
        }
        check_cancel(cancel)?;
        match stdout.read(bytes) {
            Ok(0) => return Err(incomplete()),
            Ok(n) => bytes = &mut bytes[n..],
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
fn incomplete() -> OperationError {
    OperationError::unavailable(
        "The microphone delivered incomplete audio; check that it is connected and not suspended, then try again",
    )
}

#[derive(Debug)]
pub struct CalibrationEvent {
    pub token: CalibrationToken,
    pub result: Result<Metrics>,
}
struct Request {
    token: CalibrationToken,
    seconds: u32,
    cancel: Arc<AtomicBool>,
}
#[derive(Default)]
struct State {
    stopped: bool,
    active: Option<(CalibrationToken, Arc<AtomicBool>)>,
}
pub struct CalibrationWorker {
    state: Arc<Mutex<State>>,
    requests: Option<mpsc::Sender<Request>>,
    thread: Option<JoinHandle<()>>,
}
impl CalibrationWorker {
    pub fn start() -> Result<(Self, mpsc::Receiver<CalibrationEvent>)> {
        Self::start_with(Arc::new(capture_token))
    }
    pub(crate) fn start_with(
        capture: Arc<
            dyn Fn(&CalibrationToken, u32, Arc<AtomicBool>) -> Result<Vec<u8>> + Send + Sync,
        >,
    ) -> Result<(Self, mpsc::Receiver<CalibrationEvent>)> {
        let (requests, incoming) = mpsc::channel::<Request>();
        let (events, receiver) = mpsc::channel();
        let state = Arc::new(Mutex::new(State::default()));
        let shared = state.clone();
        let thread = thread::Builder::new()
            .name("openwave-calibration".into())
            .spawn(move || {
                for request in incoming {
                    let result = check_cancel(&request.cancel)
                        .and_then(|()| {
                            capture(&request.token, request.seconds, request.cancel.clone())
                        })
                        .and_then(|raw| {
                            check_cancel(&request.cancel)?;
                            calibration::metrics_from_raw(&raw, request.token.channels)
                        });
                    let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
                    let current = state.active.as_ref().is_some_and(|(token, flag)| {
                        *token == request.token && Arc::ptr_eq(flag, &request.cancel)
                    });
                    let result =
                        if !current || state.stopped || request.cancel.load(Ordering::Acquire) {
                            Err(cancelled())
                        } else {
                            result
                        };
                    if current {
                        state.active = None;
                    }
                    let _ = events.send(CalibrationEvent {
                        token: request.token,
                        result,
                    });
                }
            })?;
        Ok((
            Self {
                state,
                requests: Some(requests),
                thread: Some(thread),
            },
            receiver,
        ))
    }
    pub fn record(&self, token: CalibrationToken, seconds: u32) -> Result<()> {
        validate_capture(&token.node_name, seconds, token.channels)?;
        if seconds != calibration::FLOOR_SECONDS && seconds != calibration::SPEECH_SECONDS {
            return Err(OperationError::invalid(
                "Record the explicit three-second noise or five-second speech phase",
            ));
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.stopped {
            return Err(cancelled());
        }
        if let Some((active, cancel)) = &state.active {
            if active == &token {
                return Err(OperationError::new(
                    ErrorCode::Busy,
                    "Calibration recording already active",
                ));
            }
            cancel.store(true, Ordering::Release);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.requests
            .as_ref()
            .ok_or_else(cancelled)?
            .send(Request {
                token: token.clone(),
                seconds,
                cancel: cancel.clone(),
            })
            .map_err(|_| OperationError::unavailable("Calibration worker stopped"))?;
        state.active = Some((token, cancel));
        Ok(())
    }
    pub fn cancel(&self, token: &CalibrationToken) -> Result<()> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((active, cancel)) = &state.active {
            if active == token {
                cancel.store(true, Ordering::Release);
            }
        }
        Ok(())
    }
    pub fn stop(&mut self) -> Result<()> {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.stopped = true;
            if let Some((_, cancel)) = &state.active {
                cancel.store(true, Ordering::Release);
            }
        }
        self.requests.take();
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| OperationError::unavailable("Calibration worker panicked"))?;
        }
        Ok(())
    }
}
impl Drop for CalibrationWorker {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            log::error!("Calibration shutdown: {error}");
        }
    }
}

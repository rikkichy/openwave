// Compile the owned module with its private fixture seams; production process
// ownership stays real, and no test replaces USB/audio state or global PATH.
use openwave_runtime::process;
#[path = "../src/calibration.rs"]
mod calibration;
#[path = "../src/recovery.rs"]
mod recovery;

use calibration::{CalibrationWorker, capture_raw, capture_with};
use openwave_core::model::{CalibrationToken, ErrorCode, NodeIdentity, SourceId};
use std::{
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

fn token(session: u64) -> CalibrationToken {
    CalibrationToken {
        session,
        source: SourceId::new("mic").unwrap(),
        node_name: "raw_mic".into(),
        identity: NodeIdentity {
            server_cookie: 1,
            object_serial: session.to_string(),
        },
        channels: 1,
    }
}
fn stalled(pid: &AtomicU32) -> openwave_core::model::Result<process::OwnedChild> {
    let child = process::OwnedChild::spawn("sleep", &["30".into()], Stdio::piped())?;
    pid.store(child.id(), Ordering::Release);
    Ok(child)
}
fn assert_reaped(pid: &AtomicU32) {
    assert!(
        !std::path::Path::new(&format!("/proc/{}", pid.load(Ordering::Acquire))).exists(),
        "owned child must be killed and reaped"
    );
}

#[test]
fn cancelled_admission_never_spawns() {
    let error = capture_with(
        1,
        1,
        Arc::new(AtomicBool::new(true)),
        Duration::from_secs(1),
        || panic!("must not spawn"),
    )
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::Cancelled);
}
#[test]
fn cancellation_during_spawn_reaps_transferred_child() {
    let cancel = Arc::new(AtomicBool::new(false));
    let pid = AtomicU32::new(0);
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("ready");
    let error = capture_with(1, 1, cancel.clone(), Duration::from_secs(2), || {
        let child = process::OwnedChild::spawn(
            "sh",
            &[
                "-c".into(),
                "trap '' TERM; printf ready > \"$1\"; sleep 30".into(),
                "capture-fixture".into(),
                marker.to_string_lossy().into_owned(),
            ],
            Stdio::piped(),
        )?;
        pid.store(child.id(), Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !marker.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(
            marker.exists(),
            "fixture must ignore TERM before cancellation"
        );
        cancel.store(true, Ordering::Release);
        Ok(child)
    })
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::Cancelled);
    assert_reaped(&pid);
}
#[test]
fn silent_read_cancellation_is_bounded_and_reaped() {
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    let pid = AtomicU32::new(0);
    let (started, ready) = mpsc::channel();
    let timer = thread::spawn(move || {
        ready.recv().unwrap();
        thread::sleep(Duration::from_millis(50));
        flag.store(true, Ordering::Release);
    });
    let start = Instant::now();
    let error = capture_with(5, 1, cancel, Duration::from_secs(9), || {
        let child = stalled(&pid)?;
        started.send(()).unwrap();
        Ok(child)
    })
    .unwrap_err();
    timer.join().unwrap();
    assert_eq!(error.code, ErrorCode::Cancelled);
    assert!(start.elapsed() < Duration::from_secs(3));
    assert_reaped(&pid);
}
#[test]
fn deadline_reaps_stalled_child() {
    let pid = AtomicU32::new(0);
    let error = capture_with(
        1,
        1,
        Arc::new(AtomicBool::new(false)),
        Duration::from_millis(50),
        || stalled(&pid),
    )
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::Unavailable);
    assert_reaped(&pid);
}
#[test]
fn exact_frames_drop_the_entire_startup_transient() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("pcm");
    let transient = vec![0xff; 24_000 * 4];
    let expected = [0x23, 0x01, 0xcc, 0xfe].repeat(48_000);
    let mut bytes = transient;
    bytes.extend_from_slice(&expected);
    std::fs::write(&file, &bytes).unwrap();
    let raw = capture_with(
        1,
        2,
        Arc::new(AtomicBool::new(false)),
        Duration::from_secs(3),
        || {
            process::OwnedChild::spawn(
                "cat",
                &[file.to_string_lossy().into_owned()],
                Stdio::piped(),
            )
        },
    )
    .unwrap();
    assert_eq!(raw, expected);
    bytes.pop();
    std::fs::write(&file, &bytes).unwrap();
    assert!(
        capture_with(
            1,
            2,
            Arc::new(AtomicBool::new(false)),
            Duration::from_secs(3),
            || process::OwnedChild::spawn(
                "cat",
                &[file.to_string_lossy().into_owned()],
                Stdio::piped()
            )
        )
        .is_err()
    );
}
#[test]
fn invalid_target_or_channels_never_open_audio() {
    for target in ["", "0", "-1", "raw\0mic", "openwave_fx_mic"] {
        assert_eq!(
            capture_raw(target, 1, 1, Arc::new(AtomicBool::new(false)))
                .unwrap_err()
                .code,
            ErrorCode::Invalid
        );
    }
    assert_eq!(
        capture_raw("raw", 1, 3, Arc::new(AtomicBool::new(false)))
            .unwrap_err()
            .code,
        ErrorCode::Invalid
    );
}
#[test]
fn replacement_cancels_late_result_without_retargeting() {
    let (entered, ready) = mpsc::channel();
    let calls = Arc::new(AtomicU32::new(0));
    let count = calls.clone();
    let (mut worker, events) =
        CalibrationWorker::start_with(Arc::new(move |token, seconds, cancel| {
            if count.fetch_add(1, Ordering::AcqRel) == 0 {
                entered.send(()).unwrap();
                while !cancel.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(1));
                }
            }
            // Deliberately return a successful stale buffer after cancellation.
            Ok([1_u8, 0].repeat(48_000 * seconds as usize * token.channels as usize))
        }))
        .unwrap();
    let old = token(1);
    let next = token(2);
    worker.record(old.clone(), 3).unwrap();
    ready.recv_timeout(Duration::from_secs(2)).unwrap();
    worker.record(next.clone(), 5).unwrap();
    let cancelled = events.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(cancelled.token, old);
    assert_eq!(cancelled.result.unwrap_err().code, ErrorCode::Cancelled);
    let recorded = events.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(recorded.token, next);
    assert!(recorded.result.is_ok());
    worker.stop().unwrap();
    assert!(events.try_recv().is_err());
}
#[test]
fn stop_waits_for_capture_and_completes_cancellation() {
    let (entered, ready) = mpsc::channel();
    let (mut worker, events) = CalibrationWorker::start_with(Arc::new(move |_, _, cancel| {
        entered.send(()).unwrap();
        while !cancel.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(1));
        }
        Ok(vec![0; 48_000 * 2])
    }))
    .unwrap();
    worker.record(token(7), 3).unwrap();
    ready.recv_timeout(Duration::from_secs(2)).unwrap();
    worker.stop().unwrap();
    assert_eq!(
        events.recv().unwrap().result.unwrap_err().code,
        ErrorCode::Cancelled
    );
    assert!(worker.record(token(8), 3).is_err());
}

#[test]
fn changed_channels_cookie_and_missing_serial_expire_raw_capture_token() {
    struct Graph(serde_json::Value);
    impl recovery::Commands for Graph {
        fn cancelled(&self) -> bool {
            false
        }
        fn run(
            &self,
            program: &str,
            _: &[String],
            _: Duration,
            restoration: bool,
        ) -> openwave_core::model::Result<Vec<u8>> {
            assert_eq!(program, "pw-dump");
            assert!(!restoration);
            Ok(serde_json::to_vec(&self.0).unwrap())
        }
    }
    let token = token(11);
    let mut graph = Graph(serde_json::json!([
        {"type":"PipeWire:Interface:Core","info":{"cookie":1}},
        {"id":11,"type":"PipeWire:Interface:Node","info":{"state":"running","props":{
            "node.name":"raw_mic","media.class":"Audio/Source","object.serial":"11","audio.channels":1
        }}}
    ]));
    calibration::validate_token(&token, &graph).unwrap();
    graph.0[1]["info"]["props"]["audio.channels"] = serde_json::json!(2);
    assert_eq!(
        calibration::validate_token(&token, &graph)
            .unwrap_err()
            .code,
        ErrorCode::Identity
    );
    graph.0[1]["info"]["props"]["audio.channels"] = serde_json::json!(1);
    graph.0[0]["info"]["cookie"] = serde_json::json!(2);
    assert_eq!(
        calibration::validate_token(&token, &graph)
            .unwrap_err()
            .code,
        ErrorCode::Identity
    );
    graph.0[0]["info"]["cookie"] = serde_json::json!(1);
    graph.0[1]["info"]["props"]
        .as_object_mut()
        .unwrap()
        .remove("object.serial");
    assert_eq!(
        calibration::validate_token(&token, &graph)
            .unwrap_err()
            .code,
        ErrorCode::Identity
    );
}

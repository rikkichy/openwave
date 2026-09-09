use openwave_core::model::ErrorCode;
use openwave_runtime::process::{CommandRunner, OwnedChild};
use std::{
    fs,
    io::Read,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

fn args(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).into()).collect()
}
fn shell(script: &str) -> Vec<String> {
    args(&["-c", script])
}
fn live(pid: u32) -> bool {
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    !stat
        .rsplit_once(')')
        .expect("proc stat")
        .1
        .trim_start()
        .starts_with('Z')
}
fn wait_dead(pid: u32) {
    let start = Instant::now();
    while live(pid) && start.elapsed() < Duration::from_secs(2) {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!live(pid), "owned PID {pid} survived cleanup");
}

#[test]
fn drains_both_pipes_and_preserves_exact_argument_boundaries() {
    let result = CommandRunner::default().run("sh", &args(&["-c",
        "i=0; while [ $i -lt 6000 ]; do printf '0123456789012345678901234567890123456789'; printf 'abcdefghijabcdefghijabcdefghijabcdefghij' >&2; i=$((i+1)); done; printf '%s' \"$1\"",
        "fixture", "a b;$(not-a-command)\n'\"" ]), Duration::from_secs(5)).unwrap();
    assert_eq!(
        result.stderr,
        b"abcdefghijabcdefghijabcdefghijabcdefghij".repeat(6000)
    );
    let mut expected = b"0123456789012345678901234567890123456789".repeat(6000);
    expected.extend_from_slice(b"a b;$(not-a-command)\n'\"");
    assert_eq!(result.stdout, expected);
}

#[test]
fn deadline_kills_owned_group_not_unrelated_child() {
    let directory = tempfile::tempdir().unwrap();
    let pidfile = directory.path().join("pid");
    let mut unrelated = Command::new("sleep").arg("30").spawn().unwrap();
    let start = Instant::now();
    let result = CommandRunner::default().run(
        "sh",
        &args(&[
            "-c",
            "trap '' TERM; sleep 30 & printf '%s' \"$!\" > \"$1\"; wait",
            "fixture",
            pidfile.to_str().unwrap(),
        ]),
        Duration::from_millis(250),
    );
    let still_running = unrelated.try_wait().unwrap().is_none();
    unrelated.kill().unwrap();
    unrelated.wait().unwrap();
    assert!(still_running);
    assert!(result.is_err());
    assert!(start.elapsed() < Duration::from_secs(3));
    wait_dead(fs::read_to_string(pidfile).unwrap().parse().unwrap());
}

#[test]
fn cancelled_before_spawn_and_during_io_but_restoration_still_runs() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("should-not-exist");
    let cancel = Arc::new(AtomicBool::new(true));
    let runner = CommandRunner::new(Arc::clone(&cancel));
    let write = args(&[
        "-c",
        "printf wrong > \"$1\"",
        "fixture",
        file.to_str().unwrap(),
    ]);
    assert_eq!(
        runner
            .run("sh", &write, Duration::from_secs(1))
            .unwrap_err()
            .code,
        ErrorCode::Cancelled
    );
    assert!(!file.exists());
    cancel.store(false, Ordering::Release);
    let trigger = Arc::clone(&cancel);
    let thread = thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        trigger.store(true, Ordering::Release);
    });
    let result = runner.run(
        "sh",
        &shell("while :; do printf 0123456789; printf abcdefghij >&2; done"),
        Duration::from_secs(5),
    );
    thread.join().unwrap();
    assert_eq!(result.unwrap_err().code, ErrorCode::Cancelled);
    let restored = runner
        .run_uncancelled("sh", &shell("printf restored"), Duration::from_secs(1))
        .unwrap();
    assert_eq!(restored.stdout, b"restored");
}

#[test]
fn nonzero_status_is_error_unless_explicitly_requested() {
    let runner = CommandRunner::default();
    let invocation = shell("printf diagnostic >&2; exit 7");
    assert!(
        runner
            .run("sh", &invocation, Duration::from_secs(1))
            .is_err()
    );
    let result = runner
        .run_status("sh", &invocation, Duration::from_secs(1))
        .unwrap();
    assert_eq!(result.status.code(), Some(7));
    assert_eq!(result.stderr, b"diagnostic");
}

#[test]
fn child_stdout_and_drop_have_owned_lifetimes() {
    if rustix::process::geteuid().is_root() {
        return;
    } // exec-child intentionally refuses root.
    let mut child =
        OwnedChild::spawn("sh", &shell("printf ready; exec sleep 30"), Stdio::piped()).unwrap();
    let pid = child.id();
    let mut stdout = child.take_stdout().unwrap();
    let mut bytes = [0; 5];
    stdout.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"ready");
    assert!(child.try_wait().unwrap().is_none());
    drop(child);
    wait_dead(pid);
    assert_eq!(stdout.read(&mut bytes).unwrap(), 0);
}

#[test]
fn helper_refuses_parent_race_without_executing_program() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("must-not-execute");
    let output = Command::new(env!("CARGO_BIN_EXE_openwave-maintenance"))
        .args([
            "exec-child",
            "--parent-pid",
            "2147483647",
            "--",
            "sh",
            "-c",
            "printf wrong > \"$1\"",
            "fixture",
        ])
        .arg(&file)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!file.exists());
}

#[test]
fn parent_death_terminates_session_child() {
    if rustix::process::geteuid().is_root() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let ready = directory.path().join("ready");
    let child_id = directory.path().join("child");
    let invocation = args(&[
        "-c",
        "\"$1\" exec-child --parent-pid $$ -- sh -c 'printf ready > \"$1\"; exec sleep 30' fixture \"$2\" & printf '%s' \"$!\" > \"$3\"; while [ ! -f \"$2\" ]; do sleep 0.01; done",
        "fixture",
        env!("CARGO_BIN_EXE_openwave-maintenance"),
        ready.to_str().unwrap(),
        child_id.to_str().unwrap(),
    ]);
    CommandRunner::default()
        .run("sh", &invocation, Duration::from_secs(3))
        .unwrap();
    wait_dead(fs::read_to_string(child_id).unwrap().parse().unwrap());
}

#[test]
fn immediate_child_cancellation_cannot_signal_parent_group() {
    if rustix::process::geteuid().is_root() {
        return;
    }
    for _ in 0..12 {
        let mut child = OwnedChild::spawn("sleep", &args(&["30"]), Stdio::null()).unwrap();
        let pid = child.id();
        child.terminate().unwrap();
        wait_dead(pid);
    }
}

#[test]
fn hidden_supervision_cannot_bypass_parent_or_root_checks() {
    let directory = tempfile::tempdir().unwrap();
    let destination = directory.path().join("must-not-create");
    for marker in [
        args(&["--worker-parent", &std::process::id().to_string()]),
        args(&["--worker-parent", "2147483647"]),
        args(&[
            "--supervised-parent",
            &std::process::id().to_string(),
            "--supervised-group",
            "1",
            "--login-uid",
            "1000",
            "--login-gid",
            "1000",
        ]),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_openwave-maintenance"))
            .args(marker)
            .args(["files-only", "--yes", "--prefix"])
            .arg(&destination)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!destination.exists());
    }
    let output = Command::new(env!("CARGO_BIN_EXE_openwave-maintenance"))
        .args([
            "--login-uid",
            "1000",
            "--login-gid",
            "1000",
            "quiesce-legacy",
            "--prefix",
        ])
        .arg(&destination)
        .output()
        .unwrap();
    assert!(!output.status.success());
}

#[test]
fn cancellation_during_owned_group_io_leaves_unrelated_group_alive() {
    let directory = tempfile::tempdir().unwrap();
    let descendant = directory.path().join("descendant");
    let mut unrelated = Command::new("sleep").arg("30").spawn().unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let trigger = cancel.clone();
    let ready = descendant.clone();
    let request = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !ready.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        trigger.store(true, Ordering::Release);
    });
    let started = Instant::now();
    let result = CommandRunner::new(cancel).run("sh", &args(&["-c",
        "trap '' TERM; sleep 30 & printf '%s' \"$!\" > \"$1\"; while :; do printf noise; printf noise >&2; done",
        "fixture", descendant.to_str().unwrap()]), Duration::from_secs(5));
    request.join().unwrap();
    let unrelated_alive = unrelated.try_wait().unwrap().is_none();
    unrelated.kill().unwrap();
    unrelated.wait().unwrap();
    assert_eq!(result.unwrap_err().code, ErrorCode::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(unrelated_alive);
    wait_dead(fs::read_to_string(descendant).unwrap().parse().unwrap());
}

use openwave_runtime::process;
#[path = "../src/health.rs"]
mod health;
#[path = "../src/recovery.rs"]
mod recovery;
use openwave_core::{
    health::HealthWatch,
    model::{ErrorCode, NodeIdentity, Observation, OperationError, Result},
};
use recovery::Commands;
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::HashMap,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
const SOURCE: &str = "alsa_input.usb-Elgato_Systems_Elgato_XLR_Dock_A-00.mono-fallback";
const SINK: &str = "alsa_output.fixture";
const CARD: &str = "alsa_card.fixture";
fn identity() -> NodeIdentity {
    NodeIdentity {
        server_cookie: 7,
        object_serial: "11".into(),
    }
}
struct Fixture {
    cancel: Arc<AtomicBool>,
    counter: AtomicU32,
    serial: AtomicU32,
    graph_reads: AtomicU32,
    malformed: bool,
    replace_after: u32,
    bad_mute: bool,
    fail_mapping: bool,
    output: bool,
    attempts: Arc<AtomicU32>,
    profile: RefCell<String>,
    entered: Option<mpsc::Sender<()>>,
    release: Option<mpsc::Receiver<()>>,
    restored: Arc<AtomicBool>,
    block_collection: bool,
    collection_ended: Arc<AtomicBool>,
    fail_restore_observation: Arc<AtomicBool>,
    cancel_on_change: bool,
    replacement: Arc<AtomicBool>,
    restoration_observations: Option<mpsc::Sender<Option<u64>>>,
}
impl Default for Fixture {
    fn default() -> Self {
        Self {
            cancel: Arc::new(AtomicBool::new(false)),
            counter: AtomicU32::new(0),
            serial: AtomicU32::new(11),
            graph_reads: AtomicU32::new(0),
            malformed: false,
            replace_after: u32::MAX,
            bad_mute: false,
            fail_mapping: false,
            output: false,
            attempts: Arc::new(AtomicU32::new(0)),
            profile: RefCell::new("pro-audio".into()),
            entered: None,
            release: None,
            restored: Arc::new(AtomicBool::new(false)),
            block_collection: false,
            collection_ended: Arc::new(AtomicBool::new(false)),
            fail_restore_observation: Arc::new(AtomicBool::new(false)),
            cancel_on_change: false,
            replacement: Arc::new(AtomicBool::new(false)),
            restoration_observations: None,
        }
    }
}
impl Fixture {
    fn graph(&self) -> Value {
        let cookie = if self.replacement.load(Ordering::Acquire)
            || self.graph_reads.fetch_add(1, Ordering::AcqRel) >= self.replace_after
        {
            8
        } else {
            7
        };
        let mut value = json!([
            {"type":"PipeWire:Interface:Core","info":{"cookie":cookie}},
            {"id":2,"type":"PipeWire:Interface:Device","info":{"props":{"device.name":CARD,"object.serial":"12"}}},
            {"id":1,"type":"PipeWire:Interface:Node","info":{"state":"running","props":{"node.name":SOURCE,"media.class":"Audio/Source","object.serial":self.serial.load(Ordering::Acquire).to_string()}}}
        ]);
        if self.malformed {
            value[2]["info"]["props"] = json!([]);
        }
        if self.output {
            value.as_array_mut().unwrap().extend([
                json!({"id":3,"type":"PipeWire:Interface:Node","info":{"state":"running","props":{"node.name":SINK,"media.class":"Audio/Sink","object.serial":"13","api.alsa.pcm.card":4,"api.alsa.pcm.device":2,"api.alsa.pcm.subdevice":1}}}),
                json!({"id":4,"type":"PipeWire:Interface:Node","info":{"state":"running","props":{"node.name":"openwave_loop_output_personal","object.serial":"14","target.object":"alsa_output.wrong_hint"}}}),
                json!({"id":5,"type":"PipeWire:Interface:Link","info":{"output-node-id":4,"input-node-id":3}}),
            ]);
        }
        value
    }
}
impl Commands for Fixture {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Acquire)
    }
    fn wait(&self, _: Duration) {}
    fn run(
        &self,
        program: &str,
        args: &[String],
        timeout: Duration,
        restoration: bool,
    ) -> Result<Vec<u8>> {
        assert_eq!(
            timeout,
            Duration::from_secs(if program == "pw-top" { 15 } else { 5 })
        );
        if self.cancelled() && !restoration {
            return Err(OperationError::new(ErrorCode::Cancelled, "stopped"));
        }
        if program == "pw-top" {
            if self.block_collection {
                self.entered.as_ref().unwrap().send(()).unwrap();
                while !self.cancelled() {
                    thread::sleep(Duration::from_millis(1));
                }
                self.collection_ended.store(true, Ordering::Release);
                return Err(OperationError::new(
                    ErrorCode::Cancelled,
                    "collector stopped",
                ));
            }
            return Ok(format!(
                "R 1 128 48000 0 0 0 0 {} S16LE {SOURCE}\n",
                self.counter.load(Ordering::Acquire)
            )
            .into_bytes());
        }
        let value = if program == "pw-dump" {
            if restoration && self.fail_restore_observation.load(Ordering::Acquire) {
                if let Some(observations) = &self.restoration_observations {
                    observations.send(None).unwrap();
                }
                return Err(OperationError::unavailable(
                    "restoration observation unavailable",
                ));
            }
            let graph = self.graph();
            if restoration {
                if let Some(observations) = &self.restoration_observations {
                    observations
                        .send(graph[0]["info"]["cookie"].as_u64())
                        .unwrap();
                }
            }
            graph
        } else if args.first().map(String::as_str) == Some("--format=json") {
            match args[2].as_str() {
                "sources" => {
                    json!([{"name":SOURCE,"mute":if self.bad_mute { json!(null) } else { json!(false) },"card":4,"properties":{"object.serial":self.serial.load(Ordering::Acquire).to_string()}}])
                }
                "sinks" => json!([{"name":SINK,"mute":false,"properties":{"object.serial":"13"}}]),
                "cards" => {
                    if self.fail_mapping {
                        self.attempts.fetch_add(1, Ordering::AcqRel);
                        return Err(OperationError::unavailable("card discovery failed"));
                    }
                    json!([{"index":4,"name":CARD,"active_profile":self.profile.borrow().clone()}])
                }
                _ => panic!("unexpected listing"),
            }
        } else {
            assert_eq!(args[0], "set-card-profile");
            assert_eq!(args[1], CARD);
            if args[2] == "off" {
                assert!(!restoration);
                self.attempts.fetch_add(1, Ordering::AcqRel);
                *self.profile.borrow_mut() = "off".into();
                if self.cancel_on_change {
                    self.cancel.store(true, Ordering::Release);
                }
                if let Some(entered) = &self.entered {
                    entered.send(()).unwrap();
                }
            } else {
                assert!(restoration);
                if let Some(release) = &self.release {
                    release.recv_timeout(Duration::from_secs(3)).unwrap();
                }
                *self.profile.borrow_mut() = args[2].clone();
                assert_eq!(args[2], "pro-audio");
                self.restored.store(true, Ordering::Release);
            }
            return Ok(vec![]);
        };
        Ok(serde_json::to_vec(&value).unwrap())
    }
}
fn ages(age: u64) -> HashMap<NodeIdentity, Duration> {
    HashMap::from([(identity(), Duration::from_secs(age))])
}
fn sample(fixture: &Fixture, age: u64) -> Observation<openwave_core::health::HealthSamples> {
    health::collect_observation(
        fixture,
        &|| ages(age),
        Path::new("/nonexistent-openwave-fixture"),
    )
}
#[test]
fn generation_scoped_bytes_and_coherent_counts_are_required() {
    let fixture = Fixture::default();
    let samples = sample(&fixture, 0);
    let capture = samples.known().unwrap().captures[SOURCE].known().unwrap();
    assert_eq!(capture.byte_age.known(), Some(&Duration::ZERO));
    assert_eq!(capture.xruns.known(), Some(&0));
    fixture.serial.store(99, Ordering::Release);
    let changed = sample(&fixture, 0);
    assert!(matches!(
        changed.known().unwrap().captures[SOURCE]
            .known()
            .unwrap()
            .byte_age,
        Observation::Unknown(_)
    ));
    let restarted = Fixture {
        replace_after: 1,
        ..Fixture::default()
    };
    assert!(matches!(sample(&restarted, 0), Observation::Unknown(_)));
}
#[test]
fn nested_malformed_and_collector_panic_pause_the_clean_interval() {
    let mut restoration = recovery::Restoration::default();
    let fixture = Fixture::default();
    let mut watch = HealthWatch::default();
    watch.observe(&sample(&fixture, 0), Duration::ZERO);
    watch.capture.record_attempt(SOURCE, Duration::ZERO);
    watch.observe(&sample(&fixture, 0), Duration::from_secs(1));
    watch.observe(&sample(&fixture, 0), Duration::from_secs(2));
    let failed = health::collect_observation(
        &fixture,
        &|| panic!("injected generation collector failure"),
        Path::new("/nonexistent"),
    );
    assert!(matches!(failed, Observation::Unknown(_)));
    watch.observe(&failed, Duration::from_secs(299));
    watch.observe(&sample(&fixture, 0), Duration::from_secs(302));
    assert_eq!(watch.capture.budget.spent(SOURCE), 1);
    assert!(matches!(
        sample(
            &Fixture {
                malformed: true,
                ..Fixture::default()
            },
            0
        ),
        Observation::Unknown(_)
    ));
    let bad_mute = sample(
        &Fixture {
            bad_mute: true,
            ..Fixture::default()
        },
        9,
    );
    let report = health::check_once(
        &mut watch,
        &fixture,
        true,
        bad_mute,
        Duration::from_secs(400),
        &mut restoration,
    );
    assert!(!report.captures[SOURCE].recovery_due);
    assert_eq!(fixture.attempts.load(Ordering::Acquire), 0);
}
#[test]
fn observe_only_never_spends_or_mutates_and_mapping_failures_are_bounded() {
    let mut restoration = recovery::Restoration::default();
    let fixture = Fixture {
        fail_mapping: true,
        ..Fixture::default()
    };
    let mut watch = HealthWatch::default();
    let report = health::check_once(
        &mut watch,
        &fixture,
        false,
        sample(&fixture, 9),
        Duration::ZERO,
        &mut restoration,
    );
    assert!(report.captures[SOURCE].no_data);
    assert_eq!(watch.capture.budget.spent(SOURCE), 0);
    assert_eq!(fixture.attempts.load(Ordering::Acquire), 0);
    for now in [0, 10, 60, 120, 500] {
        health::check_once(
            &mut watch,
            &fixture,
            true,
            sample(&fixture, 9),
            Duration::from_secs(now),
            &mut restoration,
        );
    }
    assert_eq!(fixture.attempts.load(Ordering::Acquire), 2);
    assert_eq!(watch.capture.budget.spent(SOURCE), 2);
    watch.observe(
        &Observation::Unknown(OperationError::unavailable("graph absent")),
        Duration::from_secs(600),
    );
    fixture.serial.store(99, Ordering::Release);
    health::check_once(
        &mut watch,
        &fixture,
        true,
        sample(&fixture, 9),
        Duration::from_secs(900),
        &mut restoration,
    );
    assert_eq!(watch.capture.budget.spent(SOURCE), 2);
}
#[test]
fn output_collection_reads_the_actually_linked_alsa_substream() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("card4/pcm2p/sub1");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("status"), "state: RUNNING\nhw_ptr : 712\n").unwrap();
    let fixture = Fixture {
        output: true,
        ..Fixture::default()
    };
    let samples = health::collect_samples(&fixture, &|| ages(0), dir.path()).unwrap();
    assert_eq!(
        samples.sinks[SINK]
            .known()
            .unwrap()
            .playback
            .known()
            .unwrap()
            .hw_ptr,
        712
    );
    std::fs::write(sub.join("status"), "state: RUNNING\nhw_ptr : unknown\n").unwrap();
    let samples = health::collect_samples(&fixture, &|| ages(0), dir.path()).unwrap();
    assert!(matches!(
        samples.sinks[SINK].known().unwrap().playback,
        Observation::Unknown(_)
    ));
}
#[test]
fn stop_cancels_collection_before_join_returns() {
    let (entered, ready) = mpsc::channel();
    let fixture = Fixture {
        entered: Some(entered),
        block_collection: true,
        ..Fixture::default()
    };
    let cancel = fixture.cancel.clone();
    let ended = fixture.collection_ended.clone();
    let mut monitor =
        health::HealthMonitor::start_with(true, Arc::new(|| ages(9)), cancel, fixture).unwrap();
    ready.recv_timeout(Duration::from_secs(2)).unwrap();
    monitor.stop().unwrap();
    assert!(ended.load(Ordering::Acquire));
}
#[test]
fn stop_waits_for_owed_profile_restoration() {
    let (entered, ready) = mpsc::channel();
    let (release, restoring) = mpsc::channel();
    let fixture = Fixture {
        entered: Some(entered),
        release: Some(restoring),
        ..Fixture::default()
    };
    let cancel = fixture.cancel.clone();
    let restored = fixture.restored.clone();
    let mut monitor =
        health::HealthMonitor::start_with(true, Arc::new(|| ages(9)), cancel.clone(), fixture)
            .unwrap();
    ready.recv_timeout(Duration::from_secs(2)).unwrap();
    let (stopped, done) = mpsc::channel();
    let stopper = thread::spawn(move || {
        let result = monitor.stop();
        stopped.send(result).unwrap();
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    while !cancel.load(Ordering::Acquire) && Instant::now() < deadline {
        thread::yield_now();
    }
    assert!(cancel.load(Ordering::Acquire));
    assert!(done.try_recv().is_err());
    assert!(!restored.load(Ordering::Acquire));
    release.send(()).unwrap();
    done.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
    stopper.join().unwrap();
    assert!(restored.load(Ordering::Acquire));
}

#[test]
fn owed_profile_retries_without_spending_budget_or_opt_in() {
    let fixture = Fixture {
        fail_restore_observation: Arc::new(AtomicBool::new(true)),
        ..Fixture::default()
    };
    let mut watch = HealthWatch::default();
    let mut restoration = recovery::Restoration::default();
    let report = health::check_once(
        &mut watch,
        &fixture,
        true,
        sample(&fixture, 9),
        Duration::ZERO,
        &mut restoration,
    );
    assert!(report.error.is_some());
    assert_eq!(*fixture.profile.borrow(), "off");
    assert_eq!(watch.capture.budget.spent(SOURCE), 1);
    // A new incident would be eligible at this time, but debt takes precedence.
    let report = health::check_once(
        &mut watch,
        &fixture,
        true,
        sample(&fixture, 9),
        Duration::from_secs(60),
        &mut restoration,
    );
    assert!(report.error.is_some());
    assert_eq!(watch.capture.budget.spent(SOURCE), 1);
    fixture
        .fail_restore_observation
        .store(false, Ordering::Release);
    fixture.cancel.store(true, Ordering::Release);
    let report = health::check_once(
        &mut watch,
        &fixture,
        false,
        Observation::Unknown(OperationError::unavailable("collection cancelled")),
        Duration::from_secs(61),
        &mut restoration,
    );
    assert!(report.error.is_some()); // Unknown collection is still not healthy.
    assert_eq!(*fixture.profile.borrow(), "pro-audio");
    assert!(!restoration.is_pending());
    assert_eq!(watch.capture.budget.spent(SOURCE), 1);
    assert_eq!(fixture.attempts.load(Ordering::Acquire), 1);
}

#[test]
fn stop_retains_owed_profile_until_original_identity_can_be_restored() {
    let (entered, ready) = mpsc::channel();
    let fixture = Fixture {
        entered: Some(entered),
        cancel_on_change: true,
        fail_restore_observation: Arc::new(AtomicBool::new(true)),
        ..Fixture::default()
    };
    let cancel = fixture.cancel.clone();
    let unavailable = fixture.fail_restore_observation.clone();
    let replacement = fixture.replacement.clone();
    let restored = fixture.restored.clone();
    let attempts = fixture.attempts.clone();
    let mut monitor =
        health::HealthMonitor::start_with(true, Arc::new(|| ages(9)), cancel, fixture).unwrap();
    ready.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(monitor.stop().is_err());
    assert!(!restored.load(Ordering::Acquire));
    assert!(monitor.stop().is_err());
    unavailable.store(false, Ordering::Release);
    replacement.store(true, Ordering::Release);
    assert!(monitor.stop().is_err());
    assert!(!restored.load(Ordering::Acquire));
    replacement.store(false, Ordering::Release);
    monitor.stop().unwrap();
    assert!(restored.load(Ordering::Acquire));
    assert_eq!(attempts.load(Ordering::Acquire), 1);
    monitor.stop().unwrap();
}

#[test]
fn daemon_shutdown_retains_leases_until_original_profile_is_restored() {
    let directory = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "daemon_shutdown_private_fixture", "--nocapture"])
        .env("OPENWAVE_HEALTH_SHUTDOWN_FIXTURE", directory.path())
        .env("HOME", directory.path())
        .env("XDG_CONFIG_HOME", "")
        .env("XDG_DATA_HOME", "")
        .env("XDG_STATE_HOME", "")
        .env("XDG_RUNTIME_DIR", directory.path().join("runtime"))
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .current_dir("/")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn daemon_shutdown_private_fixture() {
    use openwave_runtime::paths::Lease;

    let Some(root) = std::env::var_os("OPENWAVE_HEALTH_SHUTDOWN_FIXTURE") else {
        return;
    };
    let identity = Path::new(&root);
    let installation = Lease::installation_shared(identity).unwrap();
    let daemon = Lease::capture_daemon().unwrap();
    let (entered, ready) = mpsc::channel();
    let (observations, observed) = mpsc::channel();
    let fixture = Fixture {
        entered: Some(entered),
        cancel_on_change: true,
        fail_restore_observation: Arc::new(AtomicBool::new(true)),
        restoration_observations: Some(observations),
        ..Fixture::default()
    };
    let cancel = fixture.cancel.clone();
    let unavailable = fixture.fail_restore_observation.clone();
    let replacement = fixture.replacement.clone();
    let restored = fixture.restored.clone();
    let attempts = fixture.attempts.clone();
    let mut monitor =
        health::HealthMonitor::start_with(true, Arc::new(|| ages(9)), cancel, fixture).unwrap();
    ready.recv_timeout(Duration::from_secs(2)).unwrap();
    // The initial remedy's restore fails before shutdown takes over the debt.
    assert_eq!(observed.recv_timeout(Duration::from_secs(2)).unwrap(), None);
    let (stopped, done) = mpsc::channel();
    let stopper = thread::spawn(move || {
        let result = monitor.stop_until_restored();
        drop(monitor);
        drop(daemon);
        drop(installation);
        stopped.send(result).unwrap();
    });
    assert_eq!(observed.recv_timeout(Duration::from_secs(4)).unwrap(), None);
    assert!(matches!(done.try_recv(), Err(mpsc::TryRecvError::Empty)));
    assert_eq!(Lease::capture_daemon().unwrap_err().code, ErrorCode::Busy);
    assert_eq!(
        Lease::installation_exclusive(identity).unwrap_err().code,
        ErrorCode::Busy
    );

    replacement.store(true, Ordering::Release);
    unavailable.store(false, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(6);
    let mut replacement_observations = 0;
    // A second attempt proves the first replacement observation did not
    // discharge the obligation and release ownership.
    while replacement_observations < 2 {
        match observed
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap()
        {
            Some(cookie) => {
                assert_eq!(cookie, 8);
                replacement_observations += 1;
            }
            None => {}
        }
    }
    assert!(!restored.load(Ordering::Acquire));
    assert!(matches!(done.try_recv(), Err(mpsc::TryRecvError::Empty)));
    assert_eq!(Lease::capture_daemon().unwrap_err().code, ErrorCode::Busy);
    assert_eq!(
        Lease::installation_exclusive(identity).unwrap_err().code,
        ErrorCode::Busy
    );

    replacement.store(false, Ordering::Release);
    done.recv_timeout(Duration::from_secs(4)).unwrap().unwrap();
    stopper.join().unwrap();
    assert!(restored.load(Ordering::Acquire));
    assert_eq!(attempts.load(Ordering::Acquire), 1);
    drop(Lease::capture_daemon().unwrap());
    drop(Lease::installation_exclusive(identity).unwrap());
}

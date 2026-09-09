use openwave_runtime::process;
#[path = "../src/recovery.rs"]
mod recovery;
use openwave_core::model::{ErrorCode, NodeIdentity, OperationError, Result};
use recovery::{Commands, cycle_card_with, recycle_sink_with};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
    time::Duration,
};
const SOURCE: &str = "alsa_input.usb-Elgato_Systems_Elgato_XLR_Dock_A-00.mono-fallback";
const SINK: &str = "alsa_output.fixture";
const CARD: &str = "alsa_card.fixture";

struct Fixture {
    cancel: AtomicBool,
    cancel_on_change: bool,
    fail_change: bool,
    fail_restore: bool,
    fail_restore_observation: AtomicBool,
    graph_reads: AtomicU32,
    replace_after: u32,
    wrong_mapping: bool,
    profile: RefCell<String>,
    suspended: AtomicBool,
    mutations: RefCell<Vec<Vec<String>>>,
}
impl Default for Fixture {
    fn default() -> Self {
        Self {
            cancel: AtomicBool::new(false),
            cancel_on_change: false,
            fail_change: false,
            fail_restore: false,
            fail_restore_observation: AtomicBool::new(false),
            graph_reads: AtomicU32::new(0),
            replace_after: u32::MAX,
            wrong_mapping: false,
            profile: RefCell::new("pro-audio".into()),
            suspended: AtomicBool::new(false),
            mutations: RefCell::new(vec![]),
        }
    }
}
fn identity(serial: &str) -> NodeIdentity {
    NodeIdentity {
        server_cookie: 7,
        object_serial: serial.into(),
    }
}
impl Fixture {
    fn graph(&self) -> Value {
        let cookie = if self.graph_reads.fetch_add(1, Ordering::AcqRel) >= self.replace_after {
            8
        } else {
            7
        };
        let mut graph = json!([
            {"type":"PipeWire:Interface:Core","info":{"cookie":cookie}},
            {"id":2,"type":"PipeWire:Interface:Device","info":{"props":{"device.name":CARD,"object.serial":"12"}}},
            {"id":1,"type":"PipeWire:Interface:Node","info":{"state":"running","props":{"node.name":SOURCE,"media.class":"Audio/Source","object.serial":"11"}}},
            {"id":3,"type":"PipeWire:Interface:Node","info":{"state":"running","props":{"node.name":SINK,"media.class":"Audio/Sink","object.serial":"13"}}}
        ]);
        if *self.profile.borrow() == "off" {
            graph
                .as_array_mut()
                .unwrap()
                .retain(|entry| entry["type"] != "PipeWire:Interface:Node");
        }
        graph
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
        assert!(timeout <= Duration::from_secs(5));
        if self.cancelled() && !restoration {
            return Err(OperationError::new(ErrorCode::Cancelled, "cancelled"));
        }
        let value = if program == "pw-dump" {
            if restoration && self.fail_restore_observation.swap(false, Ordering::AcqRel) {
                return Err(OperationError::unavailable(
                    "transient restoration observation",
                ));
            }
            self.graph()
        } else if args.first().map(String::as_str) == Some("--format=json") {
            match args[2].as_str() {
                "sources" => {
                    json!([{"name":SOURCE,"card":if self.wrong_mapping { 42 } else { 4 },"properties":{"object.serial":"11"}}])
                }
                "sinks" => json!([{"name":SINK,"card":4,"properties":{"object.serial":"13"}}]),
                "cards" => {
                    json!([{"index":4,"name":CARD,"active_profile":{"name":self.profile.borrow().clone()}},{"index":9,"name":"alsa_card.other","active_profile":"off"}])
                }
                _ => panic!("unexpected listing"),
            }
        } else {
            assert_eq!(program, "pactl");
            self.mutations.borrow_mut().push(args.to_vec());
            let changing = match args[0].as_str() {
                "set-card-profile" => {
                    assert_eq!(args[1], CARD);
                    let off = args[2] == "off";
                    if !(restoration && self.fail_restore) {
                        *self.profile.borrow_mut() = args[2].clone();
                    }
                    off
                }
                "suspend-sink" => {
                    assert_eq!(args[1], SINK);
                    let suspend = args[2] == "1";
                    if !(restoration && self.fail_restore) {
                        self.suspended.store(suspend, Ordering::Release);
                    }
                    suspend
                }
                _ => panic!("unexpected mutation"),
            };
            assert_eq!(restoration, !changing);
            if changing && self.cancel_on_change {
                self.cancel.store(true, Ordering::Release);
            }
            if (changing && self.fail_change) || (!changing && self.fail_restore) {
                return Err(OperationError::unavailable(
                    "injected timeout after possible application",
                ));
            }
            return Ok(vec![]);
        };
        Ok(serde_json::to_vec(&value).unwrap())
    }
}
#[test]
fn cancellation_after_off_restores_original_profile_not_an_alternative() {
    let mut restoration = recovery::Restoration::default();
    let fixture = Fixture {
        cancel_on_change: true,
        ..Fixture::default()
    };
    cycle_card_with(SOURCE, &identity("11"), &fixture, &mut restoration).unwrap();
    assert!(fixture.cancelled());
    assert_eq!(*fixture.profile.borrow(), "pro-audio");
    assert_eq!(fixture.mutations.borrow().len(), 2);
}
#[test]
fn failed_off_still_restores_potentially_applied_profile() {
    let mut restoration = recovery::Restoration::default();
    let fixture = Fixture {
        fail_change: true,
        ..Fixture::default()
    };
    assert!(cycle_card_with(SOURCE, &identity("11"), &fixture, &mut restoration).is_err());
    assert_eq!(*fixture.profile.borrow(), "pro-audio");
}
#[test]
fn wrong_mapping_or_replaced_identity_never_switches_a_card() {
    let mut restoration = recovery::Restoration::default();
    let wrong = Fixture {
        wrong_mapping: true,
        ..Fixture::default()
    };
    assert!(cycle_card_with(SOURCE, &identity("11"), &wrong, &mut restoration).is_err());
    assert!(wrong.mutations.borrow().is_empty());
    let replaced = Fixture {
        replace_after: 1,
        ..Fixture::default()
    };
    assert_eq!(
        cycle_card_with(SOURCE, &identity("11"), &replaced, &mut restoration)
            .unwrap_err()
            .code,
        ErrorCode::Identity
    );
    assert!(replaced.mutations.borrow().is_empty());
}
#[test]
fn already_off_and_precancelled_are_not_mutated() {
    let mut restoration = recovery::Restoration::default();
    let off = Fixture::default();
    *off.profile.borrow_mut() = "off".into();
    assert!(cycle_card_with(SOURCE, &identity("11"), &off, &mut restoration).is_err());
    assert!(off.mutations.borrow().is_empty());
    let cancelled = Fixture::default();
    cancelled.cancel.store(true, Ordering::Release);
    assert_eq!(
        cycle_card_with(SOURCE, &identity("11"), &cancelled, &mut restoration)
            .unwrap_err()
            .code,
        ErrorCode::Cancelled
    );
    assert!(cancelled.mutations.borrow().is_empty());
}
#[test]
fn failed_suspend_and_cancellation_both_owe_resume() {
    let mut restoration = recovery::Restoration::default();
    let fixture = Fixture {
        cancel_on_change: true,
        fail_change: true,
        ..Fixture::default()
    };
    assert!(recycle_sink_with(SINK, &identity("13"), &fixture, &mut restoration).is_err());
    assert!(!fixture.suspended.load(Ordering::Acquire));
    assert_eq!(fixture.mutations.borrow().len(), 2);
}
#[test]
fn successful_suspend_is_resumed_even_after_cancellation() {
    let mut restoration = recovery::Restoration::default();
    let fixture = Fixture {
        cancel_on_change: true,
        ..Fixture::default()
    };
    recycle_sink_with(SINK, &identity("13"), &fixture, &mut restoration).unwrap();
    assert!(!fixture.suspended.load(Ordering::Acquire));
}
#[test]
fn restoration_failure_is_not_reported_as_recovery() {
    let mut restoration = recovery::Restoration::default();
    let fixture = Fixture {
        fail_restore: true,
        ..Fixture::default()
    };
    assert!(cycle_card_with(SOURCE, &identity("11"), &fixture, &mut restoration).is_err());
    assert_eq!(*fixture.profile.borrow(), "off");
}
#[test]
fn server_replacement_cannot_authorize_resume_of_another_sink() {
    let mut restoration = recovery::Restoration::default();
    let fixture = Fixture {
        replace_after: 1,
        ..Fixture::default()
    };
    assert!(recycle_sink_with(SINK, &identity("13"), &fixture, &mut restoration).is_err());
    assert_eq!(fixture.mutations.borrow().len(), 1);
}

#[test]
fn original_profile_remains_owed_after_transient_observation_and_cancellation() {
    let fixture = Fixture {
        cancel_on_change: true,
        fail_restore_observation: AtomicBool::new(true),
        profile: RefCell::new("output:analog-stereo+input:mono-fallback".into()),
        ..Fixture::default()
    };
    let mut restoration = recovery::Restoration::default();
    assert!(cycle_card_with(SOURCE, &identity("11"), &fixture, &mut restoration).is_err());
    assert_eq!(*fixture.profile.borrow(), "off");
    assert!(restoration.is_pending());
    restoration.restore_with(&fixture).unwrap();
    assert_eq!(
        *fixture.profile.borrow(),
        "output:analog-stereo+input:mono-fallback"
    );
    assert!(!restoration.is_pending());
    assert_eq!(fixture.mutations.borrow().len(), 2);
}

#[test]
fn sink_resume_remains_owed_after_transient_observation_and_cancellation() {
    let fixture = Fixture {
        cancel_on_change: true,
        fail_restore_observation: AtomicBool::new(true),
        ..Fixture::default()
    };
    let mut restoration = recovery::Restoration::default();
    assert!(recycle_sink_with(SINK, &identity("13"), &fixture, &mut restoration).is_err());
    assert!(fixture.suspended.load(Ordering::Acquire));
    restoration.restore_with(&fixture).unwrap();
    assert!(!fixture.suspended.load(Ordering::Acquire));
    assert!(!restoration.is_pending());
    assert_eq!(fixture.mutations.borrow().len(), 2);
}

#[test]
fn owed_card_profile_never_mutates_replacement_or_accepts_new_recovery() {
    let mut fixture = Fixture {
        fail_restore_observation: AtomicBool::new(true),
        ..Fixture::default()
    };
    let mut restoration = recovery::Restoration::default();
    assert!(cycle_card_with(SOURCE, &identity("11"), &fixture, &mut restoration).is_err());
    fixture.replace_after = 0;
    assert!(restoration.restore_with(&fixture).is_err());
    assert!(restoration.is_pending());
    assert!(recycle_sink_with(SINK, &identity("13"), &fixture, &mut restoration).is_err());
    assert_eq!(fixture.mutations.borrow().len(), 1);
    assert_eq!(*fixture.profile.borrow(), "off");
    fixture.replace_after = u32::MAX;
    restoration.restore_with(&fixture).unwrap();
    assert_eq!(*fixture.profile.borrow(), "pro-audio");
    assert!(!restoration.is_pending());
}

#[test]
fn owed_resume_never_mutates_replacement_and_survives_until_original_returns() {
    let mut fixture = Fixture {
        fail_restore_observation: AtomicBool::new(true),
        ..Fixture::default()
    };
    let mut restoration = recovery::Restoration::default();
    assert!(recycle_sink_with(SINK, &identity("13"), &fixture, &mut restoration).is_err());
    fixture.replace_after = 0;
    assert!(restoration.restore_with(&fixture).is_err());
    assert!(restoration.is_pending());
    assert_eq!(fixture.mutations.borrow().len(), 1);
    assert!(fixture.suspended.load(Ordering::Acquire));
    fixture.replace_after = u32::MAX;
    restoration.restore_with(&fixture).unwrap();
    assert!(!fixture.suspended.load(Ordering::Acquire));
    assert!(!restoration.is_pending());
}

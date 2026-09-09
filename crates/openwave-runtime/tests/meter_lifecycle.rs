use openwave_core::model::NodeIdentity;
use openwave_runtime::meter::{CaptureReadiness, PcmPeak};
use std::time::{Duration, Instant};
fn identity(serial: &str) -> NodeIdentity {
    NodeIdentity {
        server_cookie: 10,
        object_serial: serial.into(),
    }
}

#[test]
fn partial_pcm_retains_byte_and_widens_negative_full_scale() {
    let mut pcm = PcmPeak::default();
    assert_eq!(pcm.push(&[0]), None);
    assert_eq!(pcm.push(&[]), None);
    assert_eq!(pcm.push(&[128, 0]), Some(1.0));
    assert_eq!(pcm.push(&[64, 0, 0]), Some(0.5));
    assert_eq!(pcm.push(&[0, 32]), Some(0.25));
    assert_eq!(pcm.push(&[0, 0]), Some(0.0));
}
#[test]
fn late_bytes_cannot_establish_replacement_generation_readiness() {
    let bank = CaptureReadiness::default();
    let now = Instant::now();
    let old = bank.register(identity("old"), now, Duration::ZERO).unwrap();
    old.started(now, None).unwrap();
    assert!(!bank.ready(&identity("old")));
    old.received(now).unwrap();
    assert!(bank.ready(&identity("old")));
    old.invalidate();
    let new = bank.register(identity("new"), now, Duration::ZERO).unwrap();
    new.started(now, None).unwrap();
    old.received(now + Duration::from_secs(1)).unwrap();
    assert!(!bank.ready(&identity("old")));
    assert!(!bank.ready(&identity("new")));
    new.received(now + Duration::from_secs(2)).unwrap();
    drop(old);
    assert!(bank.ready(&identity("new")));
}
#[test]
fn zero_flow_and_independent_taps_survive_other_tap_retirement() {
    let bank = CaptureReadiness::default();
    let now = Instant::now();
    let flowing = bank.register(identity("a"), now, Duration::ZERO).unwrap();
    let stalled = bank.register(identity("a"), now, Duration::ZERO).unwrap();
    let other = bank.register(identity("b"), now, Duration::ZERO).unwrap();
    for tap in [&flowing, &stalled, &other] {
        tap.started(now, None).unwrap();
    }
    // PCM value does not enter the byte-flow API: digital silence is data.
    flowing.received(now + Duration::from_secs(10)).unwrap();
    other.received(now + Duration::from_secs(8)).unwrap();
    assert_eq!(
        bank.gaps_at(now + Duration::from_secs(10))[&identity("a")],
        Duration::ZERO
    );
    drop(flowing);
    let gaps = bank.gaps_at(now + Duration::from_secs(10));
    assert_eq!(gaps[&identity("a")], Duration::from_secs(10));
    assert_eq!(gaps[&identity("b")], Duration::from_secs(2));
    assert!(bank.ready(&identity("b")));
    assert!(!bank.ready(&identity("a")));
}
#[test]
fn helper_restart_does_not_erase_stall_age_or_borrow_readiness() {
    let bank = CaptureReadiness::default();
    let now = Instant::now();
    let tap = bank
        .register(identity("a"), now, Duration::from_secs(1))
        .unwrap();
    tap.started(now, Some(now - Duration::from_secs(20)))
        .unwrap();
    assert!(bank.gaps_at(now + Duration::from_millis(999)).is_empty());
    assert_eq!(
        bank.gaps_at(now + Duration::from_secs(1))[&identity("a")],
        Duration::from_secs(21)
    );
    assert!(!bank.ready(&identity("a")));
    tap.received(now + Duration::from_secs(1)).unwrap();
    assert!(bank.ready(&identity("a")));
}

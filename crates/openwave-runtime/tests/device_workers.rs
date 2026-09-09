// Compile the real owner privately so fake transports never enter the public API.
mod process {
    pub use openwave_runtime::process::*;
}
mod device {
    include!("../src/device.rs");
    fn fixture_lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        match mutex.lock() {
            Ok(guard) => guard,
            Err(_) => panic!("test fixture mutex poisoned"),
        }
    }

    use std::sync::atomic::{AtomicBool, AtomicUsize};
    struct Fixture {
        found: Mutex<Vec<(ProfileId, u8, u8)>>,
        writes: mpsc::Sender<(UnitId, Vec<DeviceSetting>)>,
        closed: mpsc::Sender<UnitId>,
        scanned: mpsc::Sender<()>,
        gate: (Mutex<bool>, Condvar),
        block_address: u8,
        fail_polls: AtomicBool,
        failed_polls: AtomicUsize,
        opens: AtomicUsize,
    }
    struct ReleaseGate(Arc<Fixture>);
    impl Drop for ReleaseGate {
        fn drop(&mut self) {
            *fixture_lock(&self.0.gate.0) = true;
            self.0.gate.1.notify_all();
        }
    }
    struct Factory(Arc<Fixture>);
    struct Backend {
        fixture: Arc<Fixture>,
        unit: UnitId,
        state: DeviceState,
    }
    impl Drop for Backend {
        fn drop(&mut self) {
            let _ = self.fixture.closed.send(self.unit);
        }
    }
    impl DeviceFactory for Factory {
        fn scan(&self) -> Result<Vec<(ProfileId, u8, u8)>> {
            let found = fixture_lock(&self.0.found).clone();
            let _ = self.0.scanned.send(());
            Ok(found)
        }
        fn open(&self, unit: UnitId) -> Result<Box<dyn UnitBackend>> {
            self.0.opens.fetch_add(1, Ordering::SeqCst);
            let state =
                ConfigBuffer::decode(unit.profile, &vec![0; unit.profile.profile().config_len])?
                    .state();
            Ok(Box::new(Backend {
                fixture: self.0.clone(),
                unit,
                state,
            }))
        }
    }
    impl UnitBackend for Backend {
        fn info(&mut self) -> Result<DeviceInfo> {
            Ok(DeviceInfo {
                api: "1.0".into(),
                firmware: "1.2.3".into(),
                serial: format!("unit-{}", self.unit.address),
            })
        }
        fn poll(&mut self) -> Result<(DeviceState, Vec<OperationIssue>)> {
            if self.fixture.fail_polls.load(Ordering::SeqCst) {
                self.fixture.failed_polls.fetch_add(1, Ordering::SeqCst);
                Err(OperationError::unavailable("fixture transfer timeout"))
            } else {
                Ok((self.state.clone(), vec![]))
            }
        }
        fn apply(&mut self, settings: &[DeviceSetting]) -> Result<DeviceState> {
            self.fixture
                .writes
                .send((self.unit, settings.to_vec()))
                .unwrap();
            if self.unit.address == self.fixture.block_address {
                let mut released = fixture_lock(&self.fixture.gate.0);
                while !*released {
                    released = match self.fixture.gate.1.wait(released) {
                        Ok(guard) => guard,
                        Err(_) => panic!("test fixture wait poisoned"),
                    };
                }
            }
            for setting in settings {
                if let DeviceSetting::Mute(value) = setting {
                    self.state.muted = *value;
                }
            }
            Ok(self.state.clone())
        }
        fn unresponsive(&self) -> bool {
            self.fixture.fail_polls.load(Ordering::SeqCst)
        }
    }
    fn fixture(
        addresses: &[u8],
        block_address: u8,
    ) -> (
        Arc<Fixture>,
        mpsc::Receiver<(UnitId, Vec<DeviceSetting>)>,
        mpsc::Receiver<UnitId>,
        mpsc::Receiver<()>,
    ) {
        let (writes, written) = mpsc::channel();
        let (closed, dropped) = mpsc::channel();
        let (scanned, scans) = mpsc::channel();
        (
            Arc::new(Fixture {
                found: Mutex::new(
                    addresses
                        .iter()
                        .map(|a| (ProfileId::Wave3, 1, *a))
                        .collect(),
                ),
                writes,
                closed,
                scanned,
                gate: (Mutex::new(false), Condvar::new()),
                block_address,
                fail_polls: AtomicBool::new(false),
                failed_polls: AtomicUsize::new(0),
                opens: AtomicUsize::new(0),
            }),
            written,
            dropped,
            scans,
        )
    }
    fn event(receiver: &mpsc::Receiver<DeviceEvent>) -> DeviceEvent {
        receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("worker event deadline")
    }
    fn connected(receiver: &mpsc::Receiver<DeviceEvent>) -> UnitId {
        loop {
            match event(receiver) {
                DeviceEvent::Connected(s) => return s.id,
                DeviceEvent::Error { error, .. } => panic!("{error}"),
                _ => {}
            }
        }
    }

    #[test]
    fn captured_queues_are_independent_and_retirement_cancels_only_not_started_jobs() {
        let (fixture, written, dropped, _) = fixture(&[1, 2], 1);
        let (mut manager, events) =
            DeviceManager::start_with(Arc::new(Factory(fixture.clone()))).unwrap();
        let _release = ReleaseGate(fixture.clone());
        let first = connected(&events);
        let second = connected(&events);
        let (a, b) = if first.address == 1 {
            (first, second)
        } else {
            (second, first)
        };
        manager
            .submit(a, 10, vec![DeviceSetting::Mute(true)])
            .unwrap();
        assert_eq!(written.recv_timeout(Duration::from_secs(5)).unwrap().0, a);
        manager
            .submit(a, 11, vec![DeviceSetting::Mute(false)])
            .unwrap();
        manager
            .submit(b, 12, vec![DeviceSetting::Mute(true)])
            .unwrap();
        assert_eq!(written.recv_timeout(Duration::from_secs(5)).unwrap().0, b);
        let queue = fixture_lock(&manager.registry).units[&a].clone();
        queue.retire();
        assert!(
            manager
                .submit(a, 13, vec![DeviceSetting::Mute(false)])
                .is_err()
        );
        assert!(
            dropped.try_recv().is_err(),
            "executing handle must not close"
        );
        *fixture_lock(&fixture.gate.0) = true;
        fixture.gate.1.notify_all();
        let mut completions = HashMap::new();
        while completions.len() < 3 {
            if let DeviceEvent::Completed { job, unit, result } = event(&events) {
                assert!(
                    completions.insert(job, (unit, result)).is_none(),
                    "duplicate completion"
                );
            }
        }
        assert_eq!(completions[&10].0, a);
        assert!(completions[&10].1.as_ref().unwrap().muted);
        assert_eq!(
            completions[&11].1.as_ref().unwrap_err().code,
            ErrorCode::Cancelled
        );
        assert_eq!(completions[&12].0, b);
        assert!(completions[&12].1.as_ref().unwrap().muted);
        assert_eq!(dropped.recv_timeout(Duration::from_secs(5)).unwrap(), a);
        assert!(
            written.try_recv().is_err(),
            "cancelled write reached transport"
        );
        manager.stop().unwrap();
        for event in events.try_iter() {
            if let DeviceEvent::Completed { job, .. } = event {
                panic!("duplicate completion {job}");
            }
        }
    }

    #[test]
    fn three_poll_failures_retire_and_suppression_requires_observed_unplug() {
        let (fixture, _written, _, scans) = fixture(&[1], 0);
        let (mut manager, events) =
            DeviceManager::start_with(Arc::new(Factory(fixture.clone()))).unwrap();
        let old = connected(&events);
        // A successful same-valued observation is still delivered to the controller.
        loop {
            if let DeviceEvent::Observed(s) = event(&events) {
                assert_eq!(s.id, old);
                assert!(!s.state.known().unwrap().muted);
                break;
            }
        }
        fixture.fail_polls.store(true, Ordering::SeqCst);
        let mut failures_seen = 0;
        loop {
            match event(&events) {
                DeviceEvent::Error {
                    unit: Some(unit), ..
                } if unit == old => {
                    failures_seen += 1;
                }
                DeviceEvent::Retired(unit) if unit == old => {
                    assert!(failures_seen >= 3);
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(fixture.failed_polls.load(Ordering::SeqCst), 3);
        assert!(
            manager
                .submit(old, 1, vec![DeviceSetting::Mute(true)])
                .is_err()
        );
        while scans.try_recv().is_ok() {}
        manager.rescan().unwrap();
        scans.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(fixture.opens.load(Ordering::SeqCst), 1);
        fixture_lock(&fixture.found).clear();
        manager.rescan().unwrap();
        scans.recv_timeout(Duration::from_secs(5)).unwrap();
        fixture.fail_polls.store(false, Ordering::SeqCst);
        fixture_lock(&fixture.found).push((ProfileId::Wave3, 1, 1));
        manager.rescan().unwrap();
        let new = connected(&events);
        assert_ne!(new.incarnation, old.incarnation);
        assert!(
            manager
                .submit(old, 2, vec![DeviceSetting::Mute(true)])
                .is_err()
        );
        manager
            .submit(new, 3, vec![DeviceSetting::Mute(true)])
            .unwrap();
        loop {
            if let DeviceEvent::Completed {
                job: 3,
                unit,
                result,
            } = event(&events)
            {
                assert_eq!(unit, new);
                assert!(result.unwrap().muted);
                break;
            }
        }
        manager.stop().unwrap();
    }

    #[test]
    fn unsupported_settings_are_rejected_before_queue_admission() {
        let (fixture, written, _, _) = fixture(&[1], 0);
        let (mut manager, events) = DeviceManager::start_with(Arc::new(Factory(fixture))).unwrap();
        let unit = connected(&events);
        assert_eq!(
            manager
                .submit(
                    unit,
                    1,
                    vec![DeviceSetting::Mute(true), DeviceSetting::Phantom(true)]
                )
                .unwrap_err()
                .code,
            ErrorCode::Unsupported
        );
        assert!(written.try_recv().is_err());
        manager.stop().unwrap();
        assert!(
            !events
                .try_iter()
                .any(|event| matches!(event, DeviceEvent::Completed { job: 1, .. }))
        );
    }

    #[test]
    fn shutdown_drains_running_transaction_before_closing_and_cancels_queued_work() {
        let (fixture, written, dropped, _) = fixture(&[1], 1);
        let (mut manager, events) =
            DeviceManager::start_with(Arc::new(Factory(fixture.clone()))).unwrap();
        let _release = ReleaseGate(fixture.clone());
        let unit = connected(&events);
        manager
            .submit(unit, 20, vec![DeviceSetting::Mute(true)])
            .unwrap();
        written.recv_timeout(Duration::from_secs(5)).unwrap();
        manager
            .submit(unit, 21, vec![DeviceSetting::Mute(false)])
            .unwrap();
        let queue = fixture_lock(&manager.registry).units[&unit].clone();
        let stopping = thread::spawn(move || manager.stop());
        let state = queue.lock_state();
        let (state, _) =
            match queue
                .wake
                .wait_timeout_while(state, Duration::from_secs(5), |state| !state.retiring)
            {
                Ok(value) => value,
                Err(_) => panic!("shutdown fixture mutex poisoned"),
            };
        assert!(state.retiring);
        drop(state);
        assert!(!stopping.is_finished());
        assert!(dropped.try_recv().is_err());
        *fixture_lock(&fixture.gate.0) = true;
        fixture.gate.1.notify_all();
        stopping.join().unwrap().unwrap();
        let completed: Vec<_> = events
            .try_iter()
            .filter_map(|e| {
                if let DeviceEvent::Completed { job, result, .. } = e {
                    Some((job, result))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(completed.len(), 2);
        assert_eq!(completed[0].0, 20);
        assert!(completed[0].1.as_ref().unwrap().muted);
        assert_eq!(completed[1].0, 21);
        assert_eq!(
            completed[1].1.as_ref().unwrap_err().code,
            ErrorCode::Cancelled
        );
        assert_eq!(dropped.recv_timeout(Duration::from_secs(5)).unwrap(), unit);
        assert!(written.try_recv().is_err());
    }

    #[test]
    fn poisoned_queue_retires_instead_of_executing_accepted_pending_job() {
        let (fixture, written, _, _) = fixture(&[1], 1);
        let (mut manager, events) =
            DeviceManager::start_with(Arc::new(Factory(fixture.clone()))).unwrap();
        let _release = ReleaseGate(fixture.clone());
        let unit = connected(&events);
        manager
            .submit(unit, 30, vec![DeviceSetting::Mute(true)])
            .unwrap();
        written.recv_timeout(Duration::from_secs(5)).unwrap();
        manager
            .submit(unit, 31, vec![DeviceSetting::Mute(false)])
            .unwrap();
        let queue = fixture_lock(&manager.registry).units[&unit].clone();
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = fixture_lock(&queue.state);
            panic!("injected queue owner failure");
        }));
        assert!(poisoned.is_err());
        assert!(
            manager
                .submit(unit, 32, vec![DeviceSetting::Mute(false)])
                .is_err()
        );
        *fixture_lock(&fixture.gate.0) = true;
        fixture.gate.1.notify_all();
        let mut results = HashMap::new();
        let mut unavailable = false;
        loop {
            match event(&events) {
                DeviceEvent::Completed { job, result, .. } => {
                    assert!(results.insert(job, result).is_none());
                }
                DeviceEvent::Error { error, .. } => {
                    unavailable |= error.code == ErrorCode::Unavailable
                }
                DeviceEvent::Retired(id) if id == unit => break,
                _ => {}
            }
        }
        assert!(results[&30].as_ref().unwrap().muted);
        assert_eq!(
            results[&31].as_ref().unwrap_err().code,
            ErrorCode::Cancelled
        );
        assert!(unavailable);
        assert!(written.try_recv().is_err());
        manager.stop().unwrap();
    }
}

mod process {
    use openwave_core::model::Result;
    use std::{
        collections::HashMap,
        path::PathBuf,
        sync::{Arc, Mutex},
        time::Duration,
    };

    #[derive(Default)]
    pub struct Amixer {
        pub contents: String,
        pub calls: Vec<Vec<String>>,
        pub values: HashMap<u32, String>,
        pub replace_after_contents: Option<PathBuf>,
    }
    #[derive(Clone, Default)]
    pub struct CommandRunner(pub Arc<Mutex<Amixer>>);
    pub struct Output {
        pub stdout: Vec<u8>,
    }
    impl CommandRunner {
        pub fn run(&self, program: &str, args: &[String], timeout: Duration) -> Result<Output> {
            assert_eq!(program, "amixer", "no native commands in ALSA fixtures");
            assert_eq!(&args[..2], &["-c", "3"]);
            assert!(timeout <= Duration::from_secs(3));
            let mut state = match self.0.lock() {
                Ok(state) => state,
                Err(_) => panic!("amixer fixture mutex poisoned"),
            };
            state.calls.push(args.to_vec());
            let stdout = if args[2] == "contents" {
                assert_eq!(args.len(), 3);
                if let Some(bus) = state.replace_after_contents.take() {
                    std::fs::write(bus, "009/009").unwrap();
                }
                state.contents.clone()
            } else {
                let id: u32 = args[3].strip_prefix("numid=").unwrap().parse().unwrap();
                assert!([71, 72, 73].contains(&id), "guessed ALSA control {id}");
                match args[2].as_str() {
                    "cset" => {
                        assert_eq!(args.len(), 5);
                        state.values.insert(id, args[4].clone());
                    }
                    "cget" => assert_eq!(args.len(), 4),
                    _ => panic!("unexpected fixture command"),
                }
                format!(
                    " : values={}\n",
                    state.values.get(&id).expect("uninitialized control read")
                )
            };
            Ok(Output {
                stdout: stdout.into_bytes(),
            })
        }
    }
}
mod device {
    include!("../src/device.rs");
    fn fixture_lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        match mutex.lock() {
            Ok(guard) => guard,
            Err(_) => panic!("test fixture mutex poisoned"),
        }
    }

    struct MemoryUsb {
        config: Vec<u8>,
        short_read: bool,
        short_write: bool,
        writes: Vec<Vec<u8>>,
    }
    struct MemoryTransport(Arc<Mutex<MemoryUsb>>);
    impl Transport for MemoryTransport {
        fn read(&mut self, _selector: u16, bytes: &mut [u8]) -> Result<usize> {
            let state = fixture_lock(&self.0);
            let length = if state.short_read {
                2
            } else {
                state.config.len().min(bytes.len())
            };
            bytes[..length].copy_from_slice(&state.config[..length]);
            Ok(length)
        }
        fn write(&mut self, _selector: u16, bytes: &[u8]) -> Result<usize> {
            let mut state = fixture_lock(&self.0);
            state.writes.push(bytes.to_vec());
            if state.short_write {
                return Ok(bytes.len() - 1);
            }
            state.config.copy_from_slice(bytes);
            Ok(bytes.len())
        }
        fn unresponsive(&self) -> bool {
            false
        }
    }
    struct AlsaState {
        values: [i32; 3],
        fail: [bool; 3],
        reads: [usize; 3],
        writes: Vec<(Field, i32)>,
    }
    struct FakeAlsa(Arc<Mutex<AlsaState>>);
    impl Alsa for FakeAlsa {
        fn expected(&self, _field: Field, value: i32) -> Result<i32> {
            Ok(value)
        }
        fn read(&mut self, field: Field) -> Result<i32> {
            let mut state = fixture_lock(&self.0);
            state.reads[field.index()] += 1;
            Ok(state.values[field.index()])
        }
        fn write(&mut self, field: Field, value: i32) -> Result<()> {
            let mut state = fixture_lock(&self.0);
            state.writes.push((field, value));
            if state.fail[field.index()] {
                return Err(OperationError::unavailable("fixture mirror failure"));
            }
            state.values[field.index()] = value;
            Ok(())
        }
    }
    fn unit() -> UnitId {
        UnitId {
            profile: ProfileId::Wave3,
            bus: 1,
            address: 2,
            incarnation: 44,
        }
    }
    fn memory() -> (VendorDevice, Arc<Mutex<MemoryUsb>>) {
        let memory = Arc::new(Mutex::new(MemoryUsb {
            config: vec![0; 16],
            short_read: false,
            short_write: false,
            writes: vec![],
        }));
        (
            VendorDevice {
                unit: unit(),
                transport: Box::new(MemoryTransport(memory.clone())),
            },
            memory,
        )
    }
    fn alsa() -> (FakeAlsa, Arc<Mutex<AlsaState>>) {
        let state = Arc::new(Mutex::new(AlsaState {
            values: [0, 0, 0],
            fail: [false; 3],
            reads: [0; 3],
            writes: vec![],
        }));
        (FakeAlsa(state.clone()), state)
    }
    fn config(muted: bool, hp: f64, gain: u16) -> ConfigBuffer {
        let mut config = ConfigBuffer::decode(ProfileId::Wave3, &[0; 16]).unwrap();
        config.apply(DeviceSetting::Mute(muted)).unwrap();
        config.apply(DeviceSetting::HeadphoneDb(hp)).unwrap();
        config.apply(DeviceSetting::GainRaw(gain)).unwrap();
        config
    }
    fn poll(
        mirror: &mut Mirror,
        config: &mut ConfigBuffer,
        alsa: &mut dyn Alsa,
        now: Instant,
    ) -> Vec<OperationIssue> {
        let (_, errors) = mirror.observe(config, alsa, now);
        mirror.committed(config.state());
        errors
    }

    #[test]
    fn raw_short_dump_is_valid_but_production_short_blocks_and_writes_fail() {
        let (mut vendor, memory) = memory();
        fixture_lock(&memory).short_read = true;
        assert_eq!(vendor.read_raw(0, &mut [0; 512]).unwrap(), 2);
        assert!(vendor.read_config().is_err());
        assert!(vendor.read_info().is_err());
        assert!(vendor.read_meters().is_err());
        assert!(vendor.write_config_raw(&[0; 15]).is_err());
        assert!(fixture_lock(&memory).writes.is_empty());
        fixture_lock(&memory).short_write = true;
        assert!(vendor.write_config_raw(&[0; 16]).is_err());
    }

    #[test]
    fn backend_batches_preserve_reserved_bytes_and_validate_before_read_write() {
        let (vendor, memory) = memory();
        let (alsa, _) = alsa();
        fixture_lock(&memory).config[15] = 0xa7;
        let mut backend = SyncedDevice {
            vendor,
            alsa: Box::new(alsa),
            mirror: Mirror::default(),
        };
        let state = backend
            .apply(&[
                DeviceSetting::GainRaw(u16::MAX),
                DeviceSetting::HeadphoneDb(-999.0),
                DeviceSetting::Mute(true),
            ])
            .unwrap();
        assert_eq!(state.gain_raw, 0x2800);
        assert_eq!(state.hp_volume_db, -128.0);
        assert!(state.muted);
        assert_eq!(fixture_lock(&memory).config[15], 0xa7);
        assert_eq!(fixture_lock(&memory).writes.len(), 1);
        fixture_lock(&memory).short_read = true;
        assert_eq!(
            backend
                .apply(&[DeviceSetting::Mute(false), DeviceSetting::Phantom(true)])
                .unwrap_err()
                .code,
            ErrorCode::Unsupported
        );
        assert_eq!(fixture_lock(&memory).writes.len(), 1);
    }

    #[test]
    fn each_failed_mirror_retains_firmware_and_retries_without_stale_feedback() {
        for field in Field::ALL {
            let (mut alsa, shared) = alsa();
            let mut mirror = Mirror::default();
            let now = Instant::now();
            let mut config = config(false, -20.0, 256);
            assert!(poll(&mut mirror, &mut config, &mut alsa, now).is_empty());
            // A physical firmware change and stale ALSA arrive together.
            fixture_lock(&shared).fail[field.index()] = true;
            let setting = match field {
                Field::Mute => DeviceSetting::Mute(true),
                Field::Headphone => DeviceSetting::HeadphoneDb(-8.0),
                Field::Gain => DeviceSetting::GainRaw(10 * 256),
            };
            config.apply(setting).unwrap();
            let wanted = config.state();
            assert_eq!(
                poll(&mut mirror, &mut config, &mut alsa, now + ALSA_POLL).len(),
                1
            );
            assert_eq!(config.state(), wanted);
            let stale = fixture_lock(&shared).values[field.index()];
            assert_eq!(
                poll(&mut mirror, &mut config, &mut alsa, now + ALSA_POLL * 2).len(),
                1
            );
            assert_eq!(config.state(), wanted);
            assert_eq!(fixture_lock(&shared).values[field.index()], stale);
            fixture_lock(&shared).fail[field.index()] = false;
            assert!(poll(&mut mirror, &mut config, &mut alsa, now + ALSA_POLL * 3).is_empty());
            assert_eq!(config.state(), wanted);
            assert_eq!(
                fixture_lock(&shared).values[field.index()],
                field.alsa(field.firmware(&wanted, ProfileId::Wave3), ProfileId::Wave3)
            );
            if field != Field::Gain {
                fixture_lock(&shared).values[field.index()] = stale;
                poll(&mut mirror, &mut config, &mut alsa, now + ALSA_POLL * 4);
                assert_ne!(
                    field.firmware(&config.state(), ProfileId::Wave3),
                    field.firmware(&wanted, ProfileId::Wave3)
                );
            }
        }
    }

    #[test]
    fn physical_change_supersedes_failed_requested_mirror_even_back_to_old_value() {
        let (vendor, memory) = memory();
        let (alsa, shared) = alsa();
        let mut backend = SyncedDevice {
            vendor,
            alsa: Box::new(alsa),
            mirror: Mirror::default(),
        };
        backend.poll().unwrap();
        fixture_lock(&shared).fail[Field::Mute.index()] = true;
        backend.apply(&[DeviceSetting::Mute(true)]).unwrap();
        assert!(backend.poll().unwrap().0.muted);
        fixture_lock(&memory).config[4] = 0; // Physical control returns to pre-command value.
        fixture_lock(&shared).fail[Field::Mute.index()] = false;
        assert!(!backend.poll().unwrap().0.muted);
        assert_eq!(fixture_lock(&shared).values[Field::Mute.index()], 0);
    }

    #[test]
    fn observations_are_throttled_but_failed_mirror_retries_every_poll() {
        let (mut alsa, shared) = alsa();
        let mut mirror = Mirror::default();
        let now = Instant::now();
        let mut config = config(false, -10.0, 256);
        poll(&mut mirror, &mut config, &mut alsa, now);
        fixture_lock(&shared).values[0] = 1;
        poll(
            &mut mirror,
            &mut config,
            &mut alsa,
            now + Duration::from_millis(100),
        );
        assert!(!config.state().muted);
        poll(&mut mirror, &mut config, &mut alsa, now + ALSA_POLL);
        assert!(config.state().muted);
        assert_eq!(fixture_lock(&shared).reads, [1, 1, 0]);
        fixture_lock(&shared).fail[0] = true;
        config.apply(DeviceSetting::Mute(false)).unwrap();
        let writes = fixture_lock(&shared).writes.len();
        poll(
            &mut mirror,
            &mut config,
            &mut alsa,
            now + Duration::from_millis(600),
        );
        poll(
            &mut mirror,
            &mut config,
            &mut alsa,
            now + Duration::from_millis(700),
        );
        assert_eq!(fixture_lock(&shared).writes.len(), writes + 2);
        assert_eq!(fixture_lock(&shared).reads, [1, 1, 0]);
    }

    #[test]
    fn unknown_roles_and_ranges_never_guess_controls_or_values() {
        let mut controls = CardControls {
            unit: unit(),
            card: 3,
            controls: AlsaControls::default(),
            runner: CommandRunner::default(),
            root: PathBuf::from("/no-alsa-fixture"),
            last_discovery: Instant::now(),
        };
        // All fail before the native command or identity lookup can run.
        for field in Field::ALL {
            assert!(controls.read(field).is_err());
            assert!(controls.write(field, 1).is_err());
        }
        assert!(CardControls::value(Field::Mute, "unavailable").is_err());
        assert!(CardControls::value(Field::Headphone, ": values=bad").is_err());
        controls.controls.hp_volume = Some(AlsaControl {
            numid: 82,
            range: None,
        });
        assert!(controls.write(Field::Headphone, 100).is_err());
    }

    #[test]
    fn runtime_card_discovery_requires_exact_complete_unique_identity() {
        let root = tempfile::tempdir().unwrap();
        for (card, id, bus) in [
            (3, "0fd9:0070", "001/002"),
            (4, "0fd9:0070", "003/004"),
            (5, "ffff:0070", "001/002"),
        ] {
            let path = root.path().join(format!("card{card}"));
            fs::create_dir(&path).unwrap();
            fs::write(path.join("usbid"), id).unwrap();
            fs::write(path.join("usbbus"), bus).unwrap();
        }
        assert_eq!(
            protocol::card_for_unit(unit(), &card_identities(root.path(), unit()).unwrap()),
            Some(3)
        );
        fs::write(root.path().join("card4/usbbus"), "001/002").unwrap();
        assert_eq!(
            protocol::card_for_unit(unit(), &card_identities(root.path(), unit()).unwrap()),
            None
        );
        fs::remove_file(root.path().join("card3/usbbus")).unwrap();
        assert!(card_identities(root.path(), unit()).is_err());
    }

    const MUTE_CONTROL: &str =
        "numid=71,iface=MIXER,name='Wave Capture Switch'\n ; type=BOOLEAN,values=1\n";
    const COMPLETE_CONTROLS: &str = concat!(
        "numid=71,iface=MIXER,name='Wave Capture Switch'\n ; type=BOOLEAN,values=1\n",
        "numid=72,iface=MIXER,name='Wave Capture Volume'\n ; type=INTEGER,values=1,min=0,max=80\n",
        "numid=73,iface=MIXER,name='Wave Playback Volume'\n ; type=INTEGER,values=1,min=0,max=120\n",
    );

    fn card_fixture(root: &Path, card: u32, id: &str, bus: Option<&str>) {
        let path = root.join(format!("card{card}"));
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("usbid"), id).unwrap();
        if let Some(bus) = bus {
            fs::write(path.join("usbbus"), bus).unwrap();
        }
    }

    fn controls_fixture(contents: &str) -> (tempfile::TempDir, CardControls, CommandRunner) {
        let root = tempfile::tempdir().unwrap();
        card_fixture(root.path(), 3, "0fd9:0070", Some("001/002"));
        let runner = CommandRunner::default();
        fixture_lock(&runner.0).contents = contents.into();
        let controls = CardControls::open_at(unit(), root.path(), runner.clone()).unwrap();
        (root, controls, runner)
    }

    #[test]
    fn unrelated_incomplete_card_does_not_block_exact_target_commands() {
        let (root, _, runner) = controls_fixture(COMPLETE_CONTROLS);
        card_fixture(root.path(), 8, "046d:0001", None);
        let mut controls = CardControls::open_at(unit(), root.path(), runner.clone()).unwrap();
        controls.write(Field::Mute, 1).unwrap();
        fs::write(root.path().join("card8/usbbus"), "malformed").unwrap();
        controls.write(Field::Mute, 0).unwrap();
        assert_eq!(fixture_lock(&runner.0).values[&71], "on");

        // Once the incomplete record could be this product it is not ignorable.
        fs::write(root.path().join("card8/usbid"), "0fd9:0070").unwrap();
        assert!(controls.write(Field::Mute, 1).is_err());
        fs::write(root.path().join("card8/usbbus"), "001/002").unwrap();
        assert!(controls.write(Field::Mute, 1).is_err());
        assert_eq!(fixture_lock(&runner.0).values[&71], "on");
    }

    fn recover_initial_controls(contents: &str) {
        let (_root, mut controls, runner) = controls_fixture(contents);
        let now = controls.last_discovery;
        let mut config = config(true, -10.0, 256);
        let expected = config.state();
        let mut mirror = Mirror::default();
        assert!(!poll(&mut mirror, &mut config, &mut controls, now).is_empty());
        assert_eq!(config.state(), expected);
        assert!(!fixture_lock(&runner.0).values.contains_key(&73));
        fixture_lock(&runner.0).contents = COMPLETE_CONTROLS.into();

        // Role discovery is bounded independently of per-poll mirror retries.
        assert!(!poll(&mut mirror, &mut config, &mut controls, now + ALSA_POLL / 2).is_empty());
        assert!(!fixture_lock(&runner.0).values.contains_key(&73));
        // The uninterrupted owner performs recovery via its ordinary vendor poll.
        // Advance only its discovery deadline, not a host clock or worker thread.
        controls.last_discovery = Instant::now() - ALSA_POLL;
        let (vendor, usb) = memory();
        fixture_lock(&usb).config = config.as_bytes().to_vec();
        let mut device = SyncedDevice {
            vendor,
            alsa: Box::new(controls),
            mirror,
        };
        let (observed, errors) = device.poll().unwrap();
        assert!(errors.is_empty());
        assert_eq!(observed, expected);
        assert_eq!(fixture_lock(&runner.0).values[&71], "off");
        assert_eq!(fixture_lock(&runner.0).values[&72], "2");
        assert_eq!(fixture_lock(&runner.0).values[&73], "100");
        assert_eq!(device.poll().unwrap().0, expected);
        assert!(fixture_lock(&usb).writes.is_empty());
        assert_eq!(
            fixture_lock(&runner.0)
                .calls
                .iter()
                .filter(|args| args[2] == "contents")
                .count(),
            2,
        );
    }

    #[test]
    fn empty_initial_controls_recover_without_reopening() {
        recover_initial_controls("");
    }

    #[test]
    fn partial_initial_controls_and_ranges_recover_without_reopening() {
        recover_initial_controls(&format!(
            "{MUTE_CONTROL}numid=73,iface=MIXER,name='Wave Playback Volume'\n ; type=INTEGER,values=1\n"
        ));
    }

    #[test]
    fn rediscovery_refuses_replacement_and_ambiguous_cards_then_retries_original() {
        let (root, mut controls, runner) = controls_fixture("");
        let now = controls.last_discovery;
        let mut config = config(true, -10.0, 256);
        let mut mirror = Mirror::default();
        assert!(!poll(&mut mirror, &mut config, &mut controls, now).is_empty());
        fixture_lock(&runner.0).contents = COMPLETE_CONTROLS.into();
        fs::write(root.path().join("card3/usbbus"), "001/003").unwrap();
        assert!(!poll(&mut mirror, &mut config, &mut controls, now + ALSA_POLL).is_empty());
        fs::write(root.path().join("card3/usbbus"), "001/002").unwrap();
        card_fixture(root.path(), 8, "0fd9:0070", Some("001/002"));
        assert!(!poll(&mut mirror, &mut config, &mut controls, now + ALSA_POLL * 2).is_empty());
        assert!(fixture_lock(&runner.0).values.is_empty());
        assert_eq!(fixture_lock(&runner.0).calls.len(), 1);

        // Retain the latest firmware obligation throughout unavailable identity.
        config.apply(DeviceSetting::HeadphoneDb(-20.0)).unwrap();
        fs::remove_dir_all(root.path().join("card8")).unwrap();
        assert!(poll(&mut mirror, &mut config, &mut controls, now + ALSA_POLL * 3).is_empty());
        assert_eq!(fixture_lock(&runner.0).values[&71], "off");
        assert_eq!(fixture_lock(&runner.0).values[&73], "80");
        fs::write(root.path().join("card3/usbbus"), "001/003").unwrap();
        card_fixture(root.path(), 9, "0fd9:0070", Some("001/002"));
        assert!(controls.write(Field::Mute, 0).is_err());
        assert_eq!(fixture_lock(&runner.0).values[&71], "off");
    }

    #[test]
    fn rediscovery_does_not_admit_contents_across_card_replacement() {
        let (root, mut controls, runner) = controls_fixture("");
        let now = controls.last_discovery;
        let mut config = config(true, -10.0, 256);
        let mut mirror = Mirror::default();
        {
            let mut amixer = fixture_lock(&runner.0);
            amixer.contents = COMPLETE_CONTROLS.into();
            amixer.replace_after_contents = Some(root.path().join("card3/usbbus"));
        }
        assert!(!poll(&mut mirror, &mut config, &mut controls, now + ALSA_POLL).is_empty());
        assert!(fixture_lock(&runner.0).values.is_empty());
        fs::write(root.path().join("card3/usbbus"), "001/002").unwrap();
        assert!(poll(&mut mirror, &mut config, &mut controls, now + ALSA_POLL * 2).is_empty());
        assert_eq!(fixture_lock(&runner.0).values[&71], "off");
        assert_eq!(fixture_lock(&runner.0).values[&73], "100");
        assert!(config.state().muted);
    }

    #[test]
    fn driver_clamp_confirmation_does_not_feed_saturation_back_into_firmware() {
        struct LimitedAlsa {
            controls: CardControls,
            values: [i32; 3],
        }
        impl Alsa for LimitedAlsa {
            fn expected(&self, field: Field, value: i32) -> Result<i32> {
                self.controls.expected(field, value)
            }
            fn read(&mut self, field: Field) -> Result<i32> {
                Ok(self.values[field.index()])
            }
            fn write(&mut self, field: Field, value: i32) -> Result<()> {
                self.values[field.index()] = self.expected(field, value)?;
                Ok(())
            }
        }
        let range = |numid, max| {
            Some(AlsaControl {
                numid,
                range: Some(protocol::AlsaRange { min: 0, max }),
            })
        };
        let controls = CardControls {
            unit: unit(),
            card: 7,
            runner: CommandRunner::default(),
            root: PathBuf::from("/no-alsa-fixture"),
            last_discovery: Instant::now(),
            controls: AlsaControls {
                mute: range(91, 1),
                hp_volume: range(93, 99),
                gain: range(92, 150),
            },
        };
        let mut alsa = LimitedAlsa {
            controls,
            values: [0; 3],
        };
        let mut config = config(true, 0.0, 30 * 256);
        let mut mirror = Mirror::default();
        let now = Instant::now();
        assert!(poll(&mut mirror, &mut config, &mut alsa, now).is_empty());
        assert_eq!(alsa.values, [1, 99, 60]);
        assert!(poll(&mut mirror, &mut config, &mut alsa, now + ALSA_POLL).is_empty());
        assert_eq!(config.state().hp_volume_db, 0.0);
        assert!(config.state().muted);
    }
}

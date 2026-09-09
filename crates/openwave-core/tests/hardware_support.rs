use openwave_core::{
    profiles::{ProfileId, profile_for_usb},
    protocol::{UsbIdentity, supported_devices},
};

#[test]
fn only_exact_enabled_vid_pid_pairs_are_admitted() {
    for (vid, pid, expected) in [
        (0x0fd9, 0x007d, Some(ProfileId::WaveXlr)),
        (0x0fd9, 0x00a6, Some(ProfileId::WaveXlrMk2)),
        (0x0fd9, 0x0070, Some(ProfileId::Wave3)),
        (0x0fd9, 0x00c7, None),
        (0x0fd9, 0x00b6, None),
        (0x0fd9, 0x9999, None),
        (0x046d, 0x007d, None),
    ] {
        assert_eq!(profile_for_usb(vid, pid).map(|p| p.id), expected);
    }
}

#[test]
fn scan_preserves_duplicate_models_and_sorts_physical_locations() {
    let scan = supported_devices([
        UsbIdentity {
            vid: 0x0fd9,
            pid: 0x00a6,
            bus: 3,
            address: 2,
        },
        UsbIdentity {
            vid: 0x0fd9,
            pid: 0x00c7,
            bus: 1,
            address: 1,
        },
        UsbIdentity {
            vid: 0x0fd9,
            pid: 0x0070,
            bus: 1,
            address: 9,
        },
        UsbIdentity {
            vid: 0x0fd9,
            pid: 0x00a6,
            bus: 1,
            address: 5,
        },
        UsbIdentity {
            vid: 0x046d,
            pid: 0x0070,
            bus: 1,
            address: 3,
        },
    ]);
    assert_eq!(
        scan,
        vec![
            (ProfileId::WaveXlrMk2, 1, 5),
            (ProfileId::Wave3, 1, 9),
            (ProfileId::WaveXlrMk2, 3, 2),
        ]
    );
}

#[test]
fn persisted_profiles_keep_legacy_identifiers() {
    let profiles: Vec<ProfileId> =
        serde_json::from_str(r#"["wave_xlr","wave_xlr_mk2","wave3"]"#).unwrap();
    assert_eq!(
        profiles,
        [ProfileId::WaveXlr, ProfileId::WaveXlrMk2, ProfileId::Wave3]
    );
    assert_eq!(
        serde_json::to_string(&profiles).unwrap(),
        r#"["wave_xlr","wave_xlr_mk2","wave3"]"#
    );
    assert!(serde_json::from_str::<ProfileId>(r#""wave_xlr_mk3""#).is_err());
}

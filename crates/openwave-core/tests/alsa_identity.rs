use openwave_core::{
    model::UnitId,
    profiles::ProfileId,
    protocol::{
        AlsaCardIdentity, ConnectedIdentity, alsa_hp_to_fw, card_for_unit, device_for_capture,
        fw_gain_to_alsa, fw_hp_to_alsa, parse_alsa_controls, parse_alsa_mute, parse_alsa_volume,
    },
};

fn unit(profile: ProfileId, address: u8) -> UnitId {
    UnitId {
        profile,
        bus: 2,
        address,
        incarnation: 7,
    }
}

#[test]
fn card_pairing_requires_exact_vendor_product_bus_and_address() {
    let target = unit(ProfileId::WaveXlrMk2, 4);
    let cards = [
        AlsaCardIdentity::parse(3, "0fd9:00a6", "011/007").unwrap(),
        AlsaCardIdentity::parse(4, "0fd9:007d", "002/004").unwrap(),
        AlsaCardIdentity::parse(5, "0FD9:00A6\n", "002/004\n").unwrap(),
        AlsaCardIdentity::parse(6, "046d:00a6", "002/004").unwrap(),
    ];
    assert_eq!(card_for_unit(target, &cards), Some(5));
    assert_eq!(card_for_unit(unit(ProfileId::Wave3, 4), &cards), None);
    assert_eq!(card_for_unit(target, &cards[..2]), None);
    assert_eq!(
        card_for_unit(
            target,
            &[
                cards[2],
                AlsaCardIdentity {
                    card: 7,
                    ..cards[2]
                }
            ]
        ),
        None
    );
    assert!(AlsaCardIdentity::parse(3, "0fd9:00a6", "").is_err());
    assert!(AlsaCardIdentity::parse(3, "0fd9:00a6", "002/999").is_err());
    assert!(AlsaCardIdentity::parse(3, "Elgato", "002/004").is_err());
    assert!(AlsaCardIdentity::parse(3, "0fd9:00a6", "2/4").is_err());
}

#[test]
fn serial_identity_follows_unit_instead_of_recycled_card() {
    let original = unit(ProfileId::WaveXlrMk2, 4);
    let replacement = unit(ProfileId::WaveXlrMk2, 5);
    let units = [
        ConnectedIdentity {
            unit: replacement,
            serial: "UNIT_B",
            connected: true,
        },
        ConnectedIdentity {
            unit: original,
            serial: "UNIT_A",
            connected: true,
        },
    ];
    assert_eq!(device_for_capture("UNIT_A", &units), Some(original));
    assert_eq!(
        device_for_capture("Elgato_Systems_Elgato_XLR_Dock_UNIT_A", &units),
        Some(original)
    );
    assert_eq!(device_for_capture("UNIT_A", &units[..1]), None);
    for serial in [
        "",
        "UNIT",
        "UNIT_AB",
        "Elgato_Systems_Elgato_XLR_Dock_UNIT_AB",
        "Elgato_Systems_Elgato_Wave_3_UNIT_A",
    ] {
        assert_eq!(device_for_capture(serial, &units), None);
    }
}

#[test]
fn absent_disconnected_and_duplicate_serials_never_authorize_a_target() {
    let original = ConnectedIdentity {
        unit: unit(ProfileId::WaveXlrMk2, 4),
        serial: "UNIT_A",
        connected: true,
    };
    let duplicate = ConnectedIdentity {
        unit: unit(ProfileId::WaveXlrMk2, 5),
        ..original
    };
    assert_eq!(device_for_capture("UNIT_A", &[original, duplicate]), None);
    assert_eq!(
        device_for_capture(
            "Elgato_Systems_Elgato_XLR_Dock_UNIT_A",
            &[original, duplicate]
        ),
        None
    );
    assert_eq!(
        device_for_capture(
            "UNIT_A",
            &[ConnectedIdentity {
                serial: "",
                ..original
            }]
        ),
        None
    );
    assert_eq!(
        device_for_capture(
            "UNIT_A",
            &[ConnectedIdentity {
                connected: false,
                ..original
            }]
        ),
        None
    );
    let other_model = ConnectedIdentity {
        unit: unit(ProfileId::Wave3, 6),
        ..original
    };
    assert_eq!(device_for_capture("UNIT_A", &[original, other_model]), None);
    assert_eq!(
        device_for_capture("Elgato_Systems_Elgato_XLR_Dock_UNIT_A", &[other_model]),
        None
    );
    assert_eq!(
        device_for_capture(
            "Elgato_Systems_Elgato_XLR_Dock_UNIT_A",
            &[original, other_model]
        ),
        Some(original.unit)
    );
}

#[test]
fn driver_names_and_real_ranges_determine_writes_not_fixed_numids() {
    let controls = parse_alsa_controls(
        "\
numid=2,iface=PCM,name='Capture Volume'
 ; type=INTEGER,access=rw------,values=1,min=0,max=36,step=0
numid=11,iface=MIXER,name='Wave XLR Mk3 Capture Switch'
 ; type=BOOLEAN,access=rw------,values=1
 : values=on
numid=12,iface=MIXER,name='Wave XLR Mk3 Capture Volume'
 ; type=INTEGER,access=rw---R--,values=1,min=0,max=200,step=0
 : values=0
numid=13,iface=MIXER,name='Wave XLR Mk3 Playback Volume'
 ; type=INTEGER,access=rw---R--,values=1,min=4,max=99,step=0
 : values=73
",
    );
    assert_eq!(controls.mute.unwrap().numid, 11);
    let gain = controls.gain.unwrap();
    assert_eq!(gain.numid, 12);
    assert_eq!(
        gain.clamp(fw_gain_to_alsa(ProfileId::WaveXlrMk2, 75 * 256))
            .unwrap(),
        150
    );
    assert_eq!(gain.clamp(250).unwrap(), 200);
    let hp = controls.hp_volume.unwrap();
    assert_eq!(hp.numid, 13);
    assert_eq!(hp.clamp(120).unwrap(), 99);
    assert_eq!(hp.clamp(0).unwrap(), 4);
}

#[test]
fn missing_controls_ranges_and_values_stay_unknown() {
    let absent = parse_alsa_controls("");
    assert!(absent.mute.is_none());
    assert!(absent.gain.is_none());
    assert!(absent.hp_volume.is_none());
    let unknown = parse_alsa_controls(
        "numid=8,iface=MIXER,name='Mic Capture Volume'\n ; type=INTEGER,values=1\n",
    );
    assert!(unknown.gain.unwrap().clamp(100).is_err());
    let malformed = parse_alsa_controls(
        "numid=8,iface=MIXER,name='Mic Capture Volume'\n ; type=INTEGER,values=1,min=100,max=0\n",
    );
    assert!(malformed.gain.unwrap().clamp(100).is_err());
    assert_eq!(parse_alsa_mute(""), None);
    assert_eq!(parse_alsa_mute(" : values=broken"), None);
    assert_eq!(parse_alsa_volume(" : values=bad"), None);
    assert_eq!(
        parse_alsa_mute(" ; type=BOOLEAN,values=1\n : values=off\n"),
        Some(true)
    );
    assert_eq!(parse_alsa_mute(" : values=on\n"), Some(false));
    assert_eq!(
        parse_alsa_volume(" ; type=INTEGER,values=1,min=0,max=120\n : values=73\n"),
        Some(73)
    );
}

#[test]
fn half_db_conversions_keep_ties_even_and_firmware_saturation() {
    let profile = ProfileId::Wave3;
    assert_eq!(fw_gain_to_alsa(profile, 64), 0);
    assert_eq!(fw_gain_to_alsa(profile, 192), 2);
    assert_eq!(fw_gain_to_alsa(profile, 320), 2);
    assert_eq!(fw_gain_to_alsa(ProfileId::WaveXlr, 80 * 256), 160);
    assert_eq!(fw_hp_to_alsa(profile, -64), 120);
    assert_eq!(fw_hp_to_alsa(profile, -192), 118);
    assert_eq!(fw_hp_to_alsa(profile, -320), 118);
    assert_eq!(fw_hp_to_alsa(profile, i16::MIN), 0);
    assert_eq!(fw_hp_to_alsa(profile, i16::MAX), 120);
    assert_eq!(alsa_hp_to_fw(profile, 0), -60 * 256);
    assert_eq!(alsa_hp_to_fw(profile, 73), -6016);
    assert_eq!(alsa_hp_to_fw(profile, i32::MIN), i16::MIN);
    assert_eq!(alsa_hp_to_fw(profile, i32::MAX), 0);
    for value in 0..=120 {
        assert_eq!(fw_hp_to_alsa(profile, alsa_hp_to_fw(profile, value)), value);
    }
}

use openwave_core::{
    model::{DeviceSetting, ErrorCode},
    profiles::ProfileId,
    protocol::{
        ConfigBuffer, KnobMode, MeterLevels, decode_device_info, decode_meters, validate_setting,
    },
};

#[test]
fn production_blocks_require_exact_model_lengths() {
    for (profile, config, meter, info) in [
        (ProfileId::WaveXlr, 34, 10, 51),
        (ProfileId::WaveXlrMk2, 34, 10, 51),
        (ProfileId::Wave3, 16, 8, 64),
    ] {
        for length in 0..=65 {
            let bytes = vec![0; length];
            assert_eq!(
                ConfigBuffer::decode(profile, &bytes).is_ok(),
                length == config
            );
            assert_eq!(decode_meters(profile, &bytes).is_ok(), length == meter);
            assert_eq!(decode_device_info(profile, &bytes).is_ok(), length == info);
        }
    }
}

#[test]
fn xlr_patches_preserve_every_reserved_byte_and_decode_signed_headphones() {
    for profile in [ProfileId::WaveXlr, ProfileId::WaveXlrMk2] {
        let original = [0xa5; 34];
        let mut block = ConfigBuffer::decode(profile, &original).unwrap();
        block.apply(DeviceSetting::GainRaw(u16::MAX)).unwrap();
        block.apply(DeviceSetting::Mute(false)).unwrap();
        block.apply(DeviceSetting::HeadphoneDb(-12.75)).unwrap();
        block.apply(DeviceSetting::Phantom(true)).unwrap();
        block.apply(DeviceSetting::LowImpedance(false)).unwrap();
        let mut expected = original;
        expected[0..2].copy_from_slice(&0x5000_u16.to_le_bytes());
        expected[4] = 0;
        expected[6] = 1;
        expected[9..11].copy_from_slice(&(-3264_i16).to_le_bytes());
        expected[33] = 0;
        assert_eq!(block.as_bytes(), expected);
        assert_eq!(
            block.blocks().collect::<Vec<_>>(),
            vec![(0, expected.as_slice())]
        );
        let state = block.state();
        assert_eq!(state.gain_raw, 0x5000);
        assert_eq!(state.hp_volume_db, -12.75);
        assert!(!state.muted);
        assert_eq!(state.phantom, Some(true));
        assert_eq!(state.low_impedance, Some(false));
        // Observations are not clamped; only 007d has a mapped mix field.
        let expected_mix = (profile == ProfileId::WaveXlr).then_some(0xa5a5);
        assert_eq!(state.monitor_mix, expected_mix);
    }
}

/// Config block captured from a physical 007d (firmware 3.7.3, API 1.4) with
/// `openwave-probe watch`; it contains no serial. Turning the knob in mix mode
/// changed only bytes 12-13, in steps of 1/24 from 0x0000 (PC only) to
/// 0x6400 (microphone only). 0x14d5 is the value seen after a power cycle.
const WAVE_XLR_CAPTURE: [u8; 34] = [
    0x40, 0x38, 0x00, 0xec, 0x00, 0x01, 0x01, 0x01, 0x00, 0x80, 0xed, 0x00, 0xd5, 0x14, 0x03, 0xff,
    0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01, 0x00, 0xff, 0x37, 0x00,
    0x00, 0x01,
];

#[test]
fn wave_xlr_monitor_mix_is_a_bounded_u16_at_offset_12() {
    let mut block = ConfigBuffer::decode(ProfileId::WaveXlr, &WAVE_XLR_CAPTURE).unwrap();
    let state = block.state();
    assert_eq!(state.gain_raw, 0x3840);
    assert_eq!(state.hp_volume_db, -18.5);
    assert_eq!(state.phantom, Some(true));
    assert_eq!(state.low_impedance, Some(true));
    assert_eq!(state.monitor_mix, Some(0x14d5));
    assert_eq!(state.knob_mode, KnobMode::MonitorMix);

    for (requested, stored) in [(0, 0x0000_u16), (0x3200, 0x3200), (u16::MAX, 0x6400)] {
        block.apply(DeviceSetting::MonitorMix(requested)).unwrap();
        let mut expected = WAVE_XLR_CAPTURE;
        expected[12..14].copy_from_slice(&stored.to_le_bytes());
        assert_eq!(block.as_bytes(), expected);
        assert_eq!(block.state().monitor_mix, Some(stored));
    }
}

#[test]
fn wave3_patches_use_its_own_bounds_and_capabilities() {
    let mut bytes = [0x55; 16];
    bytes[12] = 3;
    let mut block = ConfigBuffer::decode(ProfileId::Wave3, &bytes).unwrap();
    block.apply(DeviceSetting::GainRaw(0x5000)).unwrap();
    block.apply(DeviceSetting::MonitorMix(u16::MAX)).unwrap();
    block.apply(DeviceSetting::HeadphoneDb(-200.0)).unwrap();
    let mut expected = bytes;
    expected[0..2].copy_from_slice(&0x2800_u16.to_le_bytes());
    expected[7..9].copy_from_slice(&i16::MIN.to_le_bytes());
    expected[10..12].copy_from_slice(&0x6400_u16.to_le_bytes());
    assert_eq!(block.as_bytes(), expected);
    let state = block.state();
    assert_eq!(state.gain_raw, 0x2800);
    assert_eq!(state.hp_volume_db, -128.0);
    assert_eq!(state.monitor_mix, Some(0x6400));
    assert_eq!(state.phantom, None);
    assert_eq!(state.low_impedance, None);
    assert_eq!(state.knob_mode, KnobMode::MonitorMix);
    block.apply(DeviceSetting::HeadphoneDb(10.0)).unwrap();
    assert_eq!(block.state().hp_volume_db, 0.0);
    block.apply(DeviceSetting::HeadphoneDb(-0.001)).unwrap();
    assert_eq!(block.state().hp_volume_db, 0.0); // integer conversion truncates, not rounds
}

#[test]
fn refused_settings_leave_config_unchanged_and_fail_queue_admission() {
    for (profile, setting) in [
        (ProfileId::Wave3, DeviceSetting::Phantom(true)),
        (ProfileId::Wave3, DeviceSetting::LowImpedance(true)),
        (ProfileId::WaveXlrMk2, DeviceSetting::MonitorMix(0)),
    ] {
        let bytes = vec![0x6a; profile.profile().legacy.unwrap().config_len];
        let mut block = ConfigBuffer::decode(profile, &bytes).unwrap();
        assert_eq!(
            validate_setting(profile, setting).unwrap_err().code,
            ErrorCode::Unsupported
        );
        assert_eq!(
            block.apply(setting).unwrap_err().code,
            ErrorCode::Unsupported
        );
        assert_eq!(block.as_bytes(), bytes);
    }
    let mut block = ConfigBuffer::decode(ProfileId::Wave3, &[0x23; 16]).unwrap();
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(
            block
                .apply(DeviceSetting::HeadphoneDb(value))
                .unwrap_err()
                .code,
            ErrorCode::Invalid
        );
        assert_eq!(block.as_bytes(), [0x23; 16]);
    }
}

#[test]
fn observed_out_of_range_values_are_not_rewritten_or_clamped() {
    let mut bytes = [0; 16];
    bytes[0..2].copy_from_slice(&u16::MAX.to_le_bytes());
    bytes[4] = 2;
    bytes[7..9].copy_from_slice(&256_i16.to_le_bytes());
    bytes[10..12].copy_from_slice(&u16::MAX.to_le_bytes());
    let block = ConfigBuffer::decode(ProfileId::Wave3, &bytes).unwrap();
    let state = block.state();
    assert_eq!(state.gain_raw, u16::MAX);
    assert_eq!(state.hp_volume_db, 1.0);
    assert_eq!(state.monitor_mix, Some(u16::MAX));
    assert!(state.muted);
    assert_eq!(block.as_bytes(), bytes);
}

#[test]
fn knob_modes_follow_profile_specific_mapping_and_unknown_means_gain() {
    for (profile, length, offset) in [
        (ProfileId::WaveXlr, 34, 14),
        (ProfileId::WaveXlrMk2, 34, 14),
        (ProfileId::Wave3, 16, 12),
    ] {
        for (raw, expected) in [
            (0, KnobMode::Gain),
            (1, KnobMode::Gain),
            (2, KnobMode::Headphones),
            (
                3,
                if profile.profile().has_monitor_mix() {
                    KnobMode::MonitorMix
                } else {
                    KnobMode::Gain
                },
            ),
            (255, KnobMode::Gain),
        ] {
            let mut bytes = vec![0; length];
            bytes[offset] = raw;
            assert_eq!(
                ConfigBuffer::decode(profile, &bytes)
                    .unwrap()
                    .state()
                    .knob_mode,
                expected
            );
        }
    }
}

#[test]
fn info_uses_model_offsets_and_ascii_replacement_without_trimming_content() {
    for (profile, length, fw, serial) in [
        (ProfileId::WaveXlr, 51, 6, 27),
        (ProfileId::WaveXlrMk2, 51, 6, 27),
        (ProfileId::Wave3, 64, 21, 36),
    ] {
        let mut bytes = vec![0; length];
        bytes[0..2].copy_from_slice(&[1, 2]);
        bytes[fw..fw + 3].copy_from_slice(&[3, 4, 5]);
        bytes[serial..serial + 8].copy_from_slice(&[b' ', b'A', 0, 0xc3, 0xa9, b'Z', b' ', 0]);
        let info = decode_device_info(profile, &bytes).unwrap();
        assert_eq!(info.api, "1.2");
        assert_eq!(info.firmware, "3.4.5");
        assert_eq!(info.serial, " A\0\u{fffd}\u{fffd}Z ");
    }
}

#[test]
fn vendor_meters_decode_unsigned_words_and_ignore_reserved_tail() {
    for (profile, length) in [
        (ProfileId::WaveXlr, 10),
        (ProfileId::WaveXlrMk2, 10),
        (ProfileId::Wave3, 8),
    ] {
        let mut bytes = vec![0xaa; length];
        bytes[0..4].copy_from_slice(&0x8000_0001_u32.to_le_bytes());
        bytes[4..8].copy_from_slice(&0xfedc_ba98_u32.to_le_bytes());
        assert_eq!(
            decode_meters(profile, &bytes).unwrap(),
            MeterLevels {
                left: 0x8000_0001,
                right: 0xfedc_ba98
            }
        );
    }
}

#[test]
fn dock_requires_all_exact_blocks_and_refuses_legacy_decoders() {
    for length in 0..=39 {
        let bytes = vec![0; length];
        assert_eq!(
            ConfigBuffer::decode_dock(&bytes, &[0; 2], &[0; 6]).is_ok(),
            length == 38
        );
        assert_eq!(
            ConfigBuffer::decode_dock(&[0; 38], &bytes, &[0; 6]).is_ok(),
            length == 2
        );
        assert_eq!(
            ConfigBuffer::decode_dock(&[0; 38], &[0; 2], &bytes).is_ok(),
            length == 6
        );
        assert_eq!(
            ConfigBuffer::decode(ProfileId::XlrDockMk2, &bytes)
                .unwrap_err()
                .code,
            ErrorCode::Unsupported
        );
        assert_eq!(
            decode_device_info(ProfileId::XlrDockMk2, &bytes)
                .unwrap_err()
                .code,
            ErrorCode::Unsupported
        );
        assert_eq!(
            decode_meters(ProfileId::XlrDockMk2, &bytes)
                .unwrap_err()
                .code,
            ErrorCode::Unsupported
        );
    }
}

#[test]
fn dock_patches_preserve_reserved_bits_and_every_unrelated_block() {
    for flags in 0..=u8::MAX {
        let mut settings = [0xa5; 38];
        settings[0] = 35;
        settings[1] = flags;
        let headphones = [51, flags];
        let monitor = [100, 0x81, 0x32, 0x43, 0x54, 0x65];
        let original = ConfigBuffer::decode_dock(&settings, &headphones, &monitor).unwrap();
        assert_eq!(original.state().muted, flags & 1 != 0);
        assert_eq!(original.state().phantom, Some(flags & 2 != 0));
        assert_eq!(original.state().low_impedance, Some(flags & 2 != 0));
        assert_eq!(original.state().hp_volume_db, -12.75);
        assert_eq!(original.state().gain_raw, 35);
        assert_eq!(original.state().monitor_mix, Some(100));
        assert_eq!(original.state().knob_mode, KnobMode::None);
        for setting in [
            DeviceSetting::GainRaw(80),
            DeviceSetting::Mute(false),
            DeviceSetting::Mute(true),
            DeviceSetting::Phantom(false),
            DeviceSetting::Phantom(true),
            DeviceSetting::HeadphoneDb(-60.0),
            DeviceSetting::LowImpedance(false),
            DeviceSetting::LowImpedance(true),
            DeviceSetting::MonitorMix(200),
        ] {
            let mut expected_settings = settings;
            let mut expected_headphones = headphones;
            let mut expected_monitor = monitor;
            match setting {
                DeviceSetting::GainRaw(_) => expected_settings[0] = 80,
                DeviceSetting::Mute(on) => expected_settings[1] = (flags & !1) | u8::from(on),
                DeviceSetting::Phantom(on) => {
                    expected_settings[1] = (flags & !2) | (u8::from(on) << 1)
                }
                DeviceSetting::HeadphoneDb(_) => expected_headphones[0] = 240,
                DeviceSetting::LowImpedance(on) => {
                    expected_headphones[1] = (flags & !2) | (u8::from(on) << 1)
                }
                DeviceSetting::MonitorMix(_) => expected_monitor[0] = 200,
            }
            let mut patched = original.clone();
            patched.apply(setting).unwrap();
            assert_eq!(patched.as_bytes(), expected_settings);
            assert_eq!(
                patched.blocks().collect::<Vec<_>>(),
                vec![
                    (4, expected_settings.as_slice()),
                    (5, expected_headphones.as_slice()),
                    (1, expected_monitor.as_slice())
                ],
            );
        }
    }
}

#[test]
fn dock_bounds_rounding_and_nonfinite_rejection_preserve_observations() {
    let mut block = ConfigBuffer::decode_dock(&[255; 38], &[255; 2], &[255; 6]).unwrap();
    assert_eq!(block.state().gain_raw, 255);
    assert_eq!(block.state().hp_volume_db, -63.75);
    assert_eq!(block.state().monitor_mix, Some(255));
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let before = block.clone();
        assert_eq!(
            block
                .apply(DeviceSetting::HeadphoneDb(value))
                .unwrap_err()
                .code,
            ErrorCode::Invalid
        );
        assert_eq!(block, before);
    }
    for (db, expected) in [
        (-100.0, -60.0),
        (-60.0, -60.0),
        (-59.875, -60.0),
        (-0.375, -0.5),
        (-0.125, 0.0),
        (-0.126, -0.25),
        (10.0, 0.0),
    ] {
        block.apply(DeviceSetting::HeadphoneDb(db)).unwrap();
        assert_eq!(block.state().hp_volume_db, expected);
    }
    for (value, expected) in [(0, 0), (80, 80), (81, 80), (u16::MAX, 80)] {
        block.apply(DeviceSetting::GainRaw(value)).unwrap();
        assert_eq!(block.state().gain_raw, expected);
    }
    for (value, expected) in [(0, 0), (100, 100), (200, 200), (201, 200), (u16::MAX, 200)] {
        block.apply(DeviceSetting::MonitorMix(value)).unwrap();
        assert_eq!(block.state().monitor_mix, Some(expected));
    }
}

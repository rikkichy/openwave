use openwave_core::{
    calibration::{Metrics, analyze, analyze_tone, metrics_from_raw},
    effects::FxSettings,
};

fn pcm(left: i16, right: Option<i16>, frames: usize) -> Vec<u8> {
    let mut raw = Vec::with_capacity(frames * if right.is_some() { 4 } else { 2 });
    for _ in 0..frames {
        raw.extend(left.to_le_bytes());
        if let Some(right) = right {
            raw.extend(right.to_le_bytes());
        }
    }
    raw
}

fn tone(balance: f64, sub_db: f64, voice_low_db: f64, tilt_db: f64) -> Metrics {
    Metrics {
        peaks_db: Vec::new(),
        balance,
        sub_db,
        voice_low_db,
        tilt_db,
    }
}

#[test]
fn malformed_and_short_pcm_never_becomes_a_measurement() {
    for (raw, channels) in [
        (vec![], 2),
        (vec![0], 1),
        (vec![0, 0], 2),
        (pcm(20, None, 800), 1),
        (pcm(20, None, 24000), 3),
    ] {
        assert!(metrics_from_raw(&raw, channels).is_err());
    }
}

#[test]
fn all_complete_windows_preserve_opposite_polarity_stereo_and_louder_channel() {
    let metrics = metrics_from_raw(&pcm(8192, Some(-8192), 24000), 2).unwrap();
    assert_eq!(metrics.peaks_db.len(), 30);
    assert!((metrics.peaks_db[0] - -12.041199826559248).abs() < 1e-12);
    assert_eq!(metrics.balance, 1.0);
    let lopsided = metrics_from_raw(&pcm(8192, Some(0), 24000), 2).unwrap();
    assert_eq!(lopsided.balance, 0.0);
    assert_eq!(lopsided.peaks_db, metrics.peaks_db);
    let swapped = metrics_from_raw(&pcm(0, Some(8192), 24000), 2).unwrap();
    assert_eq!(swapped, lopsided);
    let partial = metrics_from_raw(&pcm(8192, None, 24799), 1).unwrap();
    assert_eq!(partial.peaks_db.len(), 30);
    assert_eq!(partial.balance, 1.0);
}

#[test]
fn silence_uses_minus_140_floor_without_nan_tone() {
    let metrics = metrics_from_raw(&pcm(0, Some(0), 24000), 2).unwrap();
    assert_eq!(metrics.peaks_db, vec![-140.0; 30]);
    assert_eq!(metrics.balance, 1.0);
    assert_eq!(metrics.sub_db, 0.0);
    assert_eq!(metrics.voice_low_db, 0.0);
    assert_eq!(metrics.tilt_db, 0.0);
}

#[test]
fn clipping_cannot_hide_in_one_channel_or_overflow_negative_full_scale() {
    for sample in [32767, -32768] {
        let error = metrics_from_raw(&pcm(sample, Some(0), 24000), 2).unwrap_err();
        assert!(error.message.contains("clipping"));
    }
    let mut raw = pcm(100, Some(0), 24000);
    for frame in 0..47 {
        raw[frame * 4..frame * 4 + 2].copy_from_slice(&32760_i16.to_le_bytes());
    }
    assert!(metrics_from_raw(&raw, 2).is_ok());
    raw[47 * 4..47 * 4 + 2].copy_from_slice(&32760_i16.to_le_bytes());
    assert!(
        metrics_from_raw(&raw, 2)
            .unwrap_err()
            .message
            .contains("clipping")
    );
}

#[test]
fn silence_quiet_speech_noise_and_clipping_are_not_safe_proposals() {
    for (floor, speech, reason) in [
        (-140.0, -140.0, "Speech"),
        (-90.0, -60.0, "quiet"),
        (-20.0, -5.0, "noise floor"),
        (-40.0, -25.0, "noise floor"),
        (-70.0, -0.01, "clipping"),
    ] {
        assert!(
            analyze(&vec![floor; 180], &vec![speech; 300])
                .unwrap_err()
                .message
                .contains(reason)
        );
    }
    for values in [
        vec![],
        vec![-30.0; 2],
        vec![f64::NAN; 30],
        vec![f64::INFINITY; 30],
        vec![1.0; 30],
        vec![-141.0; 30],
    ] {
        assert!(analyze(&[-70.0; 180], &values).is_err());
    }
}

#[test]
fn known_percentile_proposal_does_not_interpolate_or_modify_inputs() {
    let floor = vec![-65.0; 180];
    let mut speech = vec![-24.0; 270];
    speech.extend([-12.0; 30]);
    let original = speech.clone();
    let analysis = analyze(&floor, &speech).unwrap();
    assert_eq!(speech, original);
    assert_eq!(analysis.measured.floor_db, -65.0);
    assert_eq!(analysis.measured.quiet_voice_db, -24.0);
    assert_eq!(analysis.measured.loud_voice_db, -12.0);
    assert_eq!(analysis.fx.gate_thresh, -57.0);
    assert_eq!(analysis.fx.comp_thresh, -18.0);
    assert_eq!(analysis.fx.comp_ratio, 3.0);
    let low = analyze(&[-90.0; 180], &[-44.0; 300]).unwrap();
    assert_eq!(low.fx.gate_thresh, -70.0);
    assert_eq!(low.fx.comp_thresh, -30.0);
}

#[test]
fn decimal_and_integer_proposals_use_ties_to_even() {
    for (floor, rounded) in [(-56.25, -56.2), (-56.75, -56.8), (-56.95, -57.0)] {
        assert_eq!(
            analyze(&[floor; 180], &[-20.0; 300])
                .unwrap()
                .measured
                .floor_db,
            rounded
        );
    }
    let floor = tone(1.0, -10.0, -30.0, -15.0);
    for (tilt, shelf) in [(-16.0, 0.0), (-18.0, 2.0), (-14.0, 0.0), (-12.0, -2.0)] {
        assert_eq!(
            analyze_tone(&floor, &tone(1.0, -20.0, -20.0, tilt))
                .unwrap()
                .eq_high,
            shelf
        );
    }
}

#[test]
fn tone_protects_deep_voice_bounds_shelf_and_only_proposes_mono_when_needed() {
    let floor = tone(1.0, -2.0, -30.0, -15.0);
    let speech = tone(0.0, -20.0, -8.0, -40.0);
    let proposal = analyze_tone(&floor, &speech).unwrap();
    assert_eq!(proposal.lowcut, 80);
    assert_eq!(proposal.eq_high, 4.0);
    assert_eq!(proposal.mono, Some(true));
    let other = analyze_tone(&floor, &tone(1.0, -20.0, -20.0, -5.0)).unwrap();
    assert_eq!(other.lowcut, 120);
    assert_eq!(other.eq_high, -4.0);
    assert_eq!(other.mono, None);
    assert!(analyze_tone(&floor, &tone(1.0, -20.0, -20.0, f64::NAN)).is_err());
    assert!(analyze_tone(&floor, &tone(1.1, -20.0, -20.0, -10.0)).is_err());
}

#[test]
fn accepting_candidate_preserves_controls_not_proposed() {
    let existing = FxSettings {
        eq_low: 3.0,
        eq_mid: -2.0,
        mono: true,
        delay_ms: 25.0,
        ..FxSettings::default()
    };
    let original = existing.clone();
    let analysis = analyze(&[-65.0; 180], &[-20.0; 300]).unwrap();
    let tone = analyze_tone(
        &tone(1.0, -10.0, -30.0, -15.0),
        &tone(1.0, -20.0, -20.0, -20.0),
    )
    .unwrap();
    let candidate = analysis.proposed_settings(&existing, Some(&tone)).unwrap();
    assert_eq!(existing, original);
    assert_eq!(candidate.eq_low, 3.0);
    assert_eq!(candidate.eq_mid, -2.0);
    assert_eq!(candidate.delay_ms, 25.0);
    assert!(candidate.mono);
    assert!(candidate.gate && candidate.comp);
    assert_eq!(candidate.eq_high, 2.0);
}

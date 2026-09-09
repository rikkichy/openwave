use serde::{Deserialize, Serialize};

use crate::{
    effects::{FxSettings, capture_channels},
    model::{OperationError, Result},
};

pub const RATE: u32 = 48_000;
pub const WINDOW_FRAMES: usize = 800;
pub const MIN_WINDOWS: usize = 30;
pub const FLOOR_SECONDS: u32 = 3;
pub const SPEECH_SECONDS: u32 = 5;
pub const GRACE_SECONDS: u32 = 3;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Metrics {
    pub peaks_db: Vec<f64>,
    pub balance: f64,
    pub sub_db: f64,
    pub voice_low_db: f64,
    pub tilt_db: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MeasuredLevels {
    pub floor_db: f64,
    pub quiet_voice_db: f64,
    pub loud_voice_db: f64,
}

/// Only the proposed controls, so accepting calibration preserves other effects.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DynamicsProposal {
    pub gate: bool,
    pub gate_thresh: f64,
    pub comp: bool,
    pub comp_thresh: f64,
    pub comp_ratio: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Analysis {
    pub measured: MeasuredLevels,
    pub fx: DynamicsProposal,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToneProposal {
    pub lowcut: u16,
    pub eq_high: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mono: Option<bool>,
}

impl Analysis {
    /// Construct an acceptance candidate without applying or persisting it.
    pub fn proposed_settings(
        &self,
        existing: &FxSettings,
        tone: Option<&ToneProposal>,
    ) -> Result<FxSettings> {
        let mut next = existing.clone();
        next.gate = self.fx.gate;
        next.gate_thresh = self.fx.gate_thresh;
        next.comp = self.fx.comp;
        next.comp_thresh = self.fx.comp_thresh;
        next.comp_ratio = self.fx.comp_ratio;
        if let Some(tone) = tone {
            next.lowcut = tone.lowcut;
            next.eq_high = tone.eq_high;
            if let Some(mono) = tone.mono {
                next.mono = mono;
            }
        }
        next.validated()
    }
}

fn clipping() -> OperationError {
    OperationError::invalid("The microphone is clipping; lower its hardware gain and try again.")
}

/// Complete little-endian signed-16 frames, with the startup transient already dropped.
/// Peaks inspect every channel; tone uses the first highest-energy channel.
pub fn metrics_from_raw(raw: &[u8], channels: u32) -> Result<Metrics> {
    let channels = capture_channels(Some(channels))? as usize;
    let frame_bytes = channels * 2;
    if raw.len() % frame_bytes != 0 {
        return Err(OperationError::invalid(
            "malformed audio: expected complete signed-16 PCM frames",
        ));
    }
    let frames = raw.len() / frame_bytes;
    if frames < WINDOW_FRAMES * MIN_WINDOWS {
        return Err(OperationError::invalid(
            "not enough audio was measured; record at least half a second",
        ));
    }
    let mut energy_sums = [0_u128; 2];
    let mut clipped = 0_usize;
    let mut window_peak = 0_i32;
    let mut peaks_db = Vec::with_capacity(frames / WINDOW_FRAMES);
    for (index, frame) in raw.chunks_exact(frame_bytes).enumerate() {
        for (channel, bytes) in frame.chunks_exact(2).enumerate() {
            let sample = i32::from(i16::from_le_bytes([bytes[0], bytes[1]]));
            clipped += usize::from(sample.abs() >= 32760);
            energy_sums[channel] += (i64::from(sample) * i64::from(sample)) as u128;
            window_peak = window_peak.max(sample.abs());
        }
        if (index + 1) % WINDOW_FRAMES == 0 {
            let peak = f64::from(window_peak) / 32768.0;
            peaks_db.push(20.0 * peak.max(1e-7).log10());
            window_peak = 0;
        }
    }
    if clipped as f64 >= (3.0_f64).max((frames * channels) as f64 * 0.001) {
        return Err(clipping());
    }
    let energies = [
        energy_sums[0] as f64 / frames as f64,
        energy_sums[1] as f64 / frames as f64,
    ];
    let tone_channel = usize::from(channels == 2 && energies[1] > energies[0]);
    let total = energies[tone_channel];
    let balance = if total == 0.0 || channels == 1 {
        1.0
    } else {
        energies[0].min(energies[1]) / total
    };
    let coefficients = [90.0, 180.0, 2000.0]
        .map(|cutoff| (-2.0 * std::f64::consts::PI * cutoff / f64::from(RATE)).exp());
    let mut filter = [0.0; 3];
    let mut accumulators = [0.0; 3];
    // No deinterleaving copy: all three one-pole filters see the same selected track.
    for frame in raw.chunks_exact(frame_bytes) {
        let offset = tone_channel * 2;
        let sample = f64::from(i16::from_le_bytes([frame[offset], frame[offset + 1]]));
        for index in 0..3 {
            let a = coefficients[index];
            filter[index] = (1.0 - a) * sample + a * filter[index];
            accumulators[index] += filter[index] * filter[index];
        }
    }
    let [e90, e180, e2k] = accumulators.map(|energy| energy / frames as f64);
    let db = |energy: f64| 10.0 * (energy.max(1e-9) / total.max(1e-9)).log10();
    Ok(Metrics {
        peaks_db,
        balance,
        sub_db: db(e90),
        voice_low_db: db(e180 - e90),
        tilt_db: db(total - e2k),
    })
}

pub fn analyze_tone(floor: &Metrics, speech: &Metrics) -> Result<ToneProposal> {
    for metrics in [floor, speech] {
        if [
            metrics.balance,
            metrics.sub_db,
            metrics.voice_low_db,
            metrics.tilt_db,
        ]
        .iter()
        .any(|value| !value.is_finite())
        {
            return Err(OperationError::invalid("malformed tone measurements"));
        }
        if !(0.0..=1.0).contains(&metrics.balance) {
            return Err(OperationError::invalid("malformed channel balance"));
        }
    }
    let deep_voice = speech.voice_low_db > -12.0;
    let rumbly_floor = floor.sub_db > -6.0;
    Ok(ToneProposal {
        lowcut: if deep_voice || !rumbly_floor { 80 } else { 120 },
        eq_high: ((-15.0 - speech.tilt_db) * 0.5)
            .round_ties_even()
            .clamp(-4.0, 4.0),
        mono: if speech.balance < 0.05 {
            Some(true)
        } else {
            None
        },
    })
}

/// Preserve the original order statistic: index floor(n*p/100), capped at n-1.
/// In particular the 90th percentile of 300 values is element 270, not 269.
fn percentile(ordered: &[f64], pct: usize) -> f64 {
    ordered[(ordered.len() * pct / 100).min(ordered.len() - 1)]
}

// Round the exact binary input to decimal tenths with ties-to-even. Multiplying
// in f64 first can manufacture a tie (e.g. -56.95), unlike Python round(x, 1).
// Callers supply finite values in [-140, 0], so u128 suffices for the significand.
fn round_tenth(value: f64) -> f64 {
    if value == 0.0 {
        return value;
    }
    let bits = value.abs().to_bits();
    let exponent = ((bits >> 52) & 0x7ff) as i32 - 1023 - 52;
    let significand = u128::from((bits & ((1_u64 << 52) - 1)) | (1_u64 << 52));
    let numerator = significand * 10;
    if exponent < -127 {
        return 0.0_f64.copysign(value);
    }
    let shift = (-exponent) as u32;
    let divisor = 1_u128 << shift;
    let integer = numerator / divisor;
    let remainder = numerator % divisor;
    let rounded = integer
        + u128::from(remainder > divisor / 2 || (remainder == divisor / 2 && integer % 2 == 1));
    (rounded as f64 / 10.0).copysign(value)
}

pub fn analyze(floor_peaks_db: &[f64], speech_peaks_db: &[f64]) -> Result<Analysis> {
    for peaks in [floor_peaks_db, speech_peaks_db] {
        if peaks.len() < MIN_WINDOWS {
            return Err(OperationError::invalid(
                "not enough audio was measured; repeat both recording phases",
            ));
        }
        if peaks
            .iter()
            .any(|peak| !peak.is_finite() || !(-140.0..=0.0).contains(peak))
        {
            return Err(OperationError::invalid(
                "malformed audio level measurements",
            ));
        }
    }
    if floor_peaks_db
        .iter()
        .chain(speech_peaks_db)
        .any(|peak| *peak >= -0.1)
    {
        return Err(clipping());
    }
    let mut floor_ordered = floor_peaks_db.to_vec();
    floor_ordered.sort_unstable_by(f64::total_cmp);
    let floor = percentile(&floor_ordered, 50);
    if floor > -25.0 {
        return Err(OperationError::invalid(
            "The noise floor is too high; reduce room noise or hardware gain and try again.",
        ));
    }
    let mut voiced: Vec<_> = speech_peaks_db
        .iter()
        .copied()
        .filter(|peak| *peak > floor + 10.0)
        .collect();
    if (voiced.len() as f64) < (MIN_WINDOWS as f64).max(speech_peaks_db.len() as f64 * 0.1) {
        return Err(OperationError::invalid(
            "Speech was not clearly above the noise floor; speak longer and closer to the microphone.",
        ));
    }
    voiced.sort_unstable_by(f64::total_cmp);
    let quiet_voice = percentile(&voiced, 10);
    let loud_voice = percentile(&voiced, 90);
    if loud_voice < -45.0 || quiet_voice < -58.0 {
        return Err(OperationError::invalid(
            "Speech is too quiet; move closer or increase hardware gain, then try again.",
        ));
    }
    if quiet_voice - floor < 18.0 {
        return Err(OperationError::invalid(
            "Speech is too close to the noise floor; reduce room noise or move closer.",
        ));
    }
    let gate_thresh = (floor + 8.0).min(quiet_voice - 6.0).clamp(-70.0, -20.0);
    let validated = FxSettings {
        gate: true,
        gate_thresh: round_tenth(gate_thresh),
        comp: true,
        comp_thresh: round_tenth(loud_voice - 6.0),
        comp_ratio: 3.0,
        ..FxSettings::default()
    }
    .validated()?;
    Ok(Analysis {
        measured: MeasuredLevels {
            floor_db: round_tenth(floor),
            quiet_voice_db: round_tenth(quiet_voice),
            loud_voice_db: round_tenth(loud_voice),
        },
        fx: DynamicsProposal {
            gate: validated.gate,
            gate_thresh: validated.gate_thresh,
            comp: validated.comp,
            comp_thresh: validated.comp_thresh,
            comp_ratio: validated.comp_ratio,
        },
    })
}

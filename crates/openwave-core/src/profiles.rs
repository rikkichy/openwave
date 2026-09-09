use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::model::{OperationError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProfileId {
    #[serde(rename = "wave_xlr")]
    WaveXlr,
    #[serde(rename = "wave_xlr_mk2")]
    WaveXlrMk2,
    #[serde(rename = "wave3")]
    Wave3,
}

impl ProfileId {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WaveXlr => "wave_xlr",
            Self::WaveXlrMk2 => "wave_xlr_mk2",
            Self::Wave3 => "wave3",
        }
    }

    pub const fn profile(self) -> &'static DeviceProfile {
        match self {
            Self::WaveXlr => &PROFILES[0],
            Self::WaveXlrMk2 => &PROFILES[1],
            Self::Wave3 => &PROFILES[2],
        }
    }
}

impl fmt::Display for ProfileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ProfileId {
    type Err = OperationError;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "wave_xlr" => Ok(Self::WaveXlr),
            "wave_xlr_mk2" => Ok(Self::WaveXlrMk2),
            "wave3" => Ok(Self::Wave3),
            _ => Err(OperationError::invalid(format!(
                "Unknown device profile: {value}"
            ))),
        }
    }
}

/// Only the exact enabled VID:PID table authorizes production protocol access.
/// Offsets are byte offsets and every multibyte field is little-endian.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceProfile {
    pub id: ProfileId,
    pub display_name: &'static str,
    pub vid: u16,
    pub pid: u16,
    pub wvalue_config: u16,
    pub wvalue_meter: u16,
    pub wvalue_devinfo: u16,
    pub windex: u16,
    pub config_len: usize,
    pub meter_len: usize,
    pub devinfo_len: usize,
    pub devinfo_api: [usize; 2],
    pub devinfo_fw: [usize; 3],
    pub devinfo_serial: (usize, usize),
    pub off_gain: usize,
    pub gain_max: u16,
    pub gain_scale: u16,
    pub off_mute: usize,
    pub off_hp_vol: usize,
    pub hp_scale: u16,
    pub off_vol_select: usize,
    pub off_low_z: Option<usize>,
    pub off_phantom: Option<usize>,
    pub off_monitor_mix: Option<usize>,
    pub mix_max: u16,
    pub capture_serial_prefix: &'static str,
    pub sync_alsa_mute: bool,
    pub sync_alsa_hp: bool,
    pub sync_alsa_gain: bool,
}

impl DeviceProfile {
    pub const fn has_phantom(&self) -> bool {
        self.off_phantom.is_some()
    }
    pub const fn has_low_z(&self) -> bool {
        self.off_low_z.is_some()
    }
    pub const fn has_monitor_mix(&self) -> bool {
        self.off_monitor_mix.is_some()
    }
}

const XLR: DeviceProfile = DeviceProfile {
    id: ProfileId::WaveXlr,
    display_name: "Wave XLR",
    vid: 0x0fd9,
    pid: 0x007d,
    wvalue_config: 0,
    wvalue_meter: 1,
    wvalue_devinfo: 0x000a,
    windex: 0x3303,
    config_len: 34,
    meter_len: 10,
    devinfo_len: 51,
    devinfo_api: [0, 1],
    devinfo_fw: [6, 7, 8],
    devinfo_serial: (27, 47),
    off_gain: 0,
    gain_max: 0x5000,
    gain_scale: 256,
    off_mute: 4,
    off_hp_vol: 9,
    hp_scale: 256,
    off_vol_select: 14,
    off_low_z: Some(33),
    off_phantom: Some(6),
    off_monitor_mix: None,
    mix_max: 0,
    capture_serial_prefix: "Elgato_Systems_Elgato_Wave_XLR_",
    sync_alsa_mute: true,
    sync_alsa_hp: true,
    sync_alsa_gain: false,
};

pub static PROFILES: [DeviceProfile; 3] = [
    XLR,
    DeviceProfile {
        id: ProfileId::WaveXlrMk2,
        display_name: "Wave XLR MK.2 (0fd9:00a6)",
        pid: 0x00a6,
        capture_serial_prefix: "Elgato_Systems_Elgato_XLR_Dock_",
        ..XLR
    },
    DeviceProfile {
        id: ProfileId::Wave3,
        display_name: "Wave:3",
        pid: 0x0070,
        config_len: 16,
        meter_len: 8,
        devinfo_len: 64,
        devinfo_fw: [21, 22, 23],
        devinfo_serial: (36, 48),
        gain_max: 0x2800,
        off_hp_vol: 7,
        off_vol_select: 12,
        off_low_z: None,
        off_phantom: None,
        off_monitor_mix: Some(10),
        mix_max: 0x6400,
        capture_serial_prefix: "Elgato_Systems_Elgato_Wave_3_",
        sync_alsa_gain: true,
        ..XLR
    },
];

pub fn profile_for_usb(vid: u16, pid: u16) -> Option<&'static DeviceProfile> {
    PROFILES
        .iter()
        .find(|profile| profile.vid == vid && profile.pid == pid)
}

"""Per-model device profiles: USB protocol constants and capabilities.

Config offsets set to None mean the device lacks that feature.
"""

from dataclasses import dataclass, replace


@dataclass(frozen=True)
class DeviceProfile:
    key: str
    display_name: str
    vid: int
    pid: int
    wvalue_config: int
    wvalue_meter: int
    wvalue_devinfo: int
    windex: int
    config_len: int
    meter_len: int
    devinfo_len: int
    devinfo_api: tuple
    devinfo_fw: tuple
    devinfo_serial: tuple
    off_gain: int
    gain_max: int
    gain_scale: int | None  # raw units per dB; None = opaque raw (hex display)
    off_mute: int
    off_hp_vol: int
    hp_fmt: str  # struct format of the HP volume field
    hp_scale: int  # raw units per dB
    off_vol_select: int | None
    vol_select_map: dict
    off_low_z: int | None
    off_monitor_mix: int | None
    mix_max: int
    card_match: tuple
    sync_alsa_mute: bool
    sync_alsa_hp: bool
    sync_alsa_gain: bool

    @property
    def has_low_z(self):
        return self.off_low_z is not None

    @property
    def has_vol_select(self):
        return self.off_vol_select is not None

    @property
    def has_monitor_mix(self):
        return self.off_monitor_mix is not None


WAVE_XLR = DeviceProfile(
    key="wave_xlr",
    display_name="Wave XLR",
    vid=0x0FD9,
    pid=0x007D,
    wvalue_config=0x0000,
    wvalue_meter=0x0001,
    wvalue_devinfo=0x000A,
    windex=0x3303,  # 0x3303 not 0x3300 — bypasses snd-usb-audio ownership check
    config_len=34,
    meter_len=10,
    devinfo_len=51,
    devinfo_api=(0, 1),
    devinfo_fw=(6, 7, 8),
    devinfo_serial=(27, 47),
    off_gain=0,
    gain_max=0x5000,
    gain_scale=256,
    off_mute=4,
    off_hp_vol=9,
    hp_fmt='<h',
    hp_scale=256,
    off_vol_select=14,
    vol_select_map={0x02: "hp"},
    off_low_z=33,
    off_monitor_mix=None,
    mix_max=0,
    card_match=("Wave XLR", "Elgato"),
    sync_alsa_mute=True,
    sync_alsa_hp=True,
    sync_alsa_gain=False,
)

WAVE3 = DeviceProfile(
    key="wave3",
    display_name="Wave:3",
    vid=0x0FD9,
    pid=0x0070,
    wvalue_config=0x0000,
    wvalue_meter=0x0001,
    wvalue_devinfo=0x000A,
    windex=0x3303,  # 0x3303 not 0x3300 — bypasses snd-usb-audio ownership check
    config_len=16,
    meter_len=8,
    devinfo_len=64,
    devinfo_api=(0, 1),
    devinfo_fw=(21, 22, 23),
    devinfo_serial=(36, 48),
    off_gain=0,
    gain_max=0x2800,
    gain_scale=256,
    off_mute=4,
    off_hp_vol=7,
    hp_fmt='<h',
    hp_scale=256,
    off_vol_select=12,
    vol_select_map={0x01: "gain", 0x02: "hp", 0x03: "mix"},
    off_low_z=None,
    off_monitor_mix=10,
    mix_max=0x6400,
    card_match=("Wave3", "Elgato"),
    sync_alsa_mute=True,
    sync_alsa_hp=True,
    sync_alsa_gain=True,
)

# The 0fd9:00a6 XLR Dock speaks the original Wave XLR vendor protocol.
# Hardware captures decode the same config and device-info offsets, including
# a serial at bytes 27..46 that matches the ALSA card identity.
WAVE_XLR_MK2 = replace(
    WAVE_XLR,
    key="wave_xlr_mk2",
    display_name="Wave XLR MK.2 (0fd9:00a6)",
    pid=0x00A6,
    card_match=("XLR Dock", "Wave XLR", "Elgato"),
)

PROFILES = (WAVE_XLR, WAVE_XLR_MK2, WAVE3)

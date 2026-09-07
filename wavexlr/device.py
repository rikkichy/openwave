"""Elgato Wave USB device backend.

Uses raw libusb control transfers with wIndex=0x3303 to bypass the Linux
kernel's interface routing. The kernel sees interface 3 (unclaimed) and
lets the transfer through, while the firmware only checks the 0x33 prefix.
No driver detach needed — audio is never interrupted.

Per-model constants (USB IDs, config offsets, capabilities) live in
profiles.py; connect() picks the first supported device found.
"""

import ctypes
import ctypes.util
import struct
import subprocess
import threading

from .profiles import PROFILES

BREQUEST_READ = 0x85
BREQUEST_WRITE = 0x05

RT_CLASS_IN = 0xA1
RT_CLASS_OUT = 0x21
LIBUSB_ERROR_TIMEOUT = -7


class DeviceNotReadyError(RuntimeError):
    """The USB device exists but its ALSA interface is not registered yet."""


class DeviceUnresponsiveError(RuntimeError):
    """The device stopped answering control transfers and needs a power cycle."""



# --- Raw libusb setup ---
_lib_path = ctypes.util.find_library("usb-1.0") or "libusb-1.0.so.0"
_lib = ctypes.CDLL(_lib_path)

_lib.libusb_init.argtypes = [ctypes.POINTER(ctypes.c_void_p)]
_lib.libusb_init.restype = ctypes.c_int
_lib.libusb_open_device_with_vid_pid.argtypes = [ctypes.c_void_p, ctypes.c_uint16, ctypes.c_uint16]
_lib.libusb_open_device_with_vid_pid.restype = ctypes.c_void_p
_lib.libusb_close.argtypes = [ctypes.c_void_p]
_lib.libusb_close.restype = None
_lib.libusb_control_transfer.argtypes = [
    ctypes.c_void_p, ctypes.c_uint8, ctypes.c_uint8,
    ctypes.c_uint16, ctypes.c_uint16,
    ctypes.POINTER(ctypes.c_ubyte), ctypes.c_uint16, ctypes.c_uint,
]
_lib.libusb_control_transfer.restype = ctypes.c_int

_ctx = ctypes.c_void_p()
_lib.libusb_init(ctypes.byref(_ctx))


def _find_card(matches):
    """Find the ALSA card number for the device."""
    try:
        r = subprocess.run(
            ["aplay", "-l"], capture_output=True, text=True, timeout=3
        )
        if r.returncode != 0:
            return None
        for line in r.stdout.splitlines():
            if any(m in line for m in matches):
                return line.split(":")[0].split()[-1]
    except Exception:
        pass
    return None


def _amixer(card, *args):
    """Run amixer, returning None when the control interface did not answer."""
    try:
        r = subprocess.run(
            ["amixer", "-c", card, *args],
            capture_output=True, text=True, timeout=3,
        )
        return r.stdout if r.returncode == 0 else None
    except Exception:
        return None


def _alsa_get(card):
    """Read ALSA mute and HP volume."""
    state = {}
    # Mute (numid=5)
    out = _amixer(card, "cget", "numid=5")
    if out is not None and ": values=" in out:
        state["mute"] = ": values=off" in out
    # HP volume (numid=4) — raw ALSA value 0-120
    out = _amixer(card, "cget", "numid=4")
    if out is not None:
        for line in out.splitlines():
            if ": values=" in line:
                try:
                    state["hp_vol"] = int(line.split("=")[-1])
                except ValueError:
                    pass
    return state


def _alsa_set_mute(card, muted):
    _amixer(card, "cset", "numid=5", "off" if muted else "on")


def _alsa_set_hp_vol(card, value):
    """Set ALSA HP volume (numid=4, 0-120)."""
    _amixer(card, "cset", "numid=4", str(max(0, min(120, value))))


def _alsa_set_gain(card, value):
    """Set ALSA mic gain (numid=6, 0-80)."""
    _amixer(card, "cset", "numid=6", str(max(0, min(80, value))))


def _fw_gain_to_alsa(fw_gain_raw, scale):
    """Map firmware gain (raw / scale dB) to ALSA (0-80, 0.5 dB steps)."""
    db = fw_gain_raw / scale
    return max(0, min(80, round(db / 0.5)))


def _fw_hp_to_alsa(fw_hp_raw, scale):
    """Map firmware HP to ALSA (0-120).

    Firmware: raw / scale dB (XLR: int16 Q8.8, Wave:3: int8 whole-dB).
    ALSA driver caps lower at 0 → -60 dB; anything below saturates.
    ALSA step = 0.5 dB, so dB = (value - 120) * 0.5 → value = dB / 0.5 + 120.
    """
    db = fw_hp_raw / scale
    return max(0, min(120, round(db / 0.5 + 120)))


def _alsa_hp_to_fw(alsa_hp, scale):
    """Map ALSA HP (0-120) to firmware HP raw."""
    db = (alsa_hp - 120) * 0.5  # 0→-60, 120→0
    db = max(-128.0, min(0.0, db))  # firmware range
    return int(db * scale)


class WaveDevice:
    def __init__(self):
        self._handle = None
        self._lock = threading.Lock()
        self._card = None
        self._last_fw = None  # last known firmware state for change detection
        self.profile = None

    @property
    def connected(self):
        return self._handle is not None

    def connect(self):
        for profile in PROFILES:
            handle = _lib.libusb_open_device_with_vid_pid(
                _ctx, profile.vid, profile.pid
            )
            if not handle:
                continue

            # snd-usb-audio and OpenWave share endpoint 0. Do not start vendor
            # transfers while ALSA is still probing the device at login.
            card = _find_card(profile.card_match)
            if card is None:
                _lib.libusb_close(handle)
                raise DeviceNotReadyError(
                    f"{profile.display_name} audio interface is not ready"
                )
            if _amixer(card, "cget", "numid=5") is None:
                _lib.libusb_close(handle)
                raise DeviceUnresponsiveError(
                    f"{profile.display_name} control interface is not responding"
                )

            self._handle = handle
            self.profile = profile
            self._card = card
            return
        raise DeviceNotReadyError("No supported Elgato Wave device found")

    def disconnect(self):
        if self._handle:
            _lib.libusb_close(self._handle)
            self._handle = None
        self._card = None
        self._last_fw = None

    def _ctrl_read(self, wValue, length):
        """USB control read — no detach needed."""
        buf = (ctypes.c_ubyte * length)()
        with self._lock:
            ret = _lib.libusb_control_transfer(
                self._handle, RT_CLASS_IN, BREQUEST_READ, wValue, self.profile.windex,
                buf, length, 1000,
            )
        if ret < 0:
            error = f"USB read failed (err {ret})"
            if ret == LIBUSB_ERROR_TIMEOUT:
                raise DeviceUnresponsiveError(error)
            raise RuntimeError(error)
        return bytearray(buf[:ret])

    def _ctrl_write(self, wValue, data):
        """USB control write — no detach needed."""
        data = bytes(data)
        buf = (ctypes.c_ubyte * len(data))(*data)
        with self._lock:
            ret = _lib.libusb_control_transfer(
                self._handle, RT_CLASS_OUT, BREQUEST_WRITE, wValue, self.profile.windex,
                buf, len(data), 1000,
            )
        if ret < 0:
            error = f"USB write failed (err {ret})"
            if ret == LIBUSB_ERROR_TIMEOUT:
                raise DeviceUnresponsiveError(error)
            raise RuntimeError(error)

    def read_config(self):
        return self._ctrl_read(self.profile.wvalue_config, self.profile.config_len)

    def write_config(self, config):
        self._ctrl_write(self.profile.wvalue_config, config)

    def read_meters(self):
        data = self._ctrl_read(self.profile.wvalue_meter, self.profile.meter_len)
        left = struct.unpack_from('<I', data, 0)[0]
        right = struct.unpack_from('<I', data, 4)[0]
        return left, right

    def read_device_info(self):
        """Read and parse the device info block."""
        p = self.profile
        data = self._ctrl_read(p.wvalue_devinfo, p.devinfo_len)
        serial = bytes(data[p.devinfo_serial[0]:p.devinfo_serial[1]]).decode(
            'ascii', errors='replace').rstrip('\x00')
        return {
            "api_version": f"{data[p.devinfo_api[0]]}.{data[p.devinfo_api[1]]}",
            "fw_version": f"{data[p.devinfo_fw[0]]}.{data[p.devinfo_fw[1]]}.{data[p.devinfo_fw[2]]}",
            "serial": serial,
        }

    # --- High-level getters ---

    def get_gain_raw(self):
        return struct.unpack_from('<H', self.read_config(), self.profile.off_gain)[0]

    def get_mute(self):
        return bool(self.read_config()[self.profile.off_mute])

    def get_hp_volume_db(self):
        p = self.profile
        raw = struct.unpack_from(p.hp_fmt, self.read_config(), p.off_hp_vol)[0]
        return raw / p.hp_scale

    def get_low_impedance(self):
        if self.profile.off_low_z is None:
            return None
        return bool(self.read_config()[self.profile.off_low_z])

    def get_volume_select(self):
        if self.profile.off_vol_select is None:
            return None
        val = self.read_config()[self.profile.off_vol_select]
        return self.profile.vol_select_map.get(val, "gain")

    def get_monitor_mix(self):
        if self.profile.off_monitor_mix is None:
            return None
        return struct.unpack_from('<H', self.read_config(), self.profile.off_monitor_mix)[0]

    def get_all(self):
        p = self.profile
        config = self.read_config()
        fw_gain = struct.unpack_from('<H', config, p.off_gain)[0]
        fw_hp = struct.unpack_from(p.hp_fmt, config, p.off_hp_vol)[0]
        fw_mute = bool(config[p.off_mute])

        fw_now = {"mute": fw_mute, "gain": fw_gain, "hp": fw_hp}

        # Sync firmware ↔ ALSA
        if self._card:
            alsa = _alsa_get(self._card)
            dirty = False  # whether we need to write config back

            if self._last_fw is not None:
                # --- Mute ---
                if p.sync_alsa_mute:
                    if fw_mute != self._last_fw["mute"]:
                        _alsa_set_mute(self._card, fw_mute)
                    elif alsa.get("mute") is not None and alsa["mute"] != fw_mute:
                        config[p.off_mute] = 0x01 if alsa["mute"] else 0x00
                        fw_mute = alsa["mute"]
                        dirty = True

                # --- HP volume ---
                if p.sync_alsa_hp:
                    if fw_hp != self._last_fw["hp"]:
                        _alsa_set_hp_vol(self._card, _fw_hp_to_alsa(fw_hp, p.hp_scale))
                    elif "hp_vol" in alsa and alsa["hp_vol"] != _fw_hp_to_alsa(self._last_fw["hp"], p.hp_scale):
                        fw_hp = _alsa_hp_to_fw(alsa["hp_vol"], p.hp_scale)
                        struct.pack_into(p.hp_fmt, config, p.off_hp_vol, fw_hp)
                        dirty = True

                # --- Gain (push only: ALSA writes mirror back into firmware) ---
                if p.sync_alsa_gain:
                    if fw_gain != self._last_fw["gain"]:
                        _alsa_set_gain(self._card, _fw_gain_to_alsa(fw_gain, p.gain_scale))

            else:
                # First poll — sync firmware state to ALSA
                if p.sync_alsa_mute:
                    _alsa_set_mute(self._card, fw_mute)
                if p.sync_alsa_hp:
                    _alsa_set_hp_vol(self._card, _fw_hp_to_alsa(fw_hp, p.hp_scale))
                if p.sync_alsa_gain:
                    _alsa_set_gain(self._card, _fw_gain_to_alsa(fw_gain, p.gain_scale))

            if dirty:
                self.write_config(config)

            self._last_fw = {"mute": fw_mute, "gain": fw_gain, "hp": fw_hp}
        else:
            self._last_fw = fw_now

        state = {
            "gain_raw": fw_gain,
            "mute": fw_mute,
            "hp_volume_db": fw_hp / p.hp_scale,
        }
        if p.off_vol_select is not None:
            state["volume_select"] = p.vol_select_map.get(config[p.off_vol_select], "gain")
        if p.off_low_z is not None:
            state["low_impedance"] = bool(config[p.off_low_z])
        if p.off_monitor_mix is not None:
            state["monitor_mix"] = struct.unpack_from('<H', config, p.off_monitor_mix)[0]
        return state

    # --- High-level setters (read-modify-write) ---

    def set_gain_raw(self, value):
        value = max(0, min(0xFFFF, value))
        config = self.read_config()
        struct.pack_into('<H', config, self.profile.off_gain, value)
        self.write_config(config)
        if self._last_fw:
            self._last_fw["gain"] = value
        if self._card and self.profile.sync_alsa_gain:
            _alsa_set_gain(self._card, _fw_gain_to_alsa(value, self.profile.gain_scale))

    def set_mute(self, muted):
        config = self.read_config()
        config[self.profile.off_mute] = 0x01 if muted else 0x00
        self.write_config(config)
        if self._last_fw:
            self._last_fw["mute"] = muted
        if self._card and self.profile.sync_alsa_mute:
            _alsa_set_mute(self._card, muted)

    def set_hp_volume_db(self, db):
        p = self.profile
        db = max(-128.0, min(0.0, db))
        raw = int(db * p.hp_scale)
        config = self.read_config()
        struct.pack_into(p.hp_fmt, config, p.off_hp_vol, raw)
        self.write_config(config)
        if self._last_fw:
            self._last_fw["hp"] = raw
        if self._card and p.sync_alsa_hp:
            _alsa_set_hp_vol(self._card, _fw_hp_to_alsa(raw, p.hp_scale))

    def set_low_impedance(self, enabled):
        if self.profile.off_low_z is None:
            return
        config = self.read_config()
        config[self.profile.off_low_z] = 0x01 if enabled else 0x00
        self.write_config(config)

    def set_monitor_mix(self, value):
        p = self.profile
        if p.off_monitor_mix is None:
            return
        value = max(0, min(p.mix_max, int(value)))
        config = self.read_config()
        struct.pack_into('<H', config, p.off_monitor_mix, value)
        self.write_config(config)


WaveXLR = WaveDevice

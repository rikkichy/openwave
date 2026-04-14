"""Wave XLR USB device backend.

Uses raw libusb control transfers with wIndex=0x3303 to bypass the Linux
kernel's interface routing. The kernel sees interface 3 (unclaimed) and
lets the transfer through, while the firmware only checks the 0x33 prefix.
No driver detach needed — audio is never interrupted.
"""

import ctypes
import ctypes.util
import struct
import subprocess
import threading

VENDOR_ID = 0x0FD9
PRODUCT_ID = 0x007D

BREQUEST_READ = 0x85
BREQUEST_WRITE = 0x05
WVALUE_CONFIG = 0x0000
WVALUE_METER = 0x0001
WVALUE_DEVINFO = 0x000A
WINDEX = 0x3303  # 0x3303 not 0x3300 — bypasses snd-usb-audio ownership check
CONFIG_LEN = 34
METER_LEN = 10

RT_CLASS_IN = 0xA1
RT_CLASS_OUT = 0x21

OFF_GAIN = 0
OFF_MUTE = 4
OFF_HP_VOL = 9
OFF_VOL_SELECT = 14
OFF_LOW_Z = 33

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


def _find_card():
    """Find the ALSA card number for the Wave XLR."""
    try:
        r = subprocess.run(["aplay", "-l"], capture_output=True, text=True, timeout=3)
        for line in r.stdout.splitlines():
            if "Wave XLR" in line or "Elgato" in line:
                return line.split(":")[0].split()[-1]
    except Exception:
        pass
    return None


def _amixer(card, *args):
    """Run amixer and return stdout."""
    try:
        r = subprocess.run(
            ["amixer", "-c", card, *args],
            capture_output=True, text=True, timeout=3,
        )
        return r.stdout
    except Exception:
        return ""


def _alsa_get(card):
    """Read ALSA mute and HP volume."""
    state = {}
    # Mute (numid=5)
    out = _amixer(card, "cget", "numid=5")
    state["mute"] = ": values=off" in out
    # HP volume (numid=4) — raw ALSA value 0-120
    out = _amixer(card, "cget", "numid=4")
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


def _fw_hp_to_alsa(fw_hp_raw):
    """Map firmware HP (int16 Q8.8, -7808 to 0) to ALSA (0-120)."""
    # Firmware: -7808 (-30.5dB) to 0 (0dB), ALSA: 0 (-60dB) to 120 (0dB)
    # ALSA step = 0.5 dB, so dB = (value - 120) * 0.5 → value = dB / 0.5 + 120
    db = fw_hp_raw / 256.0
    return max(0, min(120, round(db / 0.5 + 120)))


def _alsa_hp_to_fw(alsa_hp):
    """Map ALSA HP (0-120) to firmware HP (int16 Q8.8)."""
    db = (alsa_hp - 120) * 0.5  # 0→-60, 120→0
    db = max(-30.5, min(0.0, db))  # clamp to firmware range
    return int(db * 256)


class WaveXLR:
    def __init__(self):
        self._handle = None
        self._lock = threading.Lock()
        self._card = None
        self._last_fw = None  # last known firmware state for change detection

    @property
    def connected(self):
        return self._handle is not None

    def connect(self):
        handle = _lib.libusb_open_device_with_vid_pid(_ctx, VENDOR_ID, PRODUCT_ID)
        if not handle:
            raise RuntimeError("Wave XLR not found")
        self._handle = handle
        self._card = _find_card()

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
                self._handle, RT_CLASS_IN, BREQUEST_READ, wValue, WINDEX,
                buf, length, 1000,
            )
        if ret < 0:
            raise RuntimeError(f"USB read failed (err {ret})")
        return bytearray(buf[:ret])

    def _ctrl_write(self, wValue, data):
        """USB control write — no detach needed."""
        data = bytes(data)
        buf = (ctypes.c_ubyte * len(data))(*data)
        with self._lock:
            ret = _lib.libusb_control_transfer(
                self._handle, RT_CLASS_OUT, BREQUEST_WRITE, wValue, WINDEX,
                buf, len(data), 1000,
            )
        if ret < 0:
            raise RuntimeError(f"USB write failed (err {ret})")

    def read_config(self):
        return self._ctrl_read(WVALUE_CONFIG, CONFIG_LEN)

    def write_config(self, config):
        self._ctrl_write(WVALUE_CONFIG, config)

    def read_meters(self):
        data = self._ctrl_read(WVALUE_METER, METER_LEN)
        left = struct.unpack_from('<I', data, 0)[0]
        right = struct.unpack_from('<I', data, 4)[0]
        return left, right

    def read_device_info(self):
        """Read and parse the 51-byte device info block."""
        data = self._ctrl_read(WVALUE_DEVINFO, 51)
        serial = bytes(data[27:47]).decode('ascii', errors='replace').rstrip('\x00')
        return {
            "api_version": f"{data[0]}.{data[1]}",
            "fw_version": f"{data[6]}.{data[7]}.{data[8]}",
            "serial": serial,
        }

    # --- High-level getters ---

    def get_gain_raw(self):
        return struct.unpack_from('<H', self.read_config(), OFF_GAIN)[0]

    def get_mute(self):
        return bool(self.read_config()[OFF_MUTE])

    def get_hp_volume_db(self):
        raw = struct.unpack_from('<h', self.read_config(), OFF_HP_VOL)[0]
        return raw / 256.0

    def get_low_impedance(self):
        return bool(self.read_config()[OFF_LOW_Z])

    def get_volume_select(self):
        val = self.read_config()[OFF_VOL_SELECT]
        return "hp" if val == 2 else "gain"

    def get_all(self):
        config = self.read_config()
        fw_gain = struct.unpack_from('<H', config, OFF_GAIN)[0]
        fw_hp = struct.unpack_from('<h', config, OFF_HP_VOL)[0]
        fw_mute = bool(config[OFF_MUTE])

        fw_now = {"mute": fw_mute, "gain": fw_gain, "hp": fw_hp}

        # Sync firmware ↔ ALSA
        if self._card:
            alsa = _alsa_get(self._card)
            dirty = False  # whether we need to write config back

            if self._last_fw is not None:
                # --- Mute ---
                if fw_mute != self._last_fw["mute"]:
                    _alsa_set_mute(self._card, fw_mute)
                elif alsa.get("mute") is not None and alsa["mute"] != fw_mute:
                    config[OFF_MUTE] = 0x01 if alsa["mute"] else 0x00
                    fw_mute = alsa["mute"]
                    dirty = True

                # --- HP volume ---
                if fw_hp != self._last_fw["hp"]:
                    _alsa_set_hp_vol(self._card, _fw_hp_to_alsa(fw_hp))
                elif "hp_vol" in alsa and alsa["hp_vol"] != _fw_hp_to_alsa(self._last_fw["hp"]):
                    fw_hp = _alsa_hp_to_fw(alsa["hp_vol"])
                    struct.pack_into('<h', config, OFF_HP_VOL, fw_hp)
                    dirty = True

            else:
                # First poll — sync firmware state to ALSA
                _alsa_set_mute(self._card, fw_mute)
                _alsa_set_hp_vol(self._card, _fw_hp_to_alsa(fw_hp))

            if dirty:
                self.write_config(config)

            self._last_fw = {"mute": fw_mute, "gain": fw_gain, "hp": fw_hp}
        else:
            self._last_fw = fw_now

        return {
            "gain_raw": fw_gain,
            "mute": fw_mute,
            "hp_volume_db": fw_hp / 256.0,
            "volume_select": "hp" if config[OFF_VOL_SELECT] == 2 else "gain",
            "low_impedance": bool(config[OFF_LOW_Z]),
        }

    # --- High-level setters (read-modify-write) ---

    def set_gain_raw(self, value):
        value = max(0, min(0xFFFF, value))
        config = self.read_config()
        struct.pack_into('<H', config, OFF_GAIN, value)
        self.write_config(config)
        if self._last_fw:
            self._last_fw["gain"] = value

    def set_mute(self, muted):
        config = self.read_config()
        config[OFF_MUTE] = 0x01 if muted else 0x00
        self.write_config(config)
        if self._last_fw:
            self._last_fw["mute"] = muted
        if self._card:
            _alsa_set_mute(self._card, muted)

    def set_hp_volume_db(self, db):
        db = max(-30.5, min(0.0, db))
        raw = int(db * 256)
        config = self.read_config()
        struct.pack_into('<h', config, OFF_HP_VOL, raw)
        self.write_config(config)
        if self._last_fw:
            self._last_fw["hp"] = raw
        if self._card:
            _alsa_set_hp_vol(self._card, _fw_hp_to_alsa(raw))

    def set_low_impedance(self, enabled):
        config = self.read_config()
        config[OFF_LOW_Z] = 0x01 if enabled else 0x00
        self.write_config(config)

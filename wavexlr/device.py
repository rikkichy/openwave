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
import glob
import os
import re
import struct
import subprocess
import threading
import time

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
_lib.libusb_close.argtypes = [ctypes.c_void_p]
_lib.libusb_close.restype = None
_lib.libusb_control_transfer.argtypes = [
    ctypes.c_void_p, ctypes.c_uint8, ctypes.c_uint8,
    ctypes.c_uint16, ctypes.c_uint16,
    ctypes.POINTER(ctypes.c_ubyte), ctypes.c_uint16, ctypes.c_uint,
]
_lib.libusb_control_transfer.restype = ctypes.c_int
_lib.libusb_get_device_list.argtypes = [
    ctypes.c_void_p, ctypes.POINTER(ctypes.POINTER(ctypes.c_void_p))
]
_lib.libusb_get_device_list.restype = ctypes.c_ssize_t
_lib.libusb_free_device_list.argtypes = [ctypes.POINTER(ctypes.c_void_p), ctypes.c_int]
_lib.libusb_free_device_list.restype = None
_lib.libusb_get_bus_number.argtypes = [ctypes.c_void_p]
_lib.libusb_get_bus_number.restype = ctypes.c_uint8
_lib.libusb_get_device_address.argtypes = [ctypes.c_void_p]
_lib.libusb_get_device_address.restype = ctypes.c_uint8
_lib.libusb_open.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_void_p)]
_lib.libusb_open.restype = ctypes.c_int


class _DeviceDescriptor(ctypes.Structure):
    _fields_ = [
        ("bLength", ctypes.c_uint8),
        ("bDescriptorType", ctypes.c_uint8),
        ("bcdUSB", ctypes.c_uint16),
        ("bDeviceClass", ctypes.c_uint8),
        ("bDeviceSubClass", ctypes.c_uint8),
        ("bDeviceProtocol", ctypes.c_uint8),
        ("bMaxPacketSize0", ctypes.c_uint8),
        ("idVendor", ctypes.c_uint16),
        ("idProduct", ctypes.c_uint16),
        ("bcdDevice", ctypes.c_uint16),
        ("iManufacturer", ctypes.c_uint8),
        ("iProduct", ctypes.c_uint8),
        ("iSerialNumber", ctypes.c_uint8),
        ("bNumConfigurations", ctypes.c_uint8),
    ]


_lib.libusb_get_device_descriptor.argtypes = [
    ctypes.c_void_p, ctypes.POINTER(_DeviceDescriptor)
]
_lib.libusb_get_device_descriptor.restype = ctypes.c_int

_ctx = ctypes.c_void_p()
_lib.libusb_init(ctypes.byref(_ctx))


def _each_usb_device(visit):
    """Visit devices while their enumeration references remain valid."""
    devices = ctypes.POINTER(ctypes.c_void_p)()
    count = _lib.libusb_get_device_list(_ctx, ctypes.byref(devices))
    if count < 0:
        raise RuntimeError(f"USB enumeration failed (err {count})")
    try:
        descriptor = _DeviceDescriptor()
        for index in range(count):
            dev = devices[index]
            if _lib.libusb_get_device_descriptor(dev, ctypes.byref(descriptor)):
                continue
            visit(
                descriptor.idVendor, descriptor.idProduct,
                _lib.libusb_get_bus_number(dev),
                _lib.libusb_get_device_address(dev), dev,
            )
    finally:
        _lib.libusb_free_device_list(devices, 1)


def scan():
    """Return every supported (profile, bus, address), including duplicates."""
    profiles = {(profile.vid, profile.pid): profile for profile in PROFILES}
    found = []

    def visit(vid, pid, bus, addr, _dev):
        profile = profiles.get((vid, pid))
        if profile is not None:
            found.append((profile, bus, addr))

    _each_usb_device(visit)
    return sorted(found, key=lambda entry: (entry[1], entry[2]))


def _find_card(matches, *, vid=None, pid=None, usbbus=None):
    """Find the ALSA card for one USB device without guessing its identity."""
    paths = sorted(glob.glob("/proc/asound/card*/usbid"))
    if vid is not None and pid is not None and paths:
        wanted_id = f"{vid:04x}:{pid:04x}"
        readable = False
        for path in paths:
            try:
                with open(path) as f:
                    actual_id = f.read().strip().lower()
            except OSError:
                continue
            readable = True
            if actual_id != wanted_id:
                continue
            if usbbus is not None:
                try:
                    with open(os.path.join(os.path.dirname(path), "usbbus")) as f:
                        actual_bus = f.read().strip()
                except OSError:
                    continue
                if actual_bus != usbbus:
                    continue
            card_dir = os.path.basename(os.path.dirname(path))
            if card_dir.startswith("card") and card_dir[4:].isdigit():
                return card_dir[4:]
        if readable:
            return None
    if usbbus is not None:
        return None

    # Older kernels may not expose /proc/asound/card*/usbid. Only that case
    # falls back to product-name matching, which cannot distinguish duplicates.
    try:
        result = subprocess.run(
            ["aplay", "-l"], capture_output=True, text=True, timeout=3
        )
        if result.returncode != 0:
            return None
        for line in result.stdout.splitlines():
            if any(match in line for match in matches):
                return line.split(":")[0].split()[-1]
    except Exception:
        pass
    return None


def _amixer(card, *args):
    """Run amixer, returning None when the control interface did not answer."""
    try:
        result = subprocess.run(
            ["amixer", "-c", card, *args],
            capture_output=True, text=True, timeout=3,
        )
        return result.stdout if result.returncode == 0 else None
    except Exception:
        return None


_ALSA_ROLE_SUFFIX = {
    "Capture Switch": "mute",
    "Capture Volume": "gain",
    "Playback Volume": "hp_vol",
}
_ALSA_ROLE_FALLBACK = {"mute": 5, "gain": 6, "hp_vol": 4}
_ALSA_NUMIDS = {}
_ALSA_CTL_MAX = {}


def _discover_numids(card):
    """Discover control roles and ranges from one `amixer contents` pass."""
    if card in _ALSA_NUMIDS:
        return _ALSA_NUMIDS[card]
    found = {}
    current_id = None
    current_name = None
    for line in (_amixer(card, "contents") or "").splitlines():
        stripped = line.strip()
        match = re.match(r"numid=(\d+),iface=(\w+),name='(.*)'", stripped)
        if match:
            current_id = int(match.group(1))
            current_name = match.group(3)
            if match.group(2) != "MIXER":
                current_id = current_name = None
            continue
        if current_id is None or not stripped.startswith("; type="):
            continue
        role = next(
            (
                role
                for suffix, role in _ALSA_ROLE_SUFFIX.items()
                if current_name.endswith(suffix)
            ),
            None,
        )
        if role and role not in found:
            found[role] = current_id
            maximum = re.search(r",max=(-?\d+)", stripped)
            if maximum:
                _ALSA_CTL_MAX[(card, current_id)] = int(maximum.group(1))
    _ALSA_NUMIDS[card] = found
    return found


def _numid(card, role):
    return _discover_numids(card).get(role, _ALSA_ROLE_FALLBACK[role])


def _alsa_ctl_max(card, numid, fallback):
    """Return the driver-reported maximum for one ALSA control."""
    key = (card, numid)
    if key not in _ALSA_CTL_MAX:
        output = _amixer(card, "cget", f"numid={numid}") or ""
        maximum = re.search(r",max=(-?\d+)", output)
        _ALSA_CTL_MAX[key] = int(maximum.group(1)) if maximum else fallback
    return _ALSA_CTL_MAX[key]


def _forget_alsa_card(card):
    if card is None:
        return
    _ALSA_NUMIDS.pop(card, None)
    for key in list(_ALSA_CTL_MAX):
        if key[0] == card:
            _ALSA_CTL_MAX.pop(key, None)


def _alsa_get(card):
    """Read ALSA mute and headphone volume without inventing failed values."""
    state = {}
    output = _amixer(card, "cget", f"numid={_numid(card, 'mute')}")
    if output is not None and ": values=" in output:
        state["mute"] = ": values=off" in output
    output = _amixer(card, "cget", f"numid={_numid(card, 'hp_vol')}")
    if output is not None:
        for line in output.splitlines():
            if ": values=" not in line:
                continue
            try:
                state["hp_vol"] = int(line.split("=")[-1])
            except ValueError:
                pass
    return state


def _alsa_set_mute(card, muted):
    _amixer(
        card, "cset", f"numid={_numid(card, 'mute')}",
        "off" if muted else "on",
    )


def _alsa_set_hp_vol(card, value):
    """Set ALSA headphone volume within the control's reported range."""
    numid = _numid(card, "hp_vol")
    maximum = _alsa_ctl_max(card, numid, 120)
    _amixer(card, "cset", f"numid={numid}", str(max(0, min(maximum, value))))


def _alsa_set_gain(card, value):
    """Set ALSA microphone gain within the control's reported range."""
    numid = _numid(card, "gain")
    maximum = _alsa_ctl_max(card, numid, 150)
    _amixer(card, "cset", f"numid={numid}", str(max(0, min(maximum, value))))


def _fw_gain_to_alsa(fw_gain_raw, scale):
    """Map firmware dB to the ALSA control's half-dB steps."""
    return max(0, round((fw_gain_raw / scale) / 0.5))


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


# PipeWire's ALSA device.serial is udev ID_SERIAL: manufacturer_product_serial.
# These are the supported units' udev prefixes, not node-name substring matches.
_CAPTURE_SERIAL_PREFIXES = {
    "wave_xlr": "Elgato_Systems_Elgato_Wave_XLR_",
    "wave_xlr_mk2": "Elgato_Systems_Elgato_XLR_Dock_",
    "wave3": "Elgato_Systems_Elgato_Wave_3_",
}


def device_for_capture(capture, devices):
    """Map a capture only to one connected unit with an exact serial identity.

    ALSA card indices are reusable and cannot establish physical identity.
    Accept a bare firmware serial or its model's complete udev ID_SERIAL;
    unknown formats and missing or duplicate identities use PipeWire instead.
    """
    serial = capture.get("serial")
    if not isinstance(serial, str) or not serial:
        return None
    match = None
    for dev in devices:
        unit_serial = dev.info.get("serial")
        if not dev.connected or not isinstance(unit_serial, str) or not unit_serial:
            continue
        prefix = _CAPTURE_SERIAL_PREFIXES.get(dev.profile.key)
        if serial != unit_serial and (prefix is None or serial != prefix + unit_serial):
            continue
        if match is not None:
            return None
        match = dev
    return match


class WaveDevice:
    def __init__(self):
        self._handle = None
        self._lock = threading.RLock()
        self._card = None
        self._last_fw = None  # last known firmware state for change detection
        self.profile = None
        self.usbbus = None
        self.info = {}
        self._last_alsa_at = 0.0

    @property
    def connected(self):
        return self._handle is not None

    @property
    def alsa_card(self):
        """Exact ALSA card number paired with this USB handle."""
        return self._card

    def connect(self, profile=None, bus=None, addr=None):
        """Open the exact scanned unit, only after its ALSA controls are ready."""
        if profile is None:
            found = scan()
            if not found:
                raise DeviceNotReadyError("No supported Elgato Wave device found")
            profile, bus, addr = found[0]
        if bus is None or addr is None:
            raise ValueError("Opening a device requires both USB bus and address")
        usbbus = f"{bus:03d}/{addr:03d}"
        with self._lock:
            if self._handle is not None:
                raise RuntimeError("Device is already connected")
            handle = self._open_at(profile, bus, addr)
            if handle is None:
                raise DeviceNotReadyError(
                    f"Cannot open {profile.display_name} at {usbbus}"
                )
            card = _find_card(
                profile.card_match, vid=profile.vid, pid=profile.pid,
                usbbus=usbbus,
            )
            if card is None:
                _lib.libusb_close(handle)
                raise DeviceNotReadyError(
                    f"{profile.display_name} audio interface is not ready"
                )
            _forget_alsa_card(card)
            mute_numid = _numid(card, "mute")
            if _amixer(card, "cget", f"numid={mute_numid}") is None:
                _forget_alsa_card(card)
                _lib.libusb_close(handle)
                raise DeviceUnresponsiveError(
                    f"{profile.display_name} control interface is not responding"
                )
            self._handle = handle
            self.profile = profile
            self._card = card
            self.usbbus = usbbus

    @staticmethod
    def _open_at(profile, bus, addr):
        handle = ctypes.c_void_p()

        def visit(vid, pid, device_bus, device_addr, dev):
            if handle.value or (vid, pid, device_bus, device_addr) != (
                profile.vid, profile.pid, bus, addr
            ):
                return
            _lib.libusb_open(dev, ctypes.byref(handle))

        _each_usb_device(visit)
        return handle.value

    def disconnect(self):
        card = self._card
        with self._lock:
            if self._handle:
                _lib.libusb_close(self._handle)
                self._handle = None
            self._card = None
            self._last_fw = None
            self.usbbus = None
            self.info = {}
        _forget_alsa_card(card)

    def _ctrl_read(self, wValue, length):
        """USB control read — no detach needed."""
        buf = (ctypes.c_ubyte * length)()
        with self._lock:
            if self._handle is None:
                raise RuntimeError("Device disconnected")
            ret = _lib.libusb_control_transfer(
                self._handle, RT_CLASS_IN, BREQUEST_READ, wValue, self.profile.windex,
                buf, length, 1000,
            )
        if ret < 0:
            error = f"USB read failed (err {ret})"
            if ret == LIBUSB_ERROR_TIMEOUT:
                raise DeviceUnresponsiveError(error)
            raise RuntimeError(error)
        if ret != length:
            raise RuntimeError(f"Incomplete USB read ({ret}/{length} bytes)")
        return bytearray(buf[:ret])

    def _ctrl_write(self, wValue, data):
        """USB control write — no detach needed."""
        data = bytes(data)
        buf = (ctypes.c_ubyte * len(data))(*data)
        with self._lock:
            if self._handle is None:
                raise RuntimeError("Device disconnected")
            ret = _lib.libusb_control_transfer(
                self._handle, RT_CLASS_OUT, BREQUEST_WRITE, wValue, self.profile.windex,
                buf, len(data), 1000,
            )
        if ret < 0:
            error = f"USB write failed (err {ret})"
            if ret == LIBUSB_ERROR_TIMEOUT:
                raise DeviceUnresponsiveError(error)
            raise RuntimeError(error)
        if ret != len(data):
            raise RuntimeError(f"Incomplete USB write ({ret}/{len(data)} bytes)")

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

    def get_phantom(self):
        """Return phantom-power state, or None without a powered XLR input."""
        if self.profile.off_phantom is None:
            return None
        return bool(self.read_config()[self.profile.off_phantom])


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
        """Read and synchronize one complete firmware transaction."""
        with self._lock:
            return self._get_all_locked()

    def _get_all_locked(self):
        p = self.profile
        config = self.read_config()
        fw_gain = struct.unpack_from('<H', config, p.off_gain)[0]
        fw_hp = struct.unpack_from(p.hp_fmt, config, p.off_hp_vol)[0]
        fw_mute = bool(config[p.off_mute])

        fw_now = {"mute": fw_mute, "gain": fw_gain, "hp": fw_hp}

        # Sync firmware ↔ ALSA
        if self._card:
            now = time.monotonic()
            alsa = {}
            if now - self._last_alsa_at >= 0.5:
                alsa = _alsa_get(self._card)
                self._last_alsa_at = now
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
        if p.off_phantom is not None:
            state["phantom"] = bool(config[p.off_phantom])
        if p.off_monitor_mix is not None:
            state["monitor_mix"] = struct.unpack_from('<H', config, p.off_monitor_mix)[0]
        return state

    # --- High-level setters (read-modify-write) ---
    def _write_config_byte(self, offset, value):
        """Atomically update one byte in the shared firmware config block."""
        with self._lock:
            config = self.read_config()
            config[offset] = value
            self.write_config(config)

    def _write_config_value(self, fmt, offset, value):
        """Atomically update one packed value in the firmware config block."""
        with self._lock:
            config = self.read_config()
            struct.pack_into(fmt, config, offset, value)
            self.write_config(config)


    def set_gain_raw(self, value):
        value = max(0, min(0xFFFF, value))
        self._write_config_value('<H', self.profile.off_gain, value)
        if self._last_fw:
            self._last_fw["gain"] = value
        if self._card and self.profile.sync_alsa_gain:
            _alsa_set_gain(self._card, _fw_gain_to_alsa(value, self.profile.gain_scale))

    def set_mute(self, muted):
        self._write_config_byte(self.profile.off_mute, 0x01 if muted else 0x00)
        if self._last_fw:
            self._last_fw["mute"] = muted
        if self._card and self.profile.sync_alsa_mute:
            _alsa_set_mute(self._card, muted)

    def set_hp_volume_db(self, db):
        p = self.profile
        db = max(-128.0, min(0.0, db))
        raw = int(db * p.hp_scale)
        self._write_config_value(p.hp_fmt, p.off_hp_vol, raw)
        if self._last_fw:
            self._last_fw["hp"] = raw
        if self._card and p.sync_alsa_hp:
            _alsa_set_hp_vol(self._card, _fw_hp_to_alsa(raw, p.hp_scale))

    def set_phantom(self, enabled):
        """Switch 48 V phantom power on profiles that expose the control."""
        if self.profile.off_phantom is None:
            return
        self._write_config_byte(
            self.profile.off_phantom, 0x01 if enabled else 0x00
        )


    def set_low_impedance(self, enabled):
        if self.profile.off_low_z is None:
            return
        self._write_config_byte(
            self.profile.off_low_z, 0x01 if enabled else 0x00
        )

    def set_monitor_mix(self, value):
        p = self.profile
        if p.off_monitor_mix is None:
            return
        value = max(0, min(p.mix_max, int(value)))
        self._write_config_value('<H', p.off_monitor_mix, value)



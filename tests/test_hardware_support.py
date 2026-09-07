import threading
import unittest

from wavexlr.device import WaveDevice
from wavexlr.mixer import _is_wave_card
from wavexlr.profiles import PROFILES, WAVE3, WAVE_XLR, WAVE_XLR_MK2
from wavexlr.setup import UDEV_RULES


class DockProfileTests(unittest.TestCase):
    def test_00a6_dock_uses_verified_xlr_protocol(self):
        self.assertEqual((WAVE_XLR_MK2.vid, WAVE_XLR_MK2.pid), (0x0FD9, 0x00A6))
        self.assertEqual(WAVE_XLR_MK2.config_len, WAVE_XLR.config_len)
        self.assertEqual(WAVE_XLR_MK2.off_gain, WAVE_XLR.off_gain)
        self.assertEqual(WAVE_XLR_MK2.off_mute, WAVE_XLR.off_mute)
        self.assertEqual(WAVE_XLR_MK2.off_hp_vol, WAVE_XLR.off_hp_vol)
        self.assertEqual(WAVE_XLR_MK2.off_low_z, WAVE_XLR.off_low_z)

    def test_every_profile_has_one_udev_rule(self):
        for profile in PROFILES:
            vendor = f'ATTR{{idVendor}}=="{profile.vid:04x}"'
            product = f'ATTR{{idProduct}}=="{profile.pid:04x}"'
            matching = [rule for rule in UDEV_RULES if vendor in rule and product in rule]
            self.assertEqual(len(matching), 1, profile.display_name)

    def test_pipewire_names_recognize_both_xlr_families(self):
        self.assertTrue(_is_wave_card(
            "alsa_input.usb-Elgato_Systems_Elgato_XLR_Dock_A1-00.mono-fallback"
        ))
        self.assertTrue(_is_wave_card(
            "alsa_output.usb-Elgato_Systems_Elgato_Wave_XLR_A1-00.analog-stereo"
        ))
        self.assertFalse(_is_wave_card(
            "alsa_input.usb-Elgato_Systems_Stream_Deck_A1-00.mono-fallback"
        ))


class _MemoryDevice(WaveDevice):
    def __init__(self, profile):
        self.profile = profile
        self._lock = threading.RLock()
        self._card = None
        self._last_fw = None
        self.config = bytearray(profile.config_len)
        self.phantom_read = threading.Event()
        self.release_phantom = threading.Event()
        self.lowz_read = threading.Event()
        self.pause_phantom = False

    def read_config(self):
        if threading.current_thread().name == "phantom" and self.pause_phantom:
            self.phantom_read.set()
            self.release_phantom.wait(timeout=2)
        elif threading.current_thread().name == "lowz":
            self.lowz_read.set()
        return bytearray(self.config)

    def write_config(self, config):
        self.config[:] = config


class PhantomPowerTests(unittest.TestCase):
    def test_state_is_exposed_only_by_powered_xlr_profiles(self):
        xlr = _MemoryDevice(WAVE_XLR)
        xlr.config[WAVE_XLR.off_phantom] = 1
        self.assertTrue(xlr.get_all()["phantom"])

        wave3 = _MemoryDevice(WAVE3)
        wave3.set_phantom(True)
        self.assertNotIn("phantom", wave3.get_all())

    def test_concurrent_config_updates_cannot_overwrite_each_other(self):
        device = _MemoryDevice(WAVE_XLR)
        device.pause_phantom = True
        phantom = threading.Thread(
            name="phantom", target=lambda: device.set_phantom(True)
        )
        lowz = threading.Thread(
            name="lowz", target=lambda: device.set_low_impedance(True)
        )

        phantom.start()
        self.assertTrue(device.phantom_read.wait(timeout=1))
        lowz.start()
        self.assertFalse(device.lowz_read.wait(timeout=0.1))
        device.release_phantom.set()
        phantom.join(timeout=1)
        lowz.join(timeout=1)

        self.assertFalse(phantom.is_alive())
        self.assertFalse(lowz.is_alive())
        self.assertEqual(device.config[WAVE_XLR.off_phantom], 1)
        self.assertEqual(device.config[WAVE_XLR.off_low_z], 1)


if __name__ == "__main__":
    unittest.main()

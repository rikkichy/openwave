import unittest

from wavexlr.mixer import _is_wave_card
from wavexlr.profiles import PROFILES, WAVE_XLR, WAVE_XLR_MK2
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


if __name__ == "__main__":
    unittest.main()

"""Keepalive discovery and aggregation across multiple Wave devices.

The old single-pin manager had two multi-device failures pinned here: its
source match caught only "Elgato_Wave_" (the XLR Dock enumerates as
"Elgato_XLR_Dock_" and silently got no keepalive at all), and one healthy
device could hide another's wedge.
"""

import unittest
from unittest import mock

from wavexlr import audio


def _node(name):
    return {"type": "PipeWire:Interface:Node",
            "info": {"props": {"node.name": name}}}


class Discovery(unittest.TestCase):
    def test_finds_every_wave_family_node(self):
        dump = [
            _node("alsa_input.usb-Elgato_Systems_Elgato_Wave_XLR_ABC-00.mono"),
            _node("alsa_input.usb-Elgato_Systems_Elgato_XLR_Dock_DEF-00.mono"),
            _node("alsa_input.usb-Elgato_Systems_Elgato_Wave_3_GHI-00.mono"),
        ]
        with mock.patch.object(audio, "_pw_dump", return_value=dump):
            names = audio._get_source_node_names()
        self.assertEqual(len(names), 3)
        self.assertTrue(any("XLR_Dock" in n for n in names),
                        "the Dock must be pinned too")

    def test_other_hardware_is_left_alone(self):
        dump = [
            _node("alsa_input.usb-Elgato_Systems_Game_Capture_HD60-00.mono"),
            _node("alsa_input.usb-SteelSeries_Arctis_Nova-00.mono"),
            _node("alsa_output.usb-Elgato_Systems_Elgato_Wave_XLR_A-00.st"),
        ]
        with mock.patch.object(audio, "_pw_dump", return_value=dump):
            self.assertEqual(audio._get_source_node_names(), [])

    def test_duplicates_collapse(self):
        n = _node("alsa_input.usb-Elgato_Systems_Elgato_Wave_XLR_A-00.mono")
        with mock.patch.object(audio, "_pw_dump", return_value=[n, n]):
            self.assertEqual(len(audio._get_source_node_names()), 1)


class Aggregation(unittest.TestCase):
    def test_no_pins_is_absent(self):
        self.assertEqual(audio._aggregate([]), (False, False, "absent"))

    def test_all_ok_is_healthy(self):
        self.assertEqual(audio._aggregate(["ok", "ok"]), (True, True, "ok"))

    def test_one_wedged_device_cannot_hide_behind_a_healthy_one(self):
        self.assertEqual(audio._aggregate(["ok", "wedged"]),
                         (True, False, "wedged"))

    def test_wedged_outranks_silent(self):
        self.assertEqual(audio._aggregate(["silent", "wedged"]),
                         (True, False, "wedged"))

    def test_silent_alone_reports_silent(self):
        self.assertEqual(audio._aggregate(["ok", "silent"]),
                         (True, False, "silent"))


if __name__ == "__main__":
    unittest.main()

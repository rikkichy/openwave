import os
import tempfile
import unittest
from unittest import mock

from wavexlr import device


DOCK_CONTROLS = """\
numid=3,iface=MIXER,name='PCM Playback Switch'
  ; type=BOOLEAN,access=rw------,values=1
  : values=on
numid=4,iface=MIXER,name='PCM Playback Volume'
  ; type=INTEGER,access=rw---R--,values=1,min=0,max=120,step=0
  : values=73
numid=5,iface=MIXER,name='Mic Capture Switch'
  ; type=BOOLEAN,access=rw------,values=1
  : values=on
numid=6,iface=MIXER,name='Mic Capture Volume'
  ; type=INTEGER,access=rw---R--,values=1,min=0,max=150,step=0
  : values=150
numid=2,iface=PCM,name='Capture Channel Map'
  ; type=INTEGER,access=r--v-R--,values=1,min=0,max=36,step=0
  : values=2
"""

SHUFFLED_CONTROLS = """\
numid=11,iface=MIXER,name='Wave XLR Mk3 Capture Switch'
  ; type=BOOLEAN,access=rw------,values=1
  : values=on
numid=12,iface=MIXER,name='Wave XLR Mk3 Capture Volume'
  ; type=INTEGER,access=rw---R--,values=1,min=0,max=200,step=0
  : values=0
numid=13,iface=MIXER,name='Wave XLR Mk3 Playback Volume'
  ; type=INTEGER,access=rw---R--,values=1,min=0,max=99,step=0
  : values=0
"""


class AlsaControlTests(unittest.TestCase):
    def setUp(self):
        device._ALSA_NUMIDS.clear()
        device._ALSA_CTL_MAX.clear()
        self.addCleanup(device._ALSA_NUMIDS.clear)
        self.addCleanup(device._ALSA_CTL_MAX.clear)

    def test_roles_and_ranges_follow_driver_names(self):
        calls = []

        def fake_amixer(card, *args):
            calls.append((card, args))
            return SHUFFLED_CONTROLS if args == ("contents",) else ""

        with mock.patch.object(device, "_amixer", fake_amixer):
            self.assertEqual(device._numid("3", "mute"), 11)
            self.assertEqual(device._numid("3", "gain"), 12)
            self.assertEqual(device._numid("3", "hp_vol"), 13)
            self.assertEqual(device._alsa_ctl_max("3", 12, 150), 200)
            self.assertEqual(device._alsa_ctl_max("3", 13, 120), 99)

        self.assertEqual(calls, [("3", ("contents",))])

    def test_unreadable_controls_fall_back_without_inventing_state(self):
        with mock.patch.object(device, "_amixer", return_value=None):
            self.assertEqual(device._numid("3", "mute"), 5)
            self.assertEqual(device._numid("3", "gain"), 6)
            self.assertEqual(device._numid("3", "hp_vol"), 4)
            self.assertEqual(device._alsa_get("3"), {})

    def test_gain_is_not_truncated_before_the_real_driver_clamp(self):
        calls = []

        def fake_amixer(card, *args):
            if args == ("contents",):
                return DOCK_CONTROLS
            calls.append(args)
            return ""

        with mock.patch.object(device, "_amixer", fake_amixer):
            value = device._fw_gain_to_alsa(75 * 256, 256)
            device._alsa_set_gain("3", value)

        self.assertEqual(value, 150)
        self.assertEqual(calls, [("cset", "numid=6", "150")])


class CardIdentityTests(unittest.TestCase):
    def _card_tree(self, cards):
        root = tempfile.TemporaryDirectory()
        self.addCleanup(root.cleanup)
        paths = []
        for number, (usb_id, usb_bus) in cards.items():
            card_dir = os.path.join(root.name, f"card{number}")
            os.makedirs(card_dir)
            with open(os.path.join(card_dir, "usbid"), "w") as file:
                file.write(usb_id + "\n")
            if usb_bus is not None:
                with open(os.path.join(card_dir, "usbbus"), "w") as file:
                    file.write(usb_bus + "\n")
            paths.append(os.path.join(card_dir, "usbid"))
        return sorted(paths)

    def test_vid_pid_and_bus_select_exact_cards(self):
        paths = self._card_tree({
            3: ("0fd9:00a6", "011/007"),
            4: ("0fd9:007d", "001/036"),
            5: ("0fd9:00a6", "002/004"),
        })
        with mock.patch.object(device.glob, "glob", return_value=paths):
            self.assertEqual(
                device._find_card(("Elgato",), vid=0x0FD9, pid=0x007D), "4"
            )
            self.assertEqual(
                device._find_card(
                    ("Elgato",), vid=0x0FD9, pid=0x00A6,
                    usbbus="002/004",
                ),
                "5",
            )
            self.assertIsNone(
                device._find_card(("Elgato",), vid=0x0FD9, pid=0x0070)
            )

    def test_missing_bus_identity_never_guesses_a_duplicate(self):
        paths = self._card_tree({3: ("0fd9:00a6", None)})
        with mock.patch.object(device.glob, "glob", return_value=paths):
            with mock.patch.object(device.subprocess, "run") as run:
                self.assertIsNone(device._find_card(
                    ("Elgato",), vid=0x0FD9, pid=0x00A6,
                    usbbus="011/007",
                ))
        run.assert_not_called()


if __name__ == "__main__":
    unittest.main()

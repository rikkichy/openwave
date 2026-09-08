"""Enumerating every supported Wave, including two of the same model.

connect() with no arguments opens the first device of a vid:pid, which
made a second identical unit invisible; scan() walks the bus and reports
each one with its (bus, addr) so callers can open them individually.
"""

import unittest
from types import SimpleNamespace
from unittest import mock

from wavexlr import device
from wavexlr.profiles import PROFILES, WAVE3, WAVE_XLR_MK2


def _fake_bus(entries):
    """A _each_usb_device that visits the given (vid, pid, bus, addr)."""
    def each(visit):
        for vid, pid, bus, addr in entries:
            visit(vid, pid, bus, addr, object())
    return each


class Scan(unittest.TestCase):
    def test_two_identical_models_are_two_results(self):
        bus = _fake_bus([
            (WAVE_XLR_MK2.vid, WAVE_XLR_MK2.pid, 1, 5),
            (WAVE_XLR_MK2.vid, WAVE_XLR_MK2.pid, 3, 2),
        ])
        with mock.patch.object(device, "_each_usb_device", bus):
            found = device.scan()
        self.assertEqual(len(found), 2)
        self.assertEqual({(b, a) for _p, b, a in found}, {(1, 5), (3, 2)})

    def test_unsupported_hardware_is_ignored(self):
        bus = _fake_bus([
            (0x046D, 0x0825, 1, 4),          # some webcam
            (WAVE3.vid, 0x9999, 1, 6),        # right vendor, unknown product
            (WAVE3.vid, WAVE3.pid, 2, 3),
        ])
        with mock.patch.object(device, "_each_usb_device", bus):
            found = device.scan()
        self.assertEqual([(p.key, b, a) for p, b, a in found],
                         [("wave3", 2, 3)])

    def test_results_come_in_bus_order(self):
        entries = [(p.vid, p.pid, bus, addr)
                   for (p, bus, addr) in zip(PROFILES, (9, 1, 5), (9, 1, 5))]
        with mock.patch.object(device, "_each_usb_device", _fake_bus(entries)):
            found = device.scan()
        self.assertEqual([(b, a) for _p, b, a in found],
                         [(1, 1), (5, 5), (9, 9)])


class ClosedHandle(unittest.TestCase):
    """A transfer after disconnect must be an error, never a crash.

    get_all() releases the device lock between transfers, and unplug
    handling can disconnect in that gap. libusb does not NULL-check its
    handle argument, so before the guard this was a segfault that took the
    whole app down the moment a device was unplugged mid-poll.
    """

    def test_read_on_a_cleared_handle_raises(self):
        dev = device.WaveDevice()
        dev.profile = WAVE_XLR_MK2
        with self.assertRaisesRegex(RuntimeError, "[Dd]isconnected"):
            dev._ctrl_read(0x0000, 34)

    def test_write_on_a_cleared_handle_raises(self):
        dev = device.WaveDevice()
        dev.profile = WAVE_XLR_MK2
        with self.assertRaisesRegex(RuntimeError, "[Dd]isconnected"):
            dev._ctrl_write(0x0000, b"\x00" * 34)


class DeviceReadiness(unittest.TestCase):
    def test_missing_alsa_closes_opened_handle(self):
        dev = device.WaveDevice()
        with mock.patch.object(dev, "_open_at", return_value=42), \
             mock.patch.object(device, "_find_card", return_value=None), \
             mock.patch.object(device._lib, "libusb_close") as close:
            with self.assertRaises(device.DeviceNotReadyError):
                dev.connect(WAVE_XLR_MK2, 1, 5)
        self.assertFalse(dev.connected)
        close.assert_called_once_with(42)

    def test_absent_bus_identity_does_not_use_product_name_fallback(self):
        with mock.patch.object(device.glob, "glob", return_value=[]), \
             mock.patch.object(device.subprocess, "run") as run:
            self.assertIsNone(device._find_card(
                ("Elgato",), vid=0x0FD9, pid=0x00A6, usbbus="001/005"
            ))
        run.assert_not_called()

    def test_short_transfer_is_not_decoded_as_valid_configuration(self):
        dev = device.WaveDevice()
        dev.profile = WAVE_XLR_MK2
        dev._handle = 42
        with mock.patch.object(device._lib, "libusb_control_transfer", return_value=2):
            with self.assertRaises(RuntimeError):
                dev.read_config()


class CaptureIdentity(unittest.TestCase):
    def _unit(self, serial, card="3", profile=WAVE_XLR_MK2):
        return SimpleNamespace(
            info={"serial": serial}, alsa_card=card, profile=profile, connected=True,
        )

    def test_reused_alsa_card_cannot_target_replacement_unit(self):
        replacement = self._unit("UNIT_B")
        stale_capture = {"alsa_card": "3", "serial": "UNIT_A"}
        self.assertIsNone(device.device_for_capture(stale_capture, [replacement]))

    def test_missing_capture_identity_leaves_pipewire_in_control(self):
        unit = self._unit("UNIT_A")
        self.assertIsNone(device.device_for_capture({"alsa_card": "3"}, [unit]))

    def test_missing_hardware_identity_cannot_be_supplied_by_card(self):
        unit = self._unit("")
        capture = {"alsa_card": "3", "serial": "UNIT_A"}
        self.assertIsNone(device.device_for_capture(capture, [unit]))

    def test_exact_serial_follows_unit_not_recycled_card(self):
        original = self._unit("UNIT_A", card="4")
        replacement = self._unit("UNIT_B", card="3")
        capture = {"alsa_card": "3", "serial": "UNIT_A"}
        self.assertIs(device.device_for_capture(capture, [replacement, original]), original)

    def test_complete_udev_identity_maps_same_unit(self):
        unit = self._unit("A8A9A40411NOP9")
        capture = {"serial": "Elgato_Systems_Elgato_XLR_Dock_A8A9A40411NOP9"}
        self.assertIs(device.device_for_capture(capture, [unit]), unit)

    def test_model_identity_cannot_be_replaced_by_serial_suffix(self):
        unit = self._unit("UNIT_A", profile=WAVE3)
        capture = {"alsa_card": "3", "serial": "Elgato_Systems_Elgato_XLR_Dock_UNIT_A"}
        self.assertIsNone(device.device_for_capture(capture, [unit]))

    def test_serial_substring_is_not_physical_identity(self):
        unit = self._unit("UNIT_A")
        capture = {"alsa_card": "3", "serial": "Elgato_Systems_Elgato_XLR_Dock_UNIT_AB"}
        self.assertIsNone(device.device_for_capture(capture, [unit]))

    def test_duplicate_identity_cannot_be_disambiguated_by_card(self):
        first = self._unit("UNIT_A", card="3")
        second = self._unit("UNIT_A", card="4")
        capture = {"alsa_card": "3", "serial": "Elgato_Systems_Elgato_XLR_Dock_UNIT_A"}
        self.assertIsNone(device.device_for_capture(capture, [first, second]))

    def test_disconnected_unit_is_not_a_hardware_target(self):
        unit = self._unit("UNIT_A")
        unit.connected = False
        self.assertIsNone(device.device_for_capture({"serial": "UNIT_A"}, [unit]))


if __name__ == "__main__":
    unittest.main()

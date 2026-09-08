import copy
import json
from pathlib import Path
import tempfile
import unittest

from wavexlr import scenes
from wavexlr.mixer import Mixer


class SceneStoreTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.path = Path(directory.name) / "scenes.json"

    def test_corrupt_storage_survives_read_and_mutation_attempts(self):
        payloads = (
            b'{"scenes":',
            b'{"scenes": []}',
            b'{"scenes": {"bad": null}}',
            b'{"scenes": {"bad": {"name": 42}}}',
            b'{"scenes": {"bad": {"name": "Bad", "sources": []}}}',
            b'{"scenes": {"bad": {"name": "Bad", "cells": {"mic.main": []}}}}',
            b'{"scenes": {"bad": {"name": "Bad", "outputs": {"main": 1}}}}',
            b'{"scenes": {"bad": {"name": "Bad", "sources": {"mic": {"muted": "false"}}}}}',
            b'\xff',
        )
        for payload in payloads:
            with self.subTest(payload=payload):
                self.path.write_bytes(payload)
                for operation in (
                    lambda: scenes.load(self.path),
                    lambda: scenes.put("New", {}, self.path),
                    lambda: scenes.remove("bad", self.path),
                ):
                    with self.assertRaises(scenes.Unreadable):
                        operation()
                    self.assertEqual(self.path.read_bytes(), payload)
                    self.assertEqual(set(self.path.parent.iterdir()), {self.path})

    def test_explicit_empty_maps_survive_round_trip_and_removal(self):
        self.assertEqual(scenes.load(self.path), {})
        self.assertFalse(self.path.exists())
        scenes.save({}, self.path)
        self.assertEqual(scenes.load(self.path), {})
        payload = {section: {} for section in (
            "sources", "cells", "outputs", "volumes", "hardware",
        )}
        sid = scenes.put("Quiet desk", payload, self.path)
        self.assertEqual(scenes.load(self.path), {
            sid: {"name": "Quiet desk", **payload},
        })
        self.assertTrue(scenes.remove(sid, self.path))
        self.assertFalse(scenes.remove(sid, self.path))
        self.assertEqual(scenes.load(self.path), {})

    def test_named_levels_replace_without_defining_sources_or_mixes(self):
        payload = {
            "sources": {"mic": {"level": 0.75, "muted": False}},
            "cells": {"mic.personal": {"volume": 0.5, "muted": True}},
            "outputs": {"personal": "alsa_output.headphones", "stream": "none"},
            "volumes": {"personal": {"volume": 1, "muted": False}},
            "hardware": {"wave_xlr:abc": {
                "gain_raw": 10240, "mute": False, "hp_volume_db": -12.5,
                "low_impedance": True, "monitor_mix": 12800,
            }},
        }
        sid = scenes.put("On Air!", payload, self.path)
        self.assertEqual(sid, "on-air")
        self.assertEqual(scenes.load(self.path)[sid], {"name": "On Air!", **payload})
        self.assertNotIn("name", payload)
        scenes.put("On Air!", {"sources": {}}, self.path)
        self.assertEqual(scenes.load(self.path), {
            sid: {"name": "On Air!", "sources": {}},
        })
        with self.assertRaises(ValueError):
            scenes.put("Definitions", {"mixes": {"new": {}}}, self.path)
        self.assertNotIn("definitions", scenes.load(self.path))

    def test_invalid_numbers_are_rejected_without_losing_valid_scenes(self):
        invalid_entries = (
            ("sources", "mic", {"level": True}),
            ("sources", "mic", {"level": "0.5"}),
            ("cells", "mic.main", {"volume": float("nan")}),
            ("volumes", "main", {"volume": float("inf")}),
            ("sources", "mic", {"level": -0.01}),
            ("cells", "mic.main", {"volume": 1.01}),
            ("hardware", "wave_xlr:abc", {"gain_raw": 1.5}),
            ("hardware", "wave_xlr:abc", {"gain_raw": 65536}),
            ("hardware", "wave_xlr:abc", {"monitor_mix": -1}),
            ("hardware", "wave_xlr:abc", {"hp_volume_db": -129}),
        )
        scenes.put("Keep", {}, self.path)
        original = self.path.read_bytes()
        for section, key, entry in invalid_entries:
            with self.subTest(section=section, entry=entry):
                payload = {section: {key: entry}}
                with self.assertRaises(ValueError):
                    scenes.put("Invalid", payload, self.path)
                self.assertEqual(self.path.read_bytes(), original)
                bad_path = self.path.parent / "invalid.json"
                bad_bytes = json.dumps({"scenes": {
                    "invalid": {"name": "Invalid", **payload},
                }}).encode()
                bad_path.write_bytes(bad_bytes)
                with self.assertRaises(scenes.Unreadable):
                    scenes.load(bad_path)
                self.assertEqual(bad_path.read_bytes(), bad_bytes)

    def test_phantom_power_is_not_scene_state(self):
        payload = {"hardware": {"wave_xlr:abc": {"phantom": False}}}
        original = copy.deepcopy(payload)
        with self.assertRaises(ValueError):
            scenes.put("Power", payload, self.path)
        self.assertEqual(payload, original)
        self.assertFalse(self.path.exists())


class SceneRecallTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.path = Path(directory.name) / "matrix.json"
        self.mixer = Mixer(config_path=str(self.path))
        self.addCleanup(self.mixer.stop)
        self.mixer.set_mixes({"bus": {"name": "Broadcast", "sink": "openwave_bus"}})
        self.mixer.set_sources({
            "primary": {"group": "voice", "level": 0.7},
            "backup": {"group": "voice", "muted": True},
        })

    def test_partial_recall_preserves_topology_and_group_exclusion(self):
        skipped = self.mixer.apply_scene({
            "sources": {"backup": {"muted": False, "level": 0.4}, "missing": {"level": 1}},
            "cells": {"backup.bus": {"volume": 0.6}, "missing.bus": {"volume": 1}},
            "outputs": {"bus": "unplugged_headphones"},
            "volumes": {"bus": {"volume": 0.3, "muted": True}},
        })
        state = self.mixer.scene_state()
        self.assertEqual(set(state["sources"]), {"primary", "backup"})
        self.assertTrue(state["sources"]["primary"]["muted"])
        self.assertEqual(state["sources"]["backup"], {"level": 0.4, "muted": False})
        self.assertEqual(state["cells"]["backup.bus"], {"volume": 0.6, "muted": False})
        self.assertEqual(set(state["cells"]), {"primary.bus", "backup.bus"})
        self.assertEqual(state["volumes"]["bus"], {"volume": 0.3, "muted": True})
        self.assertIsNone(self.mixer.resolve_output("bus", sinks=[]))
        self.assertEqual(self.mixer.get_output("bus"), "unplugged_headphones")
        self.assertIn("missing", " ".join(skipped))

    def test_invalid_recall_cannot_apply_an_earlier_valid_section(self):
        before = self.mixer.scene_state()
        persisted = self.path.read_bytes()
        with self.assertRaises(ValueError):
            self.mixer.apply_scene({
                "sources": {"primary": {"level": 0}},
                "volumes": {"bus": {"volume": float("nan")}},
            })
        self.assertEqual(self.mixer.scene_state(), before)
        self.assertEqual(self.path.read_bytes(), persisted)


class HardwareEntryTests(unittest.TestCase):
    def test_exact_serial_wins_over_legacy_even_with_duplicate_models(self):
        hardware = {
            "dock": {"gain_raw": 100},
            scenes.hardware_key("dock", "one"): {"gain_raw": 200},
            scenes.hardware_key("dock", "two"): {},
        }
        self.assertEqual(scenes.pick_hardware_entry(
            hardware, "dock", "one", profile_count=2,
        ), {"gain_raw": 200})
        self.assertEqual(scenes.pick_hardware_entry(
            hardware, "dock", "two", profile_count=2,
        ), {})

    def test_replacement_unit_never_borrows_another_serials_levels(self):
        hardware = {"dock:old": {"gain_raw": 500}}
        self.assertIsNone(scenes.pick_hardware_entry(hardware, "dock", "new"))
        self.assertIsNone(scenes.pick_hardware_entry(hardware, "dock", ""))
        self.assertIsNone(scenes.pick_hardware_entry(hardware, "dock", None))

    def test_legacy_model_requires_exactly_one_connected_unit(self):
        hardware = {scenes.hardware_key("dock", ""): {"gain_raw": 250}}
        for serial in ("one", "", None):
            with self.subTest(serial=serial):
                self.assertEqual(scenes.pick_hardware_entry(
                    hardware, "dock", serial,
                ), {"gain_raw": 250})
                self.assertIsNone(scenes.pick_hardware_entry(
                    hardware, "dock", serial, profile_count=2,
                ))
                self.assertIsNone(scenes.pick_hardware_entry(
                    hardware, "dock", serial, profile_count=0,
                ))
        self.assertIsNone(scenes.pick_hardware_entry(hardware, "wave3", "one"))


if __name__ == "__main__":
    unittest.main()

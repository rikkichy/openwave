import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from wavexlr import mixes, setup, sources, uninstall


class StoreTests(unittest.TestCase):
    def test_empty_mix_store_is_not_reseeded_and_corruption_is_preserved(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "mixdefs.json"
            with patch.object(mixes, "CONFIG_PATH", str(path)):
                seeded = mixes.load_seeded()
                self.assertEqual({item["sink"] for item in seeded.values()},
                                 {"openwave_personal_mix", "openwave_chat_mix", "openwave_record_mix"})
                mixes.save({})
                self.assertEqual(mixes.load_seeded(), {})
                path.write_text("{broken")
                with self.assertRaises(mixes.Unreadable):
                    mixes.load_seeded()
                self.assertEqual(path.read_text(), "{broken")

    def test_source_legacy_binding_migrates_without_changing_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "sources.json"
            path.write_text(json.dumps({"music": {"id": "music", "name": "My Music", "match_app_name": "Player"}}))
            with patch.object(sources, "CONFIG_PATH", str(path)):
                records = sources.load()
                self.assertEqual(sources.bindings(records["music"]), ["Player"])
                sources.update(records, "music", match_app_names=["Player", "Music"])
                reloaded = sources.load()
                self.assertEqual(reloaded["music"]["id"], "music")
                self.assertEqual(sources.bindings(reloaded["music"]), ["Player", "Music"])
                self.assertNotIn("match_app_name", json.loads(path.read_text())["music"])

    def test_unsafe_identity_and_duplicate_sinks_are_rejected(self):
        with self.assertRaises(ValueError):
            sources.normalize({"bad.id": {"name": "Bad"}})
        with self.assertRaises(ValueError):
            sources.normalize({"safe": {"level": float("nan")}})
        with self.assertRaises(ValueError):
            mixes.normalize({"a": {"sink": "openwave_shared"}, "b": {"sink": "openwave_shared"}})

    def test_joined_group_is_exclusive_after_reload(self):
        with tempfile.TemporaryDirectory() as directory:
            with patch.object(sources, "CONFIG_PATH", str(Path(directory) / "sources.json")):
                records = sources.normalize({"voice": {"group": "Live"}})
                sources.add(records, {"id": "backup", "group": "Live"})
                reloaded = sources.load()
                self.assertFalse(reloaded["voice"]["muted"])
                self.assertTrue(reloaded["backup"]["muted"])
                sources.update(reloaded, "voice", muted=True)
                sources.update(reloaded, "backup", muted=False)
                recalled = sources.load()
                self.assertTrue(recalled["voice"]["muted"])
                self.assertFalse(recalled["backup"]["muted"])

    def test_mix_rename_preserves_external_sink_and_cell_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            with patch.object(mixes, "CONFIG_PATH", str(Path(directory) / "mixdefs.json")):
                record = mixes.new_mix(name="Broadcast")
                records = {record["id"]: record}
                original_id, original_sink = record["id"], record["sink"]
                mixes.update(records, original_id, name="New name", description="OpenWave New name")
                reloaded = mixes.load()
                self.assertEqual(reloaded[original_id]["sink"], original_sink)
                self.assertEqual(reloaded[original_id]["name"], "New name")
                with self.assertRaises(ValueError):
                    mixes.update(records, original_id, sink="openwave_different")

    def test_generated_config_roundtrips_untrusted_description(self):
        description = 'A "quoted" mix\\path\nnext\tline\r\u2603 } ] context.objects = ['
        rendered = setup.render_mixes_conf({"chat": {"name": "Chat", "description": description}})
        encoded = rendered.split("node.description = ", 1)[1]
        decoded, _ = json.JSONDecoder().raw_decode(encoded)
        self.assertEqual(decoded, description)

    def test_corrupt_definitions_do_not_replace_installed_config(self):
        with tempfile.TemporaryDirectory() as directory:
            definitions = Path(directory) / "mixdefs.json"
            installed = Path(directory) / "mixes.conf"
            definitions.write_text("not-json")
            installed.write_text("existing configuration")
            with patch.object(mixes, "CONFIG_PATH", str(definitions)):
                with self.assertRaises(mixes.Unreadable):
                    setup.install_mixes(path=str(installed))
            self.assertEqual(installed.read_text(), "existing configuration")

    def test_sandbox_rejects_native_setup_but_allows_mix_config(self):
        with tempfile.TemporaryDirectory() as directory, patch.dict(os.environ, {"FLATPAK_ID": "io.github.rikkichy.OpenWave"}):
            with patch.object(setup.service, "install", side_effect=AssertionError("Host service touched")):
                self.assertTrue(setup.is_sandboxed())
                self.assertFalse(setup.run_setup()[0])
                self.assertFalse(uninstall.execute(uninstall.inspect()).success)
                with self.assertRaises(RuntimeError):
                    setup.install_service()
                destination = Path(directory) / "mixes.conf"
                setup.install_mixes({}, path=str(destination))
                self.assertEqual(destination.read_text(), setup.render_mixes_conf({}))

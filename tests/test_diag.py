"""Opt-in diagnostics must not open USB or expose private details by default."""

import builtins
import contextlib
import io
import json
import os
import tempfile
import types
import unittest
from unittest import mock

from wavexlr import daemon, diag


class Privacy(unittest.TestCase):
    def test_paths_are_redacted_in_both_modes_including_errors(self):
        secret_paths = [
            os.path.expanduser("~/private/project"),
            "/opt/private/python",
            "/srv/private/workspace",
            "/tmp/private folder/audio.wav",
        ]
        def broken():
            raise OSError("failed at '/srv/private/workspace'")
        for full in (False, True):
            with self.subTest(full=full):
                text = diag.assemble(full=full, sections=(
                    ("Paths", lambda: " ".join(repr(path) for path in secret_paths)),
                    ("Broken", broken),
                    ("Good", lambda: "survived"),
                ))
                for path in secret_paths:
                    self.assertNotIn(path, text)
                self.assertNotIn("private folder", text)
                self.assertIn("survived", text)
                self.assertIn("https://github.com/rikkichy/openwave", text)

    def test_default_graph_withholds_serial_and_description(self):
        node = {"info": {"state": "running", "props": {
            "node.name": "alsa_input.usb-Elgato_XLR_Dock_PRIVATE_SERIAL-00.mono-fallback",
            "node.description": "PRIVATE_DESCRIPTION",
        }}}
        with mock.patch.object(diag, "_run", return_value=json.dumps([node])):
            text = diag.collect_pipewire()
            self.assertNotIn("PRIVATE_SERIAL", text)
            self.assertNotIn("PRIVATE_DESCRIPTION", text)
            self.assertIn("running", text)
            self.assertIn("PRIVATE_SERIAL", diag.collect_pipewire(full=True))

    def test_full_does_not_authorize_usb_reads(self):
        with mock.patch.object(diag, "collect_device", return_value="device read") as device:
            sections = (("Device", device),)
            for full in (False, True):
                diag.assemble(full=full, sections=sections)
            device.assert_not_called()
            self.assertIn("device read", diag.assemble(device=True, sections=sections))

    def test_device_details_do_not_read_config_or_reveal_serial_by_default(self):
        dev = types.SimpleNamespace(
            profile=types.SimpleNamespace(display_name="Dock", vid=0x0fd9, pid=0x00ab, config_len=4),
            usbbus="001/002", _card=2,
            read_device_info=mock.Mock(return_value={"fw_version": "1", "api_version": "2", "serial": "SECRET_SERIAL"}),
            read_config=mock.Mock(return_value=b"ABCD"),
        )
        text = "\n".join(diag.describe_device(dev))
        self.assertNotIn("SECRET_SERIAL", text)
        self.assertIn("0fd9:00ab", text)
        dev.read_config.assert_not_called()
        text = "\n".join(diag.describe_device(dev, full=True))
        self.assertIn("SECRET_SERIAL", text)
        self.assertIn("41 42 43 44", text)

    def test_default_config_summary_never_includes_app_names(self):
        with tempfile.TemporaryDirectory() as directory:
            path = os.path.join(directory, "sources.json")
            with open(path, "w") as stream:
                json.dump({"private": "SECRET_APP_NAME"}, stream)
            with mock.patch.object(diag.os.path, "expanduser", return_value=path):
                self.assertNotIn("SECRET_APP_NAME", diag.collect_configs())
                self.assertIn("SECRET_APP_NAME", diag.collect_configs(full=True))

    def test_journal_is_not_collected_without_full(self):
        with mock.patch.object(diag, "_run") as run:
            diag.collect_journal()
            run.assert_not_called()

    def test_export_is_private_and_never_overwrites_existing_file(self):
        with tempfile.TemporaryDirectory() as directory:
            path = os.path.join(directory, "report.txt")
            with mock.patch.object(diag, "assemble", return_value="safe report"), contextlib.redirect_stdout(io.StringIO()):
                diag.main(["-o", path])
                self.assertEqual(os.stat(path).st_mode & 0o777, 0o600)
                with self.assertRaises(FileExistsError):
                    diag.main(["-o", path])
            with open(path) as stream:
                self.assertEqual(stream.read(), "safe report")


class HeadlessCli(unittest.TestCase):
    def test_help_and_version_precede_audio_usb_and_gi_imports(self):
        real_import = builtins.__import__
        def guarded(name, *args, **kwargs):
            if name in ("audio", "device", "health", "gi") or name.startswith(("gi.", "wavexlr.audio", "wavexlr.device")):
                raise AssertionError(f"premature import: {name}")
            return real_import(name, *args, **kwargs)
        with mock.patch("builtins.__import__", side_effect=guarded):
            for main in (daemon.main, diag.main):
                for flag in ("--help", "--version"):
                    with self.subTest(main=main.__module__, flag=flag), contextlib.redirect_stdout(io.StringIO()), self.assertRaises(SystemExit) as exit:
                        main([flag])
                    self.assertEqual(exit.exception.code, 0)


if __name__ == "__main__":
    unittest.main()

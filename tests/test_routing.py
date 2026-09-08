import copy
import subprocess
import tempfile
import threading
import time
import unittest
from pathlib import Path

from wavexlr.mixer import Mixer, claim_streams, SubprocessPipeWire


class Child:
    def __init__(self, graph, names):
        self.graph, self.names, self.dead = graph, names, False

    def poll(self):
        return 0 if self.dead else None

    def terminate(self):
        self.dead = True
        for name in self.names:
            self.graph["nodes"].pop(name, None)

    def kill(self):
        self.terminate()

    def wait(self, timeout=None):
        return 0


class PipeWire:
    def __init__(self):
        self.graph = {"nodes": {}, "ports": {}, "links": set(), "sinks": {}, "streams": {},
                      "captures": [], "capture_mutes": {}, "modules": [], "default": "headphones"}
        self.next_id = 1
        self.children = []
        self.levels = {}
        self.fail_links = 0
        self.fail_moves = 0
        self.fail_levels = 0
        self.operations = []
        self.sink("headphones")
        for mid in ("personal", "chat", "record"):
            self.sink("openwave_" + mid + "_mix")

    def node(self, name):
        ident = self.next_id
        self.next_id += 3
        self.graph["nodes"][name] = {"id": ident, "serial": str(ident), "props": {}}
        for direction, offset in (("in", 1), ("out", 2)):
            self.graph["ports"][(str(ident), direction)] = [{"id": ident + offset, "channel": "MONO"}]
        return ident

    def sink(self, name, volume=1.0):
        ident = self.node(name)
        self.graph["sinks"][name] = {"name": name, "index": ident, "identity": str(ident), "volume": volume,
                                     "muted": False, "description": name, "priority": 0}

    def snapshot(self):
        return copy.deepcopy(self.graph)

    def create_sink(self, name, description):
        self.sink(name)
        handle = (self.next_id, name)
        self.graph["modules"].append({"index": handle[0], "argument": handle[1]})
        return handle

    def destroy_sink(self, handle, graph):
        self.graph["sinks"].pop(handle[1], None)
        return True

    def spawn_loopback(self, name, **kwargs):
        self.node(name)
        self.node(name + "_cap")
        child = Child(self.graph, (name, name + "_cap"))
        self.children.append(child)
        return child

    def link(self, source, target):
        self.operations.append(("link", threading.current_thread().name))
        if self.fail_links:
            self.fail_links -= 1
            return False
        self.graph["links"].add((source, target))
        return True

    def set_level(self, node_id, volume, muted):
        if self.fail_levels:
            self.fail_levels -= 1
            return False
        self.levels[node_id] = (volume, muted)
        for sink in self.graph["sinks"].values():
            if sink["index"] == node_id:
                sink.update(volume=volume, muted=muted)
        return True

    def move_stream(self, stream, sink):
        if self.fail_moves:
            self.fail_moves -= 1
            return False
        self.graph["streams"][stream["id"]]["sink"] = sink
        return True

    def stream(self):
        ident = self.node("Music")
        self.graph["streams"][ident] = {"id": ident, "serial": str(ident), "pulse_id": ident,
                                       "sink": "headphones", "app_name": "Music", "node_name": "Music"}
        return ident


def eventually(predicate):
    deadline = time.monotonic() + 3
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.005)
    raise AssertionError("Routing did not converge")


class RoutingTests(unittest.TestCase):
    def test_claims_are_exclusive_and_order_independent(self):
        streams = {7: {"app_name": " Music ", "binary": "/usr/bin/player"}}
        rows = {"z": {"match_app_names": ["Music"]}, "a": {"match_app_names": ["player"]},
                "b": {"match_app_names": ["music"]}}
        expected = {"z": set(), "a": set(), "b": {7}}
        self.assertEqual(claim_streams(rows, streams), expected)
        self.assertEqual(claim_streams(dict(reversed(list(rows.items()))), streams), expected)

    def test_failed_edges_and_moves_retry_without_double_routing(self):
        pw = PipeWire()
        sid = pw.stream()
        pw.fail_moves, pw.fail_links, pw.fail_levels = 1, 2, 1
        with tempfile.TemporaryDirectory() as directory:
            mixer = Mixer(pw, config_path=str(Path(directory) / "mixes.json"), poll_interval=0.005)
            mixer.set_sources({"music": {"name": "Music", "match_app_names": ["Music"], "level": 0.5}})
            mixer.set_cell("music", "chat", 0.6, False)
            mixer.start()
            try:
                eventually(lambda: pw.graph["streams"][sid]["sink"] == "openwave_src_music" and len(pw.graph["links"]) >= 2)
                self.assertIn((0.3, False), pw.levels.values())
                self.assertTrue(all(thread == "openwave-mixer" for _, thread in pw.operations))
                mixer.set_cell("music", "chat", 0, False)
                eventually(lambda: all(child.dead for child in pw.children))
                self.assertEqual(pw.graph["streams"][sid]["sink"], "openwave_src_music")
            finally:
                mixer.stop()
            self.assertEqual(pw.graph["streams"][sid]["sink"], "headphones")
            self.assertTrue(all(child.dead for child in pw.children))

    def test_queries_do_not_discover_or_leak_mutable_state(self):
        class NoDiscovery(PipeWire):
            def snapshot(self):
                raise AssertionError("Discovery before start")
        with tempfile.TemporaryDirectory() as directory:
            mixer = Mixer(NoDiscovery(), config_path=str(Path(directory) / "mixes.json"))
            try:
                mixer.set_cell("music", "chat", 0.5, False)
                cell = mixer.get_cell("music", "chat")
                cell["volume"] = 0
                self.assertEqual(mixer.get_cell("music", "chat")["volume"], 0.5)
                self.assertEqual(mixer.streams(), {})
                mixer.poll_streams()
            finally:
                mixer.stop()

    def test_nonzero_commands_are_not_success(self):
        from unittest.mock import patch
        pw = SubprocessPipeWire()
        with patch("wavexlr.mixer.subprocess.run", return_value=subprocess.CompletedProcess([], 1, "", "rejected")):
            self.assertFalse(pw.link(1, 2))
            self.assertFalse(pw.move_stream({"pulse_id": 4}, "sink"))
            self.assertFalse(pw.set_level(4, 0.2, False))

    @staticmethod
    def reconcile(mixer, pw):
        graph = pw.snapshot()
        mixer._discover(graph)
        mixer._reconcile(graph)

    def test_failed_restore_retains_intake_until_original_destination_recovers(self):
        pw = PipeWire()
        pw.sink("speakers")
        stream = pw.stream()
        pw.graph["streams"][stream]["sink"] = "speakers"
        with tempfile.TemporaryDirectory() as directory:
            mixer = Mixer(pw, config_path=str(Path(directory) / "mixes.json"))
            try:
                mixer.set_sources({"music": {"name": "Music", "match_app_names": ["Music"]}})
                for _ in range(3):
                    self.reconcile(mixer, pw)
                self.assertEqual(pw.graph["streams"][stream]["sink"], "openwave_src_music")

                mixer.remove_source("music")
                pw.fail_moves = 1
                self.reconcile(mixer, pw)
                self.assertEqual(pw.graph["streams"][stream]["sink"], "openwave_src_music")
                self.assertIn("openwave_src_music", pw.graph["sinks"])

                self.reconcile(mixer, pw)
                self.assertEqual(pw.graph["streams"][stream]["sink"], "speakers")
                self.assertNotIn("openwave_src_music", pw.graph["sinks"])
            finally:
                mixer._teardown()
                mixer.stop()

    def test_teardown_retries_restore_and_retains_unresolved_intake(self):
        for persistent_failure in (False, True):
            with self.subTest(persistent_failure=persistent_failure), tempfile.TemporaryDirectory() as directory:
                pw = PipeWire()
                pw.sink("speakers")
                stream = pw.stream()
                pw.graph["streams"][stream]["sink"] = "speakers"
                mixer = Mixer(pw, config_path=str(Path(directory) / "mixes.json"))
                try:
                    mixer.set_sources({"music": {"name": "Music", "match_app_names": ["Music"]}})
                    for _ in range(3):
                        self.reconcile(mixer, pw)
                    self.assertEqual(pw.graph["streams"][stream]["sink"], "openwave_src_music")

                    pw.fail_moves = 100 if persistent_failure else 1
                    mixer._teardown()
                    if persistent_failure:
                        self.assertEqual(pw.graph["streams"][stream]["sink"], "openwave_src_music")
                        self.assertIn("openwave_src_music", pw.graph["sinks"])
                    else:
                        self.assertEqual(pw.graph["streams"][stream]["sink"], "speakers")
                        self.assertNotIn("openwave_src_music", pw.graph["sinks"])
                finally:
                    pw.fail_moves = 0
                    mixer._teardown()
                    mixer.stop()

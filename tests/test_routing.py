import copy
import json
import subprocess
import tempfile
import threading
import time
import unittest
from pathlib import Path

from wavexlr.mixer import Mixer, claim_streams, SubprocessPipeWire, mix_capture_name


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

    def remove_mix_sink(self, name, graph):
        self.graph["sinks"].pop(name, None)
        self.graph["nodes"].pop(name, None)
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

    def has_route(self, source, target, level=None):
        nodes, edges = self.graph["nodes"], self.graph["links"]
        if source not in nodes or target not in nodes:
            return False
        source_port, target_port = nodes[source]["id"] + 2, nodes[target]["id"] + 1
        for child in self.children:
            if child.dead or any(name not in nodes for name in child.names):
                continue
            playback, capture = (nodes[name]["id"] for name in child.names)
            if (source_port, capture + 1) in edges and (playback + 2, target_port) in edges:
                if level is None or self.levels.get(playback) == level:
                    return True
        return False


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
                eventually(lambda: pw.graph["streams"][sid]["sink"] == "openwave_src_music"
                           and pw.has_route("openwave_src_music", "openwave_chat_mix", (0.3, False)))
                self.assertTrue(all(thread == "openwave-mixer" for _, thread in pw.operations))
                mixer.set_cell("music", "chat", 0, False)
                eventually(lambda: not pw.has_route("openwave_src_music", "openwave_chat_mix"))
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

    def test_recreated_master_restores_before_observing_unity(self):
        pw = PipeWire()
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "mixes.json"
            path.write_text(json.dumps({"volumes": {"chat": {"volume": 0.25, "muted": True}}}))
            mixer = Mixer(pw, config_path=str(path), poll_interval=0.005)
            mixer.start()
            try:
                eventually(lambda: mixer.volumes_restored)
                self.assertEqual(pw.graph["sinks"]["openwave_chat_mix"]["volume"], 0.25)
                pw.fail_levels = 3
                pw.sink("openwave_chat_mix", 1.0)
                eventually(lambda: pw.graph["sinks"]["openwave_chat_mix"]["volume"] == 0.25)
                self.assertEqual(mixer.mix_volume("chat"), (0.25, True))
            finally:
                mixer.stop()
            self.assertEqual(json.loads(path.read_text())["volumes"]["chat"], {"volume": 0.25, "muted": True})

    def test_dynamic_outputs_publish_capture_and_stop_owned_children(self):
        pw = PipeWire()
        with tempfile.TemporaryDirectory() as directory:
            mixer = Mixer(pw, config_path=str(Path(directory) / "mixes.json"),
                          mixes_config_path=str(Path(directory) / "mixes.conf"), poll_interval=0.005)
            mixer.set_mixes({"broadcast": {"name": "Broadcast", "sink": "openwave_mix_broadcast"}})
            mixer.set_output("broadcast", "headphones")
            mixer.start()
            try:
                eventually(lambda: pw.has_route("openwave_mix_broadcast", "headphones")
                           and mix_capture_name("broadcast") in pw.graph["nodes"])
                self.assertEqual(mixer.resolve_output("broadcast"), "headphones")
                mixer.set_output("broadcast", "unplugged")
                eventually(lambda: not pw.has_route("openwave_mix_broadcast", "headphones"))
                self.assertIsNone(mixer.resolve_output("broadcast"))
                mixer.remove_mix("broadcast")
                eventually(lambda: mix_capture_name("broadcast") not in pw.graph["nodes"]
                           and "openwave_mix_broadcast" not in pw.graph["sinks"])
            finally:
                mixer.stop()
            self.assertTrue(all(child.dead for child in pw.children))
            self.assertIn("headphones", pw.graph["sinks"])

    def test_reused_module_id_does_not_authorize_deletion(self):
        from unittest.mock import patch
        pw = SubprocessPipeWire()
        graph = {"modules": [{"index": 8, "argument": "openwave.owner=someone-else"}]}
        with patch("wavexlr.mixer.subprocess.run", side_effect=AssertionError("Unrelated resource touched")):
            self.assertTrue(pw.destroy_sink((8, "our-token"), graph))

    def test_duplicate_stream_names_remain_independently_claimable(self):
        objects = [{"id": index, "type": "PipeWire:Interface:Node",
                    "info": {"props": {"node.name": "Player", "object.serial": index + 100,
                                       "media.class": "Stream/Output/Audio", "application.name": "Player"}}}
                   for index in (11, 12)]

        class DuplicateStreams(SubprocessPipeWire):
            def run(self, argv):
                if argv == ["pw-dump"]:
                    return json.dumps(objects)
                if argv == ["pactl", "get-default-sink"]:
                    return "headphones"
                if argv[-1] == "sink-inputs":
                    return json.dumps([{"index": index, "properties": {"object.serial": index + 100}}
                                       for index in (11, 12)])
                return "[]"

        graph = DuplicateStreams().snapshot()
        self.assertEqual(claim_streams({"player": {"match_app_names": ["Player"]}}, graph["streams"]),
                         {"player": {11, 12}})

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

    def test_successful_master_change_preserves_publication_and_route_identity(self):
        pw = PipeWire()
        pw.stream()
        with tempfile.TemporaryDirectory() as directory:
            mixer = Mixer(pw, config_path=str(Path(directory) / "mixes.json"))
            try:
                mixer.set_sources({"music": {"name": "Music", "match_app_names": ["Music"]}})
                mixer.set_cell("music", "personal", 0.6, False)
                for _ in range(3):
                    self.reconcile(mixer, pw)
                self.assertTrue(pw.has_route("openwave_src_music", "openwave_personal_mix"))
                self.assertTrue(pw.has_route("openwave_personal_mix", "headphones"))
                self.assertIn(mix_capture_name("personal"), pw.graph["nodes"])
                identities = {name: node["serial"] for name, node in pw.graph["nodes"].items()}

                mixer.set_mix_volume("personal", 0.4, True)
                for _ in range(2):
                    self.reconcile(mixer, pw)
                    self.assertEqual({name: node["serial"] for name, node in pw.graph["nodes"].items()},
                                     identities)
                    self.assertTrue(pw.has_route("openwave_src_music", "openwave_personal_mix"))
                    self.assertTrue(pw.has_route("openwave_personal_mix", "headphones"))
                self.assertEqual(pw.graph["sinks"]["openwave_personal_mix"]["volume"], 0.4)
                self.assertTrue(pw.graph["sinks"]["openwave_personal_mix"]["muted"])
            finally:
                mixer._teardown()
                mixer.stop()

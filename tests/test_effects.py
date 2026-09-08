import copy
import json
import math
import unittest

from wavexlr.effects import DEFAULT_FX, fx, fx_active, fx_node_name, render_fx_config


def source(**settings):
    return {"id": "mic-1", "name": "Microphone", "node_name": "alsa_input.mic",
            "fx": settings}


def graph_args(record):
    config = json.loads(render_fx_config(record))
    return next(module["args"] for module in config["context.modules"]
                if module["name"] == "libpipewire-module-filter-chain")


class EffectsTests(unittest.TestCase):
    def test_hostile_metadata_cannot_add_spa_properties(self):
        hostile = 'Mic"\n} ] context.modules = [{ name = "evil" }] #\\'
        record = source(gate=True)
        record.update(name=hostile, node_name=hostile)
        config = json.loads(render_fx_config(record))
        self.assertNotIn("evil", [module["name"] for module in config["context.modules"]])
        args = graph_args(record)
        self.assertEqual(args["capture.props"]["target.object"], hostile)
        self.assertEqual(args["node.description"], "OpenWave FX: " + hostile)
        renamed = dict(record, name="New label")
        other = graph_args(renamed)
        for props in ("capture.props", "playback.props"):
            self.assertEqual(args[props]["media.name"], other[props]["media.name"])
            self.assertEqual(args[props]["node.name"], other[props]["node.name"])

    def test_identifier_rejects_path_and_config_injection(self):
        for value in ("../mic", "mic/child", "a\nb", 'a"', "", None, "a" * 129):
            with self.subTest(value=value), self.assertRaises(ValueError):
                fx_node_name(value)

    def test_invalid_schema_cannot_reach_pipewire(self):
        for settings in ({"eq_low": math.nan}, {"delay_ms": math.inf},
                         {"gate_thresh": -math.inf}, {"comp_ratio": 10 ** 1000},
                         {"mono": "false"}, {"gate_thresh": True},
                         {"delay_ms": "20"}, {"lowcut": 90}, {"unknown": 1}):
            with self.subTest(settings=settings), self.assertRaises(ValueError):
                render_fx_config(source(**settings))
        for value in (None, [], "gate"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                fx({"fx": value})

    def test_finite_values_clamp_to_supported_plugin_ranges(self):
        record = source(gate=True, gate_thresh=-1000, comp=True,
                        comp_thresh=-1000, comp_ratio=1000, eq_low=-1000,
                        eq_mid=1000, delay_ms=1000)
        args = graph_args(record)
        nodes = {node["name"]: node for node in args["filter.graph"]["nodes"]}
        self.assertEqual(nodes["gate_0"]["control"]["Threshold (dB)"], -70)
        self.assertEqual(nodes["comp_0"]["control"]["Threshold level (dB)"], -30)
        self.assertEqual(nodes["comp_0"]["control"]["Ratio (1:n)"], 10)
        self.assertEqual(nodes["eq_low_0"]["control"]["Gain"], -12)
        self.assertEqual(nodes["eq_mid_0"]["control"]["Gain"], 12)
        self.assertEqual(nodes["delay_0"]["control"]["Delay (s)"], 0.5)

    def test_neutral_identity_has_no_filter_process(self):
        for settings in ({}, DEFAULT_FX, {"gate_thresh": -25, "comp_thresh": -5},
                         {"comp": True, "comp_ratio": 1}, {"delay_ms": -10}):
            record = source(**settings)
            original = copy.deepcopy(record)
            self.assertFalse(fx_active(record))
            self.assertIsNone(render_fx_config(record))
            self.assertEqual(record, original)

    def test_stereo_strips_never_cross_or_lose_a_channel(self):
        args = graph_args(source(lowcut=80, gate=True, comp=True, eq_low=1,
                                 eq_mid=2, eq_high=3, delay_ms=10))
        graph = args["filter.graph"]
        nodes = {node["name"]: node for node in graph["nodes"]}
        links = {link["output"]: link["input"] for link in graph["links"]}
        self.assertEqual(args["capture.props"]["audio.position"], ["FL", "FR"])
        self.assertEqual(args["playback.props"]["audio.position"], ["FL", "FR"])
        self.assertEqual(len(graph["inputs"]), 2)
        self.assertEqual(len(graph["outputs"]), 2)
        visited = []
        for start, end in zip(graph["inputs"], graph["outputs"]):
            path, port = [], start
            while True:
                name, input_port = port.split(":")
                node = nodes[name]
                path.append(name)
                ladspa = node["type"] == "ladspa"
                self.assertEqual(input_port, "Input" if ladspa else "In")
                output = name + (":Output" if ladspa else ":Out")
                if output == end:
                    break
                port = links[output]
            self.assertEqual([nodes[name]["label"] for name in path],
                             ["bq_highpass", "gate", "sc4m", "bq_lowshelf",
                              "bq_peaking", "bq_highshelf", "delay"])
            visited.append(set(path))
        self.assertFalse(visited[0] & visited[1])
        gate = next(node for node in graph["nodes"] if node["label"] == "gate")
        self.assertIn("Output select (-1 = key listen, 0 = gate, 1 = bypass)", gate["control"])

    def test_explicit_downmix_averages_both_inputs_before_processing(self):
        args = graph_args(source(mono=True, comp=True))
        graph = args["filter.graph"]
        downmix = next(node for node in graph["nodes"] if node["label"] == "mixer")
        self.assertEqual(graph["inputs"], [downmix["name"] + ":In 1", downmix["name"] + ":In 2"])
        self.assertEqual(downmix["control"], {"Gain 1": 0.5, "Gain 2": 0.5})
        self.assertEqual(len(graph["outputs"]), 1)
        self.assertIn({"output": downmix["name"] + ":Out", "input": "comp_0:Input"}, graph["links"])
        self.assertEqual(args["capture.props"]["audio.channels"], 2)
        self.assertEqual(args["playback.props"]["audio.position"], ["MONO"])

    def test_known_mono_input_needs_no_lossy_downmix(self):
        record = dict(source(eq_high=2), channels=1)
        args = graph_args(record)
        self.assertEqual(args["capture.props"]["audio.position"], ["MONO"])
        self.assertEqual(args["playback.props"]["audio.position"], ["MONO"])
        self.assertEqual(len(args["filter.graph"]["inputs"]), 1)
        self.assertFalse(any(node["label"] == "mixer" for node in args["filter.graph"]["nodes"]))
        with self.assertRaises(ValueError):
            render_fx_config(dict(record, channels=6))


if __name__ == "__main__":
    unittest.main()

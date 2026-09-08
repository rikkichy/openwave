"""Validated per-source DSP settings and pure PipeWire configuration rendering.

The mixer owns processes; this module never changes the graph or source store.
A neutral strip renders no configuration, so bypass is the original raw source.
"""

import json
import math
import re
from collections.abc import Mapping


DEFAULT_FX = {
    "lowcut": 0,
    "gate": False,
    "gate_thresh": -50.0,
    "comp": False,
    "comp_thresh": -18.0,
    "comp_ratio": 3.0,
    "eq_low": 0.0,
    "eq_mid": 0.0,
    "eq_high": 0.0,
    "delay_ms": 0,
    "mono": False,
}

# SC4 mono's documented threshold minimum is -30 dB, not -40 dB.
FX_RANGES = {
    "gate_thresh": (-70.0, -20.0),
    "comp_thresh": (-30.0, 0.0),
    "comp_ratio": (1.0, 10.0),
    "eq_low": (-12.0, 12.0),
    "eq_mid": (-12.0, 12.0),
    "eq_high": (-12.0, 12.0),
    "delay_ms": (0.0, 500.0),
}
FX_NODE_PREFIX = "openwave_fx_"


def fx(source):
    """Return canonical settings; reject malformed values, clamp finite ranges.

    Unknown fields are rejected rather than silently accepting misspelled controls.
    This is the shared schema used by persistence callers, UI, and the renderer.
    """
    if source is None:
        source = {}
    if not isinstance(source, Mapping):
        raise ValueError("source must be a mapping")
    stored = source.get("fx", {})
    if not isinstance(stored, Mapping):
        raise ValueError("fx must be a mapping")
    if stored.keys() - DEFAULT_FX.keys():
        raise ValueError("unknown effect setting")
    result = {**DEFAULT_FX, **stored}
    for key in ("gate", "comp", "mono"):
        if not isinstance(result[key], bool):
            raise ValueError(f"{key} must be a boolean")
    for key in ("lowcut", *FX_RANGES):
        value = result[key]
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            raise ValueError(f"{key} must be a finite number")
        try:
            value = float(value)
        except OverflowError as exc:
            raise ValueError(f"{key} must be a finite number") from exc
        if not math.isfinite(value):
            raise ValueError(f"{key} must be a finite number")
        if key == "lowcut":
            if value not in (0, 80, 120):
                raise ValueError("lowcut must be 0, 80, or 120 Hz")
            result[key] = int(value)
        else:
            low, high = FX_RANGES[key]
            result[key] = max(low, min(high, value))
    return result


def _active(settings):
    return bool(
        settings["lowcut"] or settings["gate"]
        or (settings["comp"] and settings["comp_ratio"] > 1)
        or settings["eq_low"] or settings["eq_mid"] or settings["eq_high"]
        or settings["delay_ms"] or settings["mono"]
    )


def fx_active(source):
    """Whether the validated strip changes audio (disabled thresholds do not)."""
    return _active(fx(source))


def fx_node_name(source_id):
    """Stable virtual-source identity, also safe for use as a config basename."""
    if not isinstance(source_id, str) or not re.fullmatch(r"[A-Za-z0-9_-]{1,128}", source_id):
        raise ValueError("invalid source identifier")
    return FX_NODE_PREFIX + source_id


def render_fx_config(source, *, owner=None):
    """Return a SPA-safe JSON PipeWire config, or None for exact raw bypass.

    Unknown inputs are stereo. Set source['channels']=1 only for a known mono
    device. Stereo uses independent strips, including gate/compressor envelopes;
    only an explicit mono toggle averages the two inputs into one output.
    """
    if not isinstance(source, Mapping):
        raise ValueError("source must be a mapping")
    settings = fx(source)
    node_name = fx_node_name(source.get("id"))
    channels = source.get("channels", 2)
    if type(channels) is not int or channels not in (1, 2):
        raise ValueError("source channels must be 1 or 2")
    if not _active(settings):
        return None
    target = source.get("node_name")
    if (not isinstance(target, str) or not target or "\0" in target
            or target in ("0", "-1") or target.startswith(FX_NODE_PREFIX)):
        raise ValueError("an effect chain requires a raw node_name")
    label = source.get("name", source["id"])
    if not isinstance(label, str) or "\0" in label:
        raise ValueError("source name must be a string without NUL")

    nodes, links, inputs, outputs = [], [], [], []
    downmix = settings["mono"] and channels == 2
    output_channels = 1 if settings["mono"] else channels
    if downmix:
        nodes.append({"type": "builtin", "name": "downmix", "label": "mixer",
                      "control": {"Gain 1": 0.5, "Gain 2": 0.5}})
        inputs.extend(("downmix:In 1", "downmix:In 2"))

    for channel in range(output_channels):
        strip = []

        def add(name, label, control=None, plugin=None, config=None):
            name = f"{name}_{channel}"
            node = {"type": "ladspa" if plugin else "builtin", "name": name,
                    "label": label}
            if control:
                node["control"] = control
            if plugin:
                node["plugin"] = plugin
            if config:
                node["config"] = config
            nodes.append(node)
            strip.append((name + (":Input" if plugin else ":In"),
                          name + (":Output" if plugin else ":Out")))

        if settings["lowcut"]:
            add("hp", "bq_highpass", {"Freq": settings["lowcut"], "Q": 0.70710678})
        if settings["gate"]:
            add("gate", "gate", {
                "Threshold (dB)": settings["gate_thresh"],
                "Attack (ms)": 10.0, "Hold (ms)": 120.0, "Decay (ms)": 150.0,
                "Range (dB)": -70.0, "LF key filter (Hz)": 40.0,
                "HF key filter (Hz)": 20000.0,
                "Output select (-1 = key listen, 0 = gate, 1 = bypass)": 0.0,
            }, plugin="gate_1410")
        if settings["comp"] and settings["comp_ratio"] > 1:
            add("comp", "sc4m", {
                "Threshold level (dB)": settings["comp_thresh"],
                "Ratio (1:n)": settings["comp_ratio"], "RMS/peak": 0.0,
                "Attack time (ms)": 15.0, "Release time (ms)": 150.0,
                "Knee radius (dB)": 3.0, "Makeup gain (dB)": 0.0,
            }, plugin="sc4m_1916")
        for key, label_name, freq in (("eq_low", "bq_lowshelf", 100.0),
                                      ("eq_mid", "bq_peaking", 1000.0),
                                      ("eq_high", "bq_highshelf", 8000.0)):
            if settings[key]:
                add(key, label_name, {"Freq": freq, "Gain": settings[key], "Q": 0.70710678})
        if settings["delay_ms"]:
            add("delay", "delay", {"Delay (s)": settings["delay_ms"] / 1000.0},
                config={"max-delay": 1.0})
        if not strip:
            add("thru", "copy")
        links.extend({"output": a[1], "input": b[0]}
                     for a, b in zip(strip, strip[1:]))
        if downmix:
            links.append({"output": "downmix:Out", "input": strip[0][0]})
        else:
            inputs.append(strip[0][0])
        outputs.append(strip[-1][1])

    description = "OpenWave FX: " + label
    capture_position = ["MONO"] if channels == 1 else ["FL", "FR"]
    output_position = ["MONO"] if output_channels == 1 else ["FL", "FR"]
    args = {
        "node.description": description,
        "media.name": node_name,
        "audio.rate": 48000,
        "filter.graph": {"nodes": nodes, "links": links, "inputs": inputs, "outputs": outputs},
        "capture.props": {
            "node.name": node_name + "_cap", "media.name": node_name + "_cap",
            "target.object": target, "node.passive": True,
            "node.dont-reconnect": True, "application.name": "OpenWave",
            "node.dont-fallback": True, "node.dont-move": True,
            "node.description": description + " (capture)",
            "audio.channels": channels, "audio.position": capture_position,
        },
        "playback.props": {
            "node.name": node_name, "media.name": node_name,
            "media.class": "Audio/Source", "application.name": "OpenWave",
            "node.description": description,
            "audio.channels": output_channels, "audio.position": output_position,
        },
    }
    if owner is not None:
        if not isinstance(owner, str) or not owner or "\0" in owner:
            raise ValueError("invalid effect owner")
        args["capture.props"]["openwave.owner"] = owner
        args["playback.props"]["openwave.owner"] = owner
    # JSON is valid SPA syntax; serialize every string, including all metadata.
    return json.dumps({
        "context.properties": {"log.level": 2},
        "context.spa-libs": {"audio.convert.*": "audioconvert/libspa-audioconvert",
                             "support.*": "support/libspa-support"},
        "context.modules": [
            {"name": "libpipewire-module-rt", "args": {"nice.level": -11},
             "flags": ["ifexists", "nofail"]},
            {"name": "libpipewire-module-protocol-native"},
            {"name": "libpipewire-module-client-node"},
            {"name": "libpipewire-module-adapter"},
            {"name": "libpipewire-module-filter-chain", "args": args},
        ],
    }, allow_nan=False, indent=2) + "\n"

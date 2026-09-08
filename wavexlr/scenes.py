"""Named level snapshots, not source or mix definitions.

Scenes recall trims, sends, mutes, outputs, masters and optional hardware
levels without creating or deleting matrix rows or columns. Phantom power
is deliberately excluded: changing microphone power requires an explicit
hardware action, never a scene recall.

The XDG config store contains {"scenes": {id: scene}}. Missing storage is an
empty store; unreadable storage is left untouched and reported to the caller.
"""

import json
import math
import os
import re
import tempfile

CONFIG_PATH = os.path.join(
    os.environ.get("XDG_CONFIG_HOME") or os.path.expanduser("~/.config"),
    "openwave", "scenes.json",
)


class Unreadable(Exception):
    """The scene store exists but cannot be read as valid scene data."""


def _mapping(value, label):
    if not isinstance(value, dict) or any(
        not isinstance(key, str) or not key for key in value
    ):
        raise ValueError(f"{label} must be a map with nonempty string keys")


def _number(value, low, high, label, *, integer=False):
    if (type(value) not in (int, float)
            or (integer and type(value) is not int)
            or not low <= value <= high
            or not math.isfinite(value)):
        raise ValueError(f"{label} must be a finite number in [{low}, {high}]")


def _validate(scenes):
    _mapping(scenes, "scenes")
    sections = {"sources", "cells", "outputs", "volumes", "hardware"}
    hardware_fields = {
        "gain_raw", "mute", "hp_volume_db", "low_impedance", "monitor_mix",
    }
    for sid, scene in scenes.items():
        _mapping(scene, f"scene {sid}")
        if set(scene) - sections - {"name"}:
            raise ValueError(f"scene {sid} has unsupported fields")
        if not isinstance(scene.get("name"), str):
            raise ValueError(f"scene {sid} needs a string name")
        for section in sections & scene.keys():
            entries = scene[section]
            _mapping(entries, f"scene {sid} {section}")
            for key, entry in entries.items():
                label = f"scene {sid} {section} {key}"
                if section == "outputs":
                    if entry is not None and not isinstance(entry, str):
                        raise ValueError(f"{label} must be a sink name or null")
                    continue
                _mapping(entry, label)
                if section == "hardware":
                    if set(entry) - hardware_fields:
                        raise ValueError(f"{label} has unsupported hardware fields")
                    for field in ("mute", "low_impedance"):
                        if field in entry and type(entry[field]) is not bool:
                            raise ValueError(f"{label} {field} must be boolean")
                    for field in ("gain_raw", "monitor_mix"):
                        if field in entry:
                            _number(entry[field], 0, 0xFFFF, f"{label} {field}",
                                    integer=True)
                    if "hp_volume_db" in entry:
                        _number(entry["hp_volume_db"], -128, 0,
                                f"{label} hp_volume_db")
                    continue
                if section == "cells":
                    source_id, separator, mix_id = key.rpartition(".")
                    if not separator or not source_id or not mix_id:
                        raise ValueError(f"{label} needs a source.mix key")
                level_field = "level" if section == "sources" else "volume"
                if set(entry) - {level_field, "muted"}:
                    raise ValueError(f"{label} has unsupported level fields")
                if level_field in entry:
                    _number(entry[level_field], 0, 1, f"{label} {level_field}")
                if "muted" in entry and type(entry["muted"]) is not bool:
                    raise ValueError(f"{label} muted must be boolean")


def load(path=None):
    """Return every scene, or {} on first run; never modify unreadable data."""
    path = CONFIG_PATH if path is None else path
    try:
        with open(path, encoding="utf-8") as file:
            data = json.load(file)
        if not isinstance(data, dict) or set(data) != {"scenes"}:
            raise ValueError("top-level shape must be {'scenes': {...}}")
        _validate(data["scenes"])
        return data["scenes"]
    except FileNotFoundError:
        return {}
    except (OSError, ValueError) as exc:
        raise Unreadable(f"Cannot read scenes from {path}: {exc}") from exc


def save(scenes, path=None):
    """Validate and atomically replace the store; invalid payloads raise ValueError."""
    _validate(scenes)
    path = os.fspath(CONFIG_PATH if path is None else path)
    directory = os.path.dirname(path) or "."
    os.makedirs(directory, exist_ok=True)
    fd, tmp = tempfile.mkstemp(prefix=".scenes-", suffix=".tmp", dir=directory)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as file:
            json.dump({"scenes": scenes}, file, indent=2, allow_nan=False)
        os.replace(tmp, path)
    finally:
        if os.path.exists(tmp):
            os.unlink(tmp)


def scene_id(name):
    """A stable id from a human name: lowercase, dashes, nothing else."""
    slug = re.sub(r"[^a-z0-9]+", "-", name.lower()).strip("-")
    return slug or "scene"


def hardware_key(profile_key, serial):
    """Address a unit by serial, or use a legacy model key if serial is absent."""
    return f"{profile_key}:{serial}" if serial else profile_key


def pick_hardware_entry(hardware, profile_key, serial, *, profile_count=1):
    """Find this unit's levels without borrowing another unit's serial entry.

    A nonempty exact serial wins, even among duplicate models. A legacy bare
    model key is safe only with exactly one connected unit of that model;
    callers must count units with missing serials too. Ambiguous serial-less
    units never share one hardware snapshot.
    """
    if not hardware:
        return None
    if serial:
        exact_key = hardware_key(profile_key, serial)
        if exact_key in hardware:
            return hardware[exact_key]
    if profile_count == 1:
        return hardware.get(profile_key)
    return None


def put(name, payload, path=None):
    """Store a scene under its name's id, replacing an existing one."""
    if not isinstance(name, str):
        raise ValueError("scene name must be a string")
    scenes = load(path)
    _mapping(payload, "scene payload")
    sid = scene_id(name)
    scenes[sid] = dict(payload, name=name)
    save(scenes, path)
    return sid


def remove(sid, path=None):
    """Remove a stored scene, returning whether it existed."""
    scenes = load(path)
    if sid in scenes:
        del scenes[sid]
        save(scenes, path)
        return True
    return False

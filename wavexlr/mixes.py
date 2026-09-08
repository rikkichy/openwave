"""Mix definitions. Missing stores seed defaults; empty and corrupt differ."""

import copy
import json
import os
import re
import uuid

from .sources import Unreadable, _atomic_write, safe_id

CONFIG_PATH = os.path.join(os.environ.get("XDG_CONFIG_HOME", os.path.expanduser("~/.config")), "openwave", "mixdefs.json")
DEFAULT_ICON = "audio-speakers-symbolic"
DEFAULT_MIXES = {
    "personal": {"id": "personal", "name": "Personal Mix", "subtitle": "What you hear", "description": "OpenWave Personal Mix", "sink": "openwave_personal_mix", "icon_name": "audio-headphones-symbolic"},
    "chat": {"id": "chat", "name": "Chat Mix", "subtitle": "Send to voice apps", "description": "OpenWave Chat Mix", "sink": "openwave_chat_mix", "icon_name": "system-users-symbolic"},
    "record": {"id": "record", "name": "Record Mix", "subtitle": "Send to OBS or a recorder", "description": "OpenWave Record Mix", "sink": "openwave_record_mix", "icon_name": "media-record-symbolic"},
}


def normalize(records):
    if not isinstance(records, dict):
        raise ValueError("Mix definitions must be an object")
    result, sinks = {}, set()
    for mid, original in records.items():
        safe_id(mid)
        if not isinstance(original, dict) or original.get("id", mid) != mid:
            raise ValueError(f"Invalid mix record: {mid}")
        mix = copy.deepcopy(original)
        mix["id"] = mid
        mix.setdefault("name", mid)
        mix.setdefault("description", "OpenWave " + mix["name"])
        mix.setdefault("subtitle", "")
        mix.setdefault("icon_name", DEFAULT_ICON)
        mix.setdefault("sink", "openwave_mix_" + mid)
        for field in ("name", "description", "subtitle", "icon_name", "sink"):
            if not isinstance(mix[field], str):
                raise ValueError(f"Mix {field} must be a string")
        sink = mix["sink"]
        if not re.fullmatch(r"openwave_[a-z0-9_]+", sink) or sink.startswith(("openwave_src_", "openwave_loop_", "openwave_capture_")):
            raise ValueError(f"Invalid mix sink name: {sink}")
        if sink in sinks:
            raise ValueError(f"Duplicate mix sink: {sink}")
        sinks.add(sink)
        result[mid] = mix
    return result


def load():
    try:
        with open(CONFIG_PATH) as f:
            return normalize(json.load(f))
    except FileNotFoundError:
        return None
    except (OSError, ValueError, TypeError) as exc:
        raise Unreadable(str(exc)) from exc


def load_seeded():
    records = load()
    if records is None:
        records = copy.deepcopy(DEFAULT_MIXES)
        save(records)
    return records


def save(records):
    _atomic_write(CONFIG_PATH, normalize(records))


def new_mix(*, name, subtitle="", icon_name=DEFAULT_ICON):
    mid = uuid.uuid4().hex[:12]
    return normalize({mid: {"name": name, "subtitle": subtitle, "icon_name": icon_name}})[mid]


def add(records, mix):
    candidate = normalize({**records, mix["id"]: mix})
    save(candidate)
    records.clear()
    records.update(candidate)
    return records


def remove(records, mix_id):
    candidate = {mid: mix for mid, mix in records.items() if mid != mix_id}
    save(candidate)
    records.clear()
    records.update(candidate)
    return records


def update(records, mix_id, **fields):
    for key in ("id", "sink"):
        if key in fields and fields[key] != records[mix_id][key]:
            raise ValueError(f"Mix {key} is immutable")
    return add(records, {**records[mix_id], **fields})

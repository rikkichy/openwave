"""User-defined matrix sources, persisted to ~/.config/openwave/sources.json.

Each source binds to a PipeWire `application.name` so any current or future
audio stream from that application gets mixed through the source's row.
"""

import json
import os
import uuid

CONFIG_PATH = os.path.expanduser("~/.config/openwave/sources.json")


def _atomic_write(path, payload):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    tmp = path + ".tmp"
    with open(tmp, "w") as f:
        json.dump(payload, f, indent=2)
    os.replace(tmp, path)


def load():
    try:
        with open(CONFIG_PATH) as f:
            data = json.load(f)
    except (FileNotFoundError, json.JSONDecodeError):
        return {}
    if not isinstance(data, dict):
        return {}
    return data


def save(sources):
    _atomic_write(CONFIG_PATH, sources)


def new_source(*, name, match_app_name, icon_name="applications-multimedia-symbolic"):
    """Return a fresh source dict ready to insert into the sources mapping."""
    return {
        "id": uuid.uuid4().hex[:12],
        "name": name,
        "match_app_name": match_app_name,
        "icon_name": icon_name,
    }


def add(sources, source):
    sources[source["id"]] = source
    save(sources)
    return sources


def remove(sources, source_id):
    sources.pop(source_id, None)
    save(sources)
    return sources

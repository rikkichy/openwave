"""Validated, stable source identities persisted independently of matrix sends."""

import copy
import json
import math
import os
import re
import tempfile
import uuid

from . import effects

CONFIG_PATH = os.path.join(os.environ.get("XDG_CONFIG_HOME", os.path.expanduser("~/.config")), "openwave", "sources.json")
KIND_APP = "app"
KIND_DEVICE = "device"
DEFAULT_APP_ICON = "applications-multimedia-symbolic"
DEFAULT_DEVICE_ICON = "audio-input-microphone-symbolic"
_ID_RE = re.compile(r"[a-z0-9_]+\Z")


class Unreadable(ValueError):
    """Existing configuration cannot be read safely; it is never overwritten."""


def safe_id(value):
    if not isinstance(value, str) or not _ID_RE.fullmatch(value):
        raise ValueError(f"Invalid stable id: {value!r}")
    return value


def level(value):
    number = float(value)
    if not math.isfinite(number):
        raise ValueError("Level must be finite")
    return max(0.0, min(1.0, number))


def _atomic_write(path, payload):
    directory = os.path.dirname(path) or "."
    os.makedirs(directory, exist_ok=True)
    fd, tmp = tempfile.mkstemp(prefix=".openwave-", dir=directory, text=True)
    try:
        with os.fdopen(fd, "w") as f:
            json.dump(payload, f, indent=2, allow_nan=False)
            f.write("\n")
        os.replace(tmp, path)
    finally:
        if os.path.exists(tmp):
            os.unlink(tmp)


def kind(source):
    return source.get("kind", KIND_APP)


def bindings(source):
    names = source.get("match_app_names")
    if names is None:
        names = [source.get("match_app_name", "")]
    if not isinstance(names, list) or any(not isinstance(n, str) for n in names):
        raise ValueError("Application bindings must be a list of strings")
    return list(dict.fromkeys(n.strip() for n in names if n.strip()))


def parse_bindings(text):
    return list(dict.fromkeys(part.strip() for part in text.split(",") if part.strip()))


def format_bindings(source):
    return ", ".join(bindings(source))


def normalize(records):
    if not isinstance(records, dict):
        raise ValueError("Sources must be an object")
    result = {}
    live_groups = set()
    for sid, original in records.items():
        safe_id(sid)
        if not isinstance(original, dict) or original.get("id", sid) != sid:
            raise ValueError(f"Invalid source record: {sid}")
        source = copy.deepcopy(original)
        source.update(id=sid, kind=kind(source), match_app_names=bindings(source))
        source.pop("match_app_name", None)
        if source["kind"] not in (KIND_APP, KIND_DEVICE):
            raise ValueError(f"Unknown source kind: {source['kind']}")
        source.setdefault("name", sid)
        source.setdefault("icon_name", DEFAULT_DEVICE_ICON if kind(source) == KIND_DEVICE else DEFAULT_APP_ICON)
        source["level"] = level(source.get("level", 1.0))
        source["muted"] = bool(source.get("muted", False))
        source["protected"] = bool(source.get("protected", False))
        source.setdefault("group", "")
        source.setdefault("node_name", "")
        for field in ("name", "icon_name", "group", "node_name"):
            if not isinstance(source[field], str):
                raise ValueError(f"Source {field} must be a string")
        if "fx" in source:
            source["fx"] = effects.fx(source)
        source["group"] = group(source)
        if source["group"] and not source["muted"]:
            if source["group"] in live_groups:
                source["muted"] = True
            else:
                live_groups.add(source["group"])
        result[sid] = source
    return result


def load():
    try:
        with open(CONFIG_PATH) as f:
            return normalize(json.load(f))
    except FileNotFoundError:
        return {}
    except (OSError, ValueError, TypeError) as exc:
        raise Unreadable(str(exc)) from exc


def save(records):
    _atomic_write(CONFIG_PATH, normalize(records))


def new_source(*, name, match_app_name="", match_app_names=None, icon_name=DEFAULT_APP_ICON):
    sid = uuid.uuid4().hex[:12]
    return normalize({sid: {"name": name, "icon_name": icon_name,
                           "match_app_names": match_app_names if match_app_names is not None else parse_bindings(match_app_name)}})[sid]


def new_device_source(*, name, node_name, icon_name=DEFAULT_DEVICE_ICON):
    sid = uuid.uuid4().hex[:12]
    return normalize({sid: {"name": name, "kind": KIND_DEVICE, "node_name": node_name,
                           "icon_name": icon_name}})[sid]


def group(source):
    return source.get("group", "").strip()


def groups(records):
    return sorted({group(source) for source in records.values() if group(source)})


def is_protected(source):
    return bool(source.get("protected", False))


def add(records, source):
    candidate = normalize({**records, source["id"]: source})
    save(candidate)
    records.clear()
    records.update(candidate)
    return records


def remove(records, source_id):
    if is_protected(records.get(source_id, {})):
        raise ValueError("Protected source cannot be removed")
    candidate = {sid: source for sid, source in records.items() if sid != source_id}
    save(candidate)
    records.clear()
    records.update(candidate)
    return records


def update(records, source_id, **fields):
    if "id" in fields and fields["id"] != source_id:
        raise ValueError("Source identity is immutable")
    return add(records, {**records[source_id], **fields, "id": source_id})


def set_order(records, order):
    reordered = {sid: records[sid] for sid in dict.fromkeys([*order, *records]) if sid in records}
    save(reordered)
    return reordered


def reorder(records, source_id, delta):
    order = list(records)
    index = order.index(source_id)
    order.insert(max(0, min(len(order) - 1, index + delta)), order.pop(index))
    return set_order(records, order)
